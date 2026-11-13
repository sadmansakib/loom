//! `WorkspaceEdit` application through loom's own write path (slice E2,
//! docs/lsp-design.md §5): server-computed edits land as ordinary file
//! effects — same review trail, same sandbox, same facts feed — never as
//! a server-initiated `applyEdit` (which loom declines on the wire).
//!
//! Robustness rules that keep application deterministic and confined:
//! - Every server-supplied URI is resolved through the workspace
//!   confinement check before any use; a server cannot direct loom's
//!   write path at files outside the workspace (servers are untrusted).
//! - **Plan before apply**: all semantic validation (confinement,
//!   non-file URIs, version guards, range conversion, overlap
//!   detection) happens in a planning pass that touches no state; only
//!   a fully validated plan reaches the IO pass. The common failure
//!   classes therefore abort before the first byte is written.
//! - `documentChanges` (preferred over the legacy `changes` map) is
//!   applied in array order — servers already order file ops vs edits.
//! - Edits inside one document are applied bottom-up (descending range
//!   start) so earlier offsets stay valid; overlapping ranges are
//!   rejected rather than silently double-applied. Ties at the same
//!   start position apply in **reverse array order** so same-position
//!   inserts end up in array order in the final text (spec test case:
//!   insert "World"@3 then "Hello"@3 yields "`WorldHello`").
//! - Every text range converts from the server's negotiated position
//!   encoding (utf-16 by default) to byte offsets before slicing.
//! - Effect keys are workspace-relative and carry pre-images
//!   (`docs/rewind-design.md` tier 2): the plan pass already reads
//!   every file it is about to modify for its undo snapshots, so the
//!   bytes for a rewind pre-image are in hand before any mutation —
//!   capture rides the same read, never a second one. Bodies go to
//!   the artifact store content-addressed at plan time; a batch that
//!   rolls back leaves the body orphaned (GC-collectable) and no
//!   ledger entry, because effects — pre-images included — are only
//!   recorded after the whole batch commits.

mod text;

use std::borrow::Cow;
use text::{apply_text_edits, plain_edits};

use async_lsp::lsp_types;
use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::{BeforeImage, ToolEffects};

use crate::lsp::client::LspServer;
use crate::pre_image::capture_before_image;

/// One fully validated, not-yet-performed operation. Overwrite and
/// ignore-if-not-exists flags are resolved at plan time (errors there,
/// skips never become ops), so the op carries only what IO needs.
enum PlanOp {
    Create {
        path: std::path::PathBuf,
    },
    Rename {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
    },
    Delete {
        path: std::path::PathBuf,
    },
    Edit {
        path: std::path::PathBuf,
        /// Final text (edits already applied in memory).
        text: String,
    },
}

/// How to reverse one planned op if a later op in the same batch fails.
enum Undo {
    /// Remove a file this batch created.
    RemoveFile(std::path::PathBuf),
    /// Restore original bytes (overwrite, delete, or edit).
    WriteBytes {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    },
    /// Move a rename back, optionally restoring a replaced target.
    RenameBack {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        replaced: Option<Vec<u8>>,
    },
}

impl Undo {
    /// Best effort; errors are collected by the caller so a rollback is
    /// reported honestly rather than silently half-done.
    async fn rollback(self) -> Result<(), String> {
        match self {
            Undo::RemoveFile(path) => tokio::fs::remove_file(&path)
                .await
                .map_err(|e| format!("undo create {}: {e}", path.display())),
            Undo::WriteBytes { path, bytes } => tokio::fs::write(&path, &bytes)
                .await
                .map_err(|e| format!("undo write {}: {e}", path.display())),
            Undo::RenameBack { from, to, replaced } => {
                tokio::fs::rename(&to, &from).await.map_err(|e| {
                    format!("undo rename {} → {}: {e}", to.display(), from.display())
                })?;
                if let Some(bytes) = replaced {
                    tokio::fs::write(&to, &bytes)
                        .await
                        .map_err(|e| format!("undo rename target {}: {e}", to.display()))?;
                }
                Ok(())
            }
        }
    }
}

/// One planned op plus how to reverse it.
struct PlannedOp {
    op: PlanOp,
    undo: Undo,
    /// Tier-2 pre-images for the paths this op touches, keyed by the
    /// same workspace-relative string the effects ledger will record
    /// (ADR-012). Empty when capture was impossible (no artifact
    /// store, or a source file that could not be read): restore then
    /// refuses those paths rather than guessing. Renames carry two —
    /// deleted source first, written target second.
    before_images: Vec<BeforeImage>,
}

/// Resolves a server-supplied URI to a workspace-confined path.
///
/// Servers are untrusted: a `WorkspaceEdit` naming `/etc/passwd` or any
/// other path outside the workspace is rejected here, before any IO —
/// the confinement invariant every other loom write tool obeys.
pub(crate) fn confined_path(
    server: &LspServer,
    uri: &lsp_types::Url,
    op: &str,
) -> Result<std::path::PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|()| format!("{op}: non-file uri {uri}"))?;
    let display = path.display().to_string();
    crate::confinement::resolve_within(server.workspace_root(), &display)
        .map_err(|e| format!("{op}: uri {display} escapes the workspace root: {e}"))
}

/// The workspace-relative string under which a path's effects and
/// pre-images are recorded (ADR-012: relative so a record means the
/// same thing on any machine; `confined_path` guarantees the strip
/// succeeds).
fn effect_key(server: &LspServer, path: &std::path::Path) -> String {
    crate::confinement::workspace_relative(server.workspace_root(), path)
}

/// Builds the validated plan for a whole edit batch: every semantic
/// error (confinement escape, non-file URI, stale version, bad range,
/// overlap) surfaces here, before the first mutation.
async fn plan_workspace_edit(
    server: &LspServer,
    edit: &lsp_types::WorkspaceEdit,
    artifacts: Option<&std::sync::Arc<dyn ArtifactStore>>,
) -> Result<Vec<PlannedOp>, String> {
    // One coercion for every capture site: the helper takes the trait,
    // callers hold an Arc (or nothing, which means captures degrade).
    let store: Option<&dyn ArtifactStore> = artifacts.map(std::sync::Arc::as_ref);
    let operations: Cow<'_, [lsp_types::DocumentChangeOperation]> =
        if let Some(changes) = edit.document_changes.as_ref() {
            match changes {
                lsp_types::DocumentChanges::Operations(ops) => Cow::Borrowed(ops),
                lsp_types::DocumentChanges::Edits(edits) => Cow::Owned(
                    edits
                        .iter()
                        .map(|e| lsp_types::DocumentChangeOperation::Edit(e.clone()))
                        .collect(),
                ),
            }
        } else if let Some(changes) = edit.changes.as_ref() {
            // Legacy map: order by URI for determinism (BTreeMap-ordered).
            let mut uris: Vec<_> = changes.keys().cloned().collect();
            uris.sort();
            Cow::Owned(
                uris.into_iter()
                    .map(|uri| {
                        let edits = changes.get(&uri).cloned().unwrap_or_default();
                        lsp_types::DocumentChangeOperation::Edit(lsp_types::TextDocumentEdit {
                            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                                uri,
                                version: None,
                            },
                            edits: edits.into_iter().map(lsp_types::OneOf::Left).collect(),
                        })
                    })
                    .collect(),
            )
        } else {
            return Ok(Vec::new());
        };

    let mut plan = Vec::with_capacity(operations.len());
    for op in operations.iter() {
        match op {
            lsp_types::DocumentChangeOperation::Op(resource) => match resource {
                lsp_types::ResourceOp::Create(create) => {
                    if let Some(planned) = plan_create(server, create, store).await? {
                        plan.push(planned);
                    }
                }
                lsp_types::ResourceOp::Rename(rename) => {
                    if let Some(planned) = plan_rename(server, rename, store).await? {
                        plan.push(planned);
                    }
                }
                lsp_types::ResourceOp::Delete(delete) => {
                    if let Some(planned) = plan_delete(server, delete, store).await? {
                        plan.push(planned);
                    }
                }
            },
            lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                plan.push(plan_edit(server, doc_edit, store).await?);
            }
        }
    }
    Ok(plan)
}

/// Plans a `create`: refuses an existing file without overwrite, else
/// captures the replaced bytes for undo.
async fn plan_create(
    server: &LspServer,
    create: &lsp_types::CreateFile,
    store: Option<&dyn ArtifactStore>,
) -> Result<Option<PlannedOp>, String> {
    let path = confined_path(server, &create.uri, "create")?;
    let rel = effect_key(server, &path);
    let overwrite = create
        .options
        .as_ref()
        .is_some_and(|o| o.overwrite == Some(true));
    // Preflight + undo capture: an existing file under a
    // no-overwrite create fails the whole batch here,
    // before any IO; an overwrite keeps the original
    // bytes for rollback. The pre-image rides the same
    // read the undo already needed (absent needs none).
    let (undo, before_image) = if path.exists() {
        if !overwrite {
            return Err(format!(
                "create: {} already exists (server did not allow overwrite)",
                path.display()
            ));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("create: cannot read {}: {e}", path.display()))?;
        let image = capture_before_image(store, &rel, Some(&bytes));
        (
            Undo::WriteBytes {
                path: path.clone(),
                bytes,
            },
            image,
        )
    } else {
        (
            Undo::RemoveFile(path.clone()),
            capture_before_image(store, &rel, None),
        )
    };
    Ok(Some(PlannedOp {
        op: PlanOp::Create { path },
        undo,
        before_images: before_image.into_iter().collect(),
    }))
}

/// Plans a `rename`: both endpoints must be in-workspace and the
/// target must not exist without overwrite; both source and target
/// bytes are captured for rollback.
async fn plan_rename(
    server: &LspServer,
    rename: &lsp_types::RenameFile,
    store: Option<&dyn ArtifactStore>,
) -> Result<Option<PlannedOp>, String> {
    let from = confined_path(server, &rename.old_uri, "rename")?;
    let to = confined_path(server, &rename.new_uri, "rename")?;
    let from_rel = effect_key(server, &from);
    let to_rel = effect_key(server, &to);
    let overwrite = rename
        .options
        .as_ref()
        .is_some_and(|o| o.overwrite == Some(true));
    if !from.exists() {
        return Err(format!("rename: {} does not exist", from.display()));
    }
    let replaced = if to.exists() {
        if !overwrite {
            return Err(format!(
                "rename: {} already exists (server did not allow overwrite)",
                to.display()
            ));
        }
        Some(
            tokio::fs::read(&to)
                .await
                .map_err(|e| format!("rename: cannot read {}: {e}", to.display()))?,
        )
    } else {
        None
    };
    // The rename itself never reads the source — but a
    // rewind to before this op must put those bytes
    // back, so the pre-image reads them best-effort. An
    // unreadable source yields NO entry (not "absent":
    // the file existed), so restore refuses instead of
    // deleting on rewind.
    let from_image = match tokio::fs::read(&from).await {
        Ok(bytes) => capture_before_image(store, &from_rel, Some(&bytes)),
        Err(_) => None,
    };
    let to_image = capture_before_image(store, &to_rel, replaced.as_deref());
    let mut before_images = Vec::with_capacity(2);
    before_images.extend(from_image);
    before_images.extend(to_image);
    Ok(Some(PlannedOp {
        op: PlanOp::Rename {
            from: from.clone(),
            to: to.clone(),
        },
        undo: Undo::RenameBack { from, to, replaced },
        before_images,
    }))
}

/// Plans a `delete`: refuses a missing file unless
/// `ignore_if_not_exists` (which plans nothing at all), else captures
/// the removed bytes for undo.
async fn plan_delete(
    server: &LspServer,
    delete: &lsp_types::DeleteFile,
    store: Option<&dyn ArtifactStore>,
) -> Result<Option<PlannedOp>, String> {
    let path = confined_path(server, &delete.uri, "delete")?;
    let ignore_if_not_exists = delete
        .options
        .as_ref()
        .is_some_and(|o| o.ignore_if_not_exists == Some(true));
    if !path.exists() {
        if ignore_if_not_exists {
            // Nothing to do: the op never enters the plan,
            // so apply never touches a missing file.
            return Ok(None);
        }
        return Err(format!("delete: {} does not exist", path.display()));
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("delete: cannot read {}: {e}", path.display()))?;
    let rel = effect_key(server, &path);
    let before_image = capture_before_image(store, &rel, Some(&bytes));
    let undo_path = path.clone();
    Ok(Some(PlannedOp {
        op: PlanOp::Delete { path },
        undo: Undo::WriteBytes {
            path: undo_path,
            bytes,
        },
        before_images: before_image.into_iter().collect(),
    }))
}

/// Plans a text `edit`: version guard, read, apply, capture undo bytes.
async fn plan_edit(
    server: &LspServer,
    doc_edit: &lsp_types::TextDocumentEdit,
    store: Option<&dyn ArtifactStore>,
) -> Result<PlannedOp, String> {
    let path = confined_path(server, &doc_edit.text_document.uri, "edit")?;
    let path_str = path.to_string_lossy().into_owned();
    // Version guard: if the server pinned a version and our
    // tracked open version moved past it, the server computed
    // against stale text — refuse rather than corrupt.
    if let Some(pinned) = doc_edit.text_document.version {
        let local = server.open_version(&path_str);
        if let Some(local) = local
            && i64::from(pinned) < i64::from(local)
        {
            return Err(format!(
                "edit for {} computed against version {pinned} but loom is at {local}; re-run the query",
                path.display()
            ));
        }
    }
    let mut text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("edit: cannot read {}: {e}", path.display()))?;
    let original = text.clone().into_bytes();
    let edits = plain_edits(&doc_edit.edits);
    apply_text_edits(&mut text, &edits, server.uses_utf8_positions())?;
    let rel = effect_key(server, &path);
    let before_image = capture_before_image(store, &rel, Some(&original));
    Ok(PlannedOp {
        op: PlanOp::Edit { path, text },
        undo: Undo::WriteBytes {
            path: std::path::PathBuf::from(path_str),
            bytes: original,
        },
        before_images: before_image.into_iter().collect(),
    })
}

/// Performs one planned operation: pure IO plus effect recording.
/// Effect keys are workspace-relative under `root` (ADR-012), matching
/// every other loom write tool; `notes` keep ABSOLUTE paths because
/// open-document sync keys on them. Pre-images arrive with the op and
/// are folded by the caller after it commits.
async fn perform(
    op: PlanOp,
    root: &std::path::Path,
    effects: &mut ToolEffects,
    notes: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let relative = |path: &std::path::Path| crate::confinement::workspace_relative(root, path);
    match op {
        PlanOp::Create { path } => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("create {}: {e}", path.display()))?;
            }
            // 0.95 CreateFile carries no content (uri/options
            // only): create empty, subsequent edits fill it.
            tokio::fs::write(&path, "")
                .await
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            *effects = std::mem::take(effects).with_file_written(relative(&path));
        }
        PlanOp::Rename { from, to } => {
            tokio::fs::rename(&from, &to)
                .await
                .map_err(|e| format!("rename {} → {}: {e}", from.display(), to.display()))?;
            *effects = std::mem::take(effects).with_file_deleted(relative(&from));
            *effects = std::mem::take(effects).with_file_written(relative(&to));
        }
        PlanOp::Delete { path } => {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| format!("delete {}: {e}", path.display()))?;
            *effects = std::mem::take(effects).with_file_deleted(relative(&path));
        }
        PlanOp::Edit { path, text } => {
            tokio::fs::write(&path, &text)
                .await
                .map_err(|e| format!("edit: cannot write {}: {e}", path.display()))?;
            *effects = std::mem::take(effects).with_file_written(relative(&path));
            notes.push((path.to_string_lossy().into_owned(), text));
        }
    }
    Ok(())
}

/// Runs the undos for already-applied ops (reverse order, best effort)
/// and folds the outcome into one error string.
async fn report_rollback(error: String, undos: Vec<Undo>) -> String {
    let mut failures: Vec<String> = Vec::new();
    for undo in undos.into_iter().rev() {
        if let Err(failure) = undo.rollback().await {
            failures.push(failure);
        }
    }
    if failures.is_empty() {
        format!("{error}; batch rolled back — workspace restored")
    } else {
        format!("{error}; ROLLBACK INCOMPLETE: {}", failures.join("; "))
    }
}

/// Executes a plan op by op. On the first IO failure the already-applied
/// prefix is rolled back (reverse, best effort); effects and sync notes
/// are returned only when the whole batch committed. Pre-image bodies
/// were stored at plan time — a batch that rolls back leaves the body
/// orphaned (content-addressed, GC-collectable) and no ledger entry.
async fn apply_plan(
    plan: Vec<PlannedOp>,
    root: &std::path::Path,
) -> Result<(ToolEffects, Vec<(String, String)>), String> {
    let mut effects = ToolEffects::default();
    let mut notes: Vec<(String, String)> = Vec::new();
    let mut undos: Vec<Undo> = Vec::new();
    for PlannedOp {
        op,
        undo,
        before_images,
    } in plan
    {
        if let Err(error) = perform(op, root, &mut effects, &mut notes).await {
            return Err(report_rollback(error, undos).await);
        }
        for image in before_images {
            effects = std::mem::take(&mut effects).with_before_image(image);
        }
        undos.push(undo);
    }
    Ok((effects, notes))
}

/// Applies a whole [`lsp_types::WorkspaceEdit`] through loom's write
/// path. The batch is fully planned first: every semantic check
/// (confinement, versions, ranges, overlaps) AND every IO precondition
/// (existence, overwrite flags, undo snapshots of original bytes) — a
/// plan error touches nothing. IO failures mid-batch (permissions,
/// disk) roll the applied prefix back in reverse; effects and
/// open-document sync notes are recorded only after the whole batch
/// commits, so a rolled-back run leaves neither ledger writes nor
/// stale didChange bumps. `artifacts` receives the pre-image bodies
/// (tier-2 rewind); without a store, writes still happen but restores
/// for them will refuse.
pub(crate) async fn apply_workspace_edit(
    server: &LspServer,
    edit: &lsp_types::WorkspaceEdit,
    artifacts: Option<&std::sync::Arc<dyn ArtifactStore>>,
) -> Result<ToolEffects, String> {
    let plan = plan_workspace_edit(server, edit, artifacts).await?;
    let (effects, notes) = apply_plan(plan, server.workspace_root()).await?;
    // Keep the server's view of open files current — after commit only.
    for (path, text) in notes {
        server.note_local_write(&path, text);
    }
    Ok(effects)
}

#[cfg(test)]
#[path = "apply/tests.rs"]
mod tests;
