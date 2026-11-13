//! The rename intent: collision-checked, whole-word replacement with the
//! `cargo check` proof, and the LSP `workspace/applyEdit` fallback for
//! exactly the collision class (roadmap item 34).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::lsp::LspServer;
use crate::policy::CommandPolicy;
/// Applies the rename intent: collision-checked, whole-word, verified.
/// `Err` carries the specific guard that declined (roadmap item 34:
/// per-cause reasons keep the LSP-rename revisit trigger measurable);
/// `Ok(None)` is the unclassified fallback — no site reaches it today.
pub(super) async fn apply_rename(
    workspace: &Path,
    policy: &CommandPolicy,
    from: &str,
    to: &str,
) -> Result<Option<FastPathOutcome>, &'static str> {
    let files = rust_files(workspace);
    let mut touched: Vec<(PathBuf, String)> = Vec::new();
    let mut occurrences = 0;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let count = count_word(&text, from);
        if count == 0 {
            continue;
        }
        if count_word(&text, to) > 0 {
            // The target name already exists in a touched file: a
            // blind replace would shadow it. Decline.
            return Err("intent-rename-collision");
        }
        occurrences += count;
        touched.push((path.clone(), text));
    }
    if occurrences == 0 {
        return Err("intent-rename-zero-occurrences");
    }
    if occurrences > 500 {
        return Err("intent-rename-too-many-occurrences");
    }
    // A pre-existing target name anywhere in the workspace is ambiguity,
    // not just in the touched files.
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if count_word(&text, to) > 0 {
            return Err("intent-rename-collision");
        }
    }
    let snapshots: Vec<Snapshot> = touched
        .iter()
        .filter_map(|(path, _)| {
            std::fs::read(path).ok().map(|bytes| Snapshot {
                path: path.clone(),
                bytes,
            })
        })
        .collect();
    for (path, text) in &touched {
        let (rewritten, _) = replace_word(text, from, to);
        if std::fs::write(path, rewritten).is_err() {
            restore(&snapshots);
            return Err("intent-rename-write-failed");
        }
    }
    if !cargo_ok(workspace, policy, &["check"]).await {
        restore(&snapshots);
        return Err("intent-rename-verify-failed");
    }
    Ok(Some(FastPathOutcome {
        kind: "rename",
        files_changed: u32::try_from(touched.len()).unwrap_or(u32::MAX),
        note: format!(
            "Fast path (rename): `{from}` → `{to}` across {} file(s), \
             {occurrences} occurrence(s); verified with `cargo check`.",
            touched.len()
        ),
    }))
}

use super::verify::cargo_ok;
use super::workspace::{Snapshot, count_word, replace_word, restore, rust_files};

use super::FastPathOutcome;

/// The LSP fallback for exactly the collision class (roadmap item 34):
/// a blind whole-word rename declines when the target name exists
/// anywhere in the workspace, but a semantic rename resolves one
/// declared symbol regardless of same-named strings elsewhere.
/// Requires an unambiguous declaration (exactly one exact
/// workspace-symbol match, workspace-confined), applies through the
/// same confined edit machinery as the `lsp_rename` tool, honors the
/// same 500-edit budget, and verifies with `cargo check`. Any
/// surprise restores the snapshots and returns `None`, so the caller
/// keeps the original collision decline.
pub(super) async fn apply_rename_lsp(
    workspace: &Path,
    policy: &CommandPolicy,
    from: &str,
    to: &str,
    servers: &[Arc<LspServer>],
) -> Option<FastPathOutcome> {
    use lsp_types::request::{Rename, WorkspaceSymbolRequest};
    use lsp_types::{
        OneOf, RenameParams, TextDocumentIdentifier, TextDocumentPositionParams,
        WorkspaceSymbolParams,
    };
    let server = servers.iter().find(|server| {
        matches!(
            &server.capabilities.rename_provider,
            Some(OneOf::Left(true) | OneOf::Right(_))
        )
    })?;
    // Resolve the declaration site: exactly one exact match.
    let symbols = server
        .request::<WorkspaceSymbolRequest>(WorkspaceSymbolParams {
            query: from.to_owned(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        })
        .await
        .ok()??;
    let flat = match symbols {
        lsp_types::WorkspaceSymbolResponse::Flat(infos) => infos,
        // Nested responses carry no guaranteed range; the fallback
        // does not guess positions.
        lsp_types::WorkspaceSymbolResponse::Nested(_) => return None,
    };
    let exact: Vec<_> = flat.iter().filter(|info| info.name == from).collect();
    if exact.len() != 1 {
        return None;
    }
    let location = &exact[0].location;
    // Server-supplied URIs are untrusted: confinement before any use.
    crate::lsp::confined_path(server, &location.uri, "rename").ok()?;
    let edit = server
        .request::<Rename>(RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: location.uri.clone(),
                },
                position: location.range.start,
            },
            new_name: to.to_owned(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        })
        .await
        .ok()??;
    let ops = crate::lsp::edit_op_count(&edit);
    if ops == 0 || ops > 500 {
        return None;
    }
    // Snapshot-guarded like the blind path: capture every Rust file's
    // bytes before the first write, restore on any failure, so the
    // agent loop starts from the untouched workspace.
    let snapshots: Vec<Snapshot> = rust_files(workspace)
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            Some(Snapshot { path, bytes })
        })
        .collect();
    crate::lsp::apply_workspace_edit(server, &edit, None)
        .await
        .ok()?;
    if !cargo_ok(workspace, policy, &["check"]).await {
        restore(&snapshots);
        return None;
    }
    let files_changed = u32::try_from(
        snapshots
            .iter()
            .filter(|snap| std::fs::read(&snap.path).is_ok_and(|now| now != snap.bytes))
            .count(),
    )
    .unwrap_or(u32::MAX);
    Some(FastPathOutcome {
        kind: "rename",
        files_changed,
        note: format!(
            "Fast path (rename via LSP `{}`): `{from}` → `{to}` — {ops} edit(s) \
             across {files_changed} file(s); verified with `cargo check`.",
            server.name
        ),
    })
}
