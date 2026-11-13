//! LSP text-edit conversion and application helpers.

use async_lsp::lsp_types;

pub(super) fn range_to_bytes(
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
pub(super) fn line_start_offsets(text: &str) -> Vec<usize> {
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
pub(super) fn apply_text_edits(
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
pub(super) fn plain_edits(
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
