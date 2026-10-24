//! WorkspaceEdit application through loom's own write path (slice E2,
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
//!   insert "World"@3 then "Hello"@3 yields "WorldHello").
//! - Every text range converts from the server's negotiated position
//!   encoding (utf-16 by default) to byte offsets before slicing.

use async_lsp::lsp_types;
use loom_kernel::tool::ToolEffects;

use crate::lsp::client::LspServer;

/// Converts an LSP range (zero-based line/character, utf-16 chars by
/// default) into byte offsets against the current file text.
fn range_to_bytes(
    text: &str,
    range: &lsp_types::Range,
    utf8_positions: bool,
) -> Result<(usize, usize), String> {
    let line_starts = line_start_offsets(text);
    let offset_of = |pos: &lsp_types::Position| -> Result<usize, String> {
        let line = pos.line as usize;
        if line >= line_starts.len() {
            return Err(format!(
                "edit position line {} outside file ({} lines)",
                line + 1,
                line_starts.len()
            ));
        }
        let line_start = line_starts[line];
        let line_end = if line + 1 < line_starts.len() {
            line_starts[line + 1]
        } else {
            text.len()
        };
        let line_text = &text[line_start..line_end];
        let byte_col = if utf8_positions {
            // UTF-8 position encoding: the wire value IS a byte offset.
            (pos.character as usize).min(line_text.len())
        } else {
            crate::lsp::position::utf16_units_to_byte_offset(line_text, pos.character)
        };
        Ok(line_start + byte_col)
    };
    let start = offset_of(&range.start)?;
    let end = offset_of(&range.end)?;
    if end < start {
        return Err("edit range end before start".to_string());
    }
    if end > text.len() {
        return Err("edit range end outside file".to_string());
    }
    Ok((start, end))
}

/// Byte offset of every line start (0 for line 0), including one past
/// the final newline so the last line has an end boundary.
fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

/// Applies one batch of text edits to a document in place.
///
/// Ordering: a stable ascending sort over (line, character) keeps
/// array order among edits that share a start position; applying the
/// sorted sequence in **reverse** then walks bottom-up (later offsets
/// first, so earlier offsets stay valid) while same-position inserts
/// land in array order in the final text — the spec's canonical
/// `[insert "World"@3, insert "Hello"@3] → "WorldHello"` case.
fn apply_text_edits(
    text: &mut String,
    edits: &[lsp_types::TextEdit],
    utf8_positions: bool,
) -> Result<(), String> {
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by(|&a, &b| {
        (edits[a].range.start.line, edits[a].range.start.character)
            .cmp(&(edits[b].range.start.line, edits[b].range.start.character))
    });
    let mut last_start: Option<usize> = None;
    let mut bytes = std::mem::take(text).into_bytes();
    for idx in order.into_iter().rev() {
        let edit = &edits[idx];
        // Zero-copy view; a splice through a non-utf-8 boundary fails
        // closed here instead of corrupting the file with U+FFFD.
        let current =
            std::str::from_utf8(&bytes).map_err(|e| format!("edited file is not utf-8: {e}"))?;
        let (start, end) = range_to_bytes(current, &edit.range, utf8_positions)?;
        if let Some(prev) = last_start
            && end > prev
        {
            return Err("overlapping text edits in one document".to_string());
        }
        last_start = Some(start);
        bytes.splice(start..end, edit.new_text.bytes().collect::<Vec<u8>>());
    }
    *text = String::from_utf8(bytes).map_err(|e| format!("edited file is not utf-8: {e}"))?;
    Ok(())
}

/// Extracts the plain [`lsp_types::TextEdit`]s from an edit list
/// (annotated edits carry the same shape plus metadata we ignore).
fn plain_edits(
    edits: &[lsp_types::OneOf<lsp_types::TextEdit, lsp_types::AnnotatedTextEdit>],
) -> Vec<lsp_types::TextEdit> {
    edits
        .iter()
        .map(|e| match e {
            lsp_types::OneOf::Left(edit) => edit.clone(),
            lsp_types::OneOf::Right(annotated) => annotated.text_edit.clone(),
        })
        .collect()
}

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
}

/// Resolves a server-supplied URI to a workspace-confined path.
///
/// Servers are untrusted: a WorkspaceEdit naming `/etc/passwd` or any
/// other path outside the workspace is rejected here, before any IO —
/// the confinement invariant every other loom write tool obeys.
fn confined_path(
    server: &LspServer,
    uri: &lsp_types::Url,
    op: &str,
) -> Result<std::path::PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("{op}: non-file uri {uri}"))?;
    let display = path.display().to_string();
    crate::confinement::resolve_within(server.workspace_root(), &display)
        .map_err(|e| format!("{op}: uri {display} escapes the workspace root: {e}"))
}

/// Builds the validated plan for a whole edit batch: every semantic
/// error (confinement escape, non-file URI, stale version, bad range,
/// overlap) surfaces here, before the first mutation.
async fn plan_workspace_edit(
    server: &LspServer,
    edit: &lsp_types::WorkspaceEdit,
) -> Result<Vec<PlannedOp>, String> {
    let operations: Vec<lsp_types::DocumentChangeOperation> =
        if let Some(changes) = edit.document_changes.as_ref() {
            match changes {
                lsp_types::DocumentChanges::Operations(ops) => ops.clone(),
                lsp_types::DocumentChanges::Edits(edits) => edits
                    .iter()
                    .map(|e| lsp_types::DocumentChangeOperation::Edit(e.clone()))
                    .collect(),
            }
        } else if let Some(changes) = edit.changes.as_ref() {
            // Legacy map: order by URI for determinism (BTreeMap-ordered).
            let mut uris: Vec<_> = changes.keys().cloned().collect();
            uris.sort();
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
                .collect()
        } else {
            return Ok(Vec::new());
        };

    let mut plan = Vec::with_capacity(operations.len());
    for op in &operations {
        match op {
            lsp_types::DocumentChangeOperation::Op(resource) => match resource {
                lsp_types::ResourceOp::Create(create) => {
                    let path = confined_path(server, &create.uri, "create")?;
                    let overwrite = create
                        .options
                        .as_ref()
                        .is_some_and(|o| o.overwrite == Some(true));
                    // Preflight + undo capture: an existing file under a
                    // no-overwrite create fails the whole batch here,
                    // before any IO; an overwrite keeps the original
                    // bytes for rollback.
                    let undo = if path.exists() {
                        if !overwrite {
                            return Err(format!(
                                "create: {} already exists (server did not allow overwrite)",
                                path.display()
                            ));
                        }
                        let bytes = tokio::fs::read(&path)
                            .await
                            .map_err(|e| format!("create: cannot read {}: {e}", path.display()))?;
                        Undo::WriteBytes {
                            path: path.clone(),
                            bytes,
                        }
                    } else {
                        Undo::RemoveFile(path.clone())
                    };
                    plan.push(PlannedOp {
                        op: PlanOp::Create { path },
                        undo,
                    });
                }
                lsp_types::ResourceOp::Rename(rename) => {
                    let from = confined_path(server, &rename.old_uri, "rename")?;
                    let to = confined_path(server, &rename.new_uri, "rename")?;
                    let overwrite = rename
                        .options
                        .as_ref()
                        .is_some_and(|o| o.overwrite == Some(true));
                    if !from.exists() {
                        return Err(format!("rename: {} does not exist", from.display()));
                    }
                    let replaced =
                        if to.exists() {
                            if !overwrite {
                                return Err(format!(
                                    "rename: {} already exists (server did not allow overwrite)",
                                    to.display()
                                ));
                            }
                            Some(tokio::fs::read(&to).await.map_err(|e| {
                                format!("rename: cannot read {}: {e}", to.display())
                            })?)
                        } else {
                            None
                        };
                    plan.push(PlannedOp {
                        op: PlanOp::Rename {
                            from: from.clone(),
                            to: to.clone(),
                        },
                        undo: Undo::RenameBack { from, to, replaced },
                    });
                }
                lsp_types::ResourceOp::Delete(delete) => {
                    let path = confined_path(server, &delete.uri, "delete")?;
                    let ignore_if_not_exists = delete
                        .options
                        .as_ref()
                        .is_some_and(|o| o.ignore_if_not_exists == Some(true));
                    if !path.exists() {
                        if ignore_if_not_exists {
                            // Nothing to do: the op never enters the plan,
                            // so apply never touches a missing file.
                            continue;
                        }
                        return Err(format!("delete: {} does not exist", path.display()));
                    }
                    let bytes = tokio::fs::read(&path)
                        .await
                        .map_err(|e| format!("delete: cannot read {}: {e}", path.display()))?;
                    let undo_path = path.clone();
                    plan.push(PlannedOp {
                        op: PlanOp::Delete { path },
                        undo: Undo::WriteBytes {
                            path: undo_path,
                            bytes,
                        },
                    });
                }
            },
            lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                let path = confined_path(server, &doc_edit.text_document.uri, "edit")?;
                let path_str = path.to_string_lossy().into_owned();
                // Version guard: if the server pinned a version and our
                // tracked open version moved past it, the server computed
                // against stale text — refuse rather than corrupt.
                if let Some(pinned) = doc_edit.text_document.version {
                    let local = server.open_version(&path_str);
                    if let Some(local) = local
                        && (pinned as i64) < i64::from(local)
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
                plan.push(PlannedOp {
                    op: PlanOp::Edit { path, text },
                    undo: Undo::WriteBytes {
                        path: std::path::PathBuf::from(path_str),
                        bytes: original,
                    },
                });
            }
        }
    }
    Ok(plan)
}

/// Performs one planned operation: pure IO plus effect recording.
/// `notes` collects (path, final text) for open-document sync after the
/// whole batch commits.
async fn perform(
    op: PlanOp,
    effects: &mut ToolEffects,
    notes: &mut Vec<(String, String)>,
) -> Result<(), String> {
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
            *effects = std::mem::take(effects).with_file_written(path.to_string_lossy());
        }
        PlanOp::Rename { from, to } => {
            tokio::fs::rename(&from, &to)
                .await
                .map_err(|e| format!("rename {} → {}: {e}", from.display(), to.display()))?;
            *effects = std::mem::take(effects).with_file_deleted(from.to_string_lossy());
            *effects = std::mem::take(effects).with_file_written(to.to_string_lossy());
        }
        PlanOp::Delete { path } => {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| format!("delete {}: {e}", path.display()))?;
            *effects = std::mem::take(effects).with_file_deleted(path.to_string_lossy());
        }
        PlanOp::Edit { path, text } => {
            tokio::fs::write(&path, &text)
                .await
                .map_err(|e| format!("edit: cannot write {}: {e}", path.display()))?;
            *effects = std::mem::take(effects).with_file_written(path.to_string_lossy());
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
/// are returned only when the whole batch committed.
async fn apply_plan(plan: Vec<PlannedOp>) -> Result<(ToolEffects, Vec<(String, String)>), String> {
    let mut effects = ToolEffects::default();
    let mut notes: Vec<(String, String)> = Vec::new();
    let mut undos: Vec<Undo> = Vec::new();
    for PlannedOp { op, undo } in plan {
        if let Err(error) = perform(op, &mut effects, &mut notes).await {
            return Err(report_rollback(error, undos).await);
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
/// stale didChange bumps.
pub(crate) async fn apply_workspace_edit(
    server: &LspServer,
    edit: &lsp_types::WorkspaceEdit,
) -> Result<ToolEffects, String> {
    let plan = plan_workspace_edit(server, edit).await?;
    let (effects, notes) = apply_plan(plan).await?;
    // Keep the server's view of open files current — after commit only.
    for (path, text) in notes {
        server.note_local_write(&path, text).await;
    }
    Ok(effects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(line: u32, start: u32, end: u32, new: &str) -> lsp_types::TextEdit {
        lsp_types::TextEdit {
            range: lsp_types::Range {
                start: lsp_types::Position::new(line, start),
                end: lsp_types::Position::new(line, end),
            },
            new_text: new.to_string(),
        }
    }

    #[test]
    fn applies_edits_bottom_up() {
        let mut text = String::from("fn one() {}\nfn two() {}\n");
        apply_text_edits(
            &mut text,
            &[edit(1, 3, 6, "double"), edit(0, 3, 6, "single")],
            false,
        )
        .unwrap();
        assert_eq!(text, "fn single() {}\nfn double() {}\n");
    }

    #[test]
    fn rejects_overlapping_edits() {
        let mut text = String::from("abcdef\n");
        let err = apply_text_edits(&mut text, &[edit(0, 0, 3, "X"), edit(0, 2, 5, "Y")], false);
        assert!(err.is_err());
    }

    #[test]
    fn utf16_ranges_map_through_multibyte_lines() {
        // "ab😀cd ef" — 😀 occupies utf-16 units 2..4; 'e' is unit 7.
        let mut text = String::from("ab😀cd ef\n");
        apply_text_edits(&mut text, &[edit(0, 7, 8, "E")], false).unwrap();
        assert_eq!(text, "ab😀cd Ef\n");
    }

    #[test]
    fn same_position_inserts_keep_array_order() {
        // Spec test case: same-start inserts appear in array order in
        // the resulting text ("WorldHello", not "HelloWorld").
        let mut text = String::from("abc");
        apply_text_edits(
            &mut text,
            &[edit(0, 3, 3, "World"), edit(0, 3, 3, "Hello")],
            false,
        )
        .unwrap();
        assert_eq!(text, "abcWorldHello");
    }

    #[test]
    fn mixed_inserts_and_replace_at_same_start() {
        // Any number of inserts at one position followed by a replace
        // at that position is a legal batch (spec): inserts land in
        // array order, the replace consumes the original bytes.
        let mut text = String::from("abcdef");
        apply_text_edits(
            &mut text,
            &[edit(0, 2, 2, "1"), edit(0, 2, 2, "2"), edit(0, 2, 4, "X")],
            false,
        )
        .unwrap();
        assert_eq!(text, "ab12Xef");
    }

    #[test]
    fn invalid_utf8_splice_fails_closed() {
        // A range whose byte offsets split a multibyte char cannot
        // produce valid utf-8; the edit is refused, not corrupted.
        let mut text = String::from("ab😀cd\n");
        let err = apply_text_edits(
            &mut text,
            // 😀 occupies bytes 2..6; cutting at byte 4 splits it.
            // Send raw byte offsets via the utf-8 encoding flag.
            &[edit(0, 4, 6, "X")],
            true,
        );
        assert!(
            err.is_err(),
            "splice through a multibyte char must fail closed"
        );
    }

    #[test]
    fn line_starts_count_every_newline() {
        assert_eq!(line_start_offsets("a\nbb\nccc\n"), vec![0, 2, 5, 9]);
        assert_eq!(line_start_offsets("no newline"), vec![0]);
    }

    #[tokio::test]
    async fn rollback_on_mid_batch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "original").unwrap();
        // A regular FILE sitting where the second op needs a directory:
        // create_dir_all cannot succeed, so op 2 fails after op 1 wrote.
        let barrier = dir.path().join("barrier");
        std::fs::write(&barrier, "i am a file").unwrap();
        let doomed = barrier.join("inner.txt");

        let plan = vec![
            PlannedOp {
                op: PlanOp::Edit {
                    path: file.clone(),
                    text: "changed".to_owned(),
                },
                undo: Undo::WriteBytes {
                    path: file.clone(),
                    bytes: b"original".to_vec(),
                },
            },
            PlannedOp {
                op: PlanOp::Create { path: doomed },
                undo: Undo::RemoveFile(barrier.join("inner.txt")),
            },
        ];
        let err = apply_plan(plan).await.expect_err("second op must fail");
        assert!(err.contains("rolled back"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[tokio::test]
    async fn undo_restores_replaced_rename_target() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        std::fs::write(&from, "moved").unwrap();
        std::fs::write(&to, "clobbered original").unwrap();

        let undo = Undo::RenameBack {
            from: from.clone(),
            to: to.clone(),
            replaced: Some(b"clobbered original".to_vec()),
        };
        tokio::fs::rename(&from, &to).await.unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "moved");
        undo.rollback().await.unwrap();
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "moved");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "clobbered original");
    }
}
