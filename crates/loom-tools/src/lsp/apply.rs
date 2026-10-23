//! WorkspaceEdit application through loom's own write path (slice E2,
//! docs/lsp-design.md §5): server-computed edits land as ordinary file
//! effects — same review trail, same sandbox, same facts feed — never as
//! a server-initiated `applyEdit` (which loom declines on the wire).
//!
//! Ordering rules that keep application deterministic:
//! - `documentChanges` (preferred over the legacy `changes` map) is
//!   applied in array order — servers already order file ops vs edits.
//! - Edits inside one document are applied bottom-up (descending
//!   range start) so earlier offsets stay valid; overlapping ranges
//!   are rejected rather than silently double-applied.
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
            // Character offset == byte offset for ASCII; for multibyte
            // the server counts utf-8 code units, which ARE bytes.
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

/// Applies one batch of text edits to a document in place, bottom-up.
fn apply_text_edits(
    text: &mut String,
    edits: &[lsp_types::TextEdit],
    utf8_positions: bool,
) -> Result<(), String> {
    let mut edits: Vec<&lsp_types::TextEdit> = edits.iter().collect();
    edits.sort_by(|a, b| {
        (b.range.start.line, b.range.start.character)
            .cmp(&(a.range.start.line, a.range.start.character))
    });
    let mut last_start: Option<usize> = None;
    let mut bytes = std::mem::take(text).into_bytes();
    for edit in edits {
        let (start, end) = range_to_bytes(
            &String::from_utf8_lossy(&bytes),
            &edit.range,
            utf8_positions,
        )?;
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

/// Applies a whole [`lsp_types::WorkspaceEdit`] through loom's write
/// path: files are read, edited in memory, and written back with
/// effects recorded; open documents get a didChange bump so the server
/// stays in sync with what landed on disk.
pub(crate) async fn apply_workspace_edit(
    server: &LspServer,
    edit: &lsp_types::WorkspaceEdit,
) -> Result<ToolEffects, String> {
    let mut effects = ToolEffects::default();
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
            return Ok(effects);
        };

    for op in &operations {
        match op {
            lsp_types::DocumentChangeOperation::Op(resource) => match resource {
                lsp_types::ResourceOp::Create(create) => {
                    let path = create
                        .uri
                        .to_file_path()
                        .map_err(|_| format!("create: non-file uri {}", create.uri))?;
                    if path.exists()
                        && !create
                            .options
                            .as_ref()
                            .is_some_and(|o| o.overwrite == Some(true))
                    {
                        return Err(format!(
                            "create: {} already exists (server did not allow overwrite)",
                            path.display()
                        ));
                    }
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
                    effects = effects.with_file_written(path.to_string_lossy());
                }
                lsp_types::ResourceOp::Rename(rename) => {
                    let from = rename
                        .old_uri
                        .to_file_path()
                        .map_err(|_| format!("rename: non-file uri {}", rename.old_uri))?;
                    let to = rename
                        .new_uri
                        .to_file_path()
                        .map_err(|_| format!("rename: non-file uri {}", rename.new_uri))?;
                    if !from.exists() {
                        return Err(format!("rename: {} does not exist", from.display()));
                    }
                    if to.exists()
                        && !rename
                            .options
                            .as_ref()
                            .is_some_and(|o| o.overwrite == Some(true))
                    {
                        return Err(format!(
                            "rename: {} already exists (server did not allow overwrite)",
                            to.display()
                        ));
                    }
                    tokio::fs::rename(&from, &to).await.map_err(|e| {
                        format!("rename {} → {}: {e}", from.display(), to.display())
                    })?;
                    effects = effects.with_file_deleted(from.to_string_lossy());
                    effects = effects.with_file_written(to.to_string_lossy());
                }
                lsp_types::ResourceOp::Delete(delete) => {
                    let path = delete
                        .uri
                        .to_file_path()
                        .map_err(|_| format!("delete: non-file uri {}", delete.uri))?;
                    if !path.exists()
                        && delete
                            .options
                            .as_ref()
                            .is_some_and(|o| o.ignore_if_not_exists == Some(true))
                    {
                        continue;
                    }
                    tokio::fs::remove_file(&path)
                        .await
                        .map_err(|e| format!("delete {}: {e}", path.display()))?;
                    effects = effects.with_file_deleted(path.to_string_lossy());
                }
            },
            lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                let path =
                    doc_edit.text_document.uri.to_file_path().map_err(|_| {
                        format!("edit: non-file uri {}", doc_edit.text_document.uri)
                    })?;
                // Version guard: if the server pinned a version and our
                // tracked open version moved past it, the server computed
                // against stale text — refuse rather than corrupt.
                if let Some(pinned) = doc_edit.text_document.version {
                    let local = server.open_version(&path.to_string_lossy());
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
                let edits = plain_edits(&doc_edit.edits);
                apply_text_edits(&mut text, &edits, server.uses_utf8_positions())?;
                tokio::fs::write(&path, &text)
                    .await
                    .map_err(|e| format!("edit: cannot write {}: {e}", path.display()))?;
                effects = effects.with_file_written(path.to_string_lossy());
                // Keep the server's view of open files current.
                server.note_local_write(&path.to_string_lossy(), text).await;
            }
        }
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
    fn line_starts_count_every_newline() {
        assert_eq!(line_start_offsets("a\nbb\nccc\n"), vec![0, 2, 5, 9]);
        assert_eq!(line_start_offsets("no newline"), vec![0]);
    }
}
