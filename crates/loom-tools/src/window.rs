//! Shared numbered-line windowing for `read_file` and `artifact_read`.
//!
//! Both tools present text as a *window* of numbered lines with a total-lines
//! header, so the prompt's "read the smallest relevant range" is actionable
//! (ADR-014, V7). The window is bounded by lines (`limit`) and by bytes
//! ([`READ_FILE_HEAD_BYTES`]); both cuts are explicit.

use std::io::Read;
use std::path::Path;

use crate::budget;

/// Lines returned when the caller does not pass `limit`.
///
/// Deliberately large: the real bound is the byte budget
/// ([`READ_FILE_HEAD_BYTES`]), so a default read returns an ordinary source
/// file in one call instead of forcing the model to page it. Paging costs a
/// full model turn per page (each replaying the whole transcript); on B1 that
/// measured ~2× the input tokens of a whole-file read, which is why the line
/// default is not the knob that bounds a read.
pub(crate) const DEFAULT_LIMIT: usize = 2000;

/// Bytes scanned to count the total lines. Larger inputs are reported as
/// "at least N lines" and can still be windowed with `offset`.
pub(crate) const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// One requested window plus the total line count that could be seen.
pub(crate) struct Window {
    /// 1-based line number and text for each returned line.
    pub(crate) lines: Vec<(usize, String)>,
    /// Total lines in the input, or the lines seen before the scan cap.
    pub(crate) total: usize,
    /// Whether the whole input was scanned (`false` when the scan cap was hit).
    pub(crate) complete: bool,
}

impl Window {
    /// Windows in-memory text: an artifact body, or a file already decoded.
    pub(crate) fn from_text(text: &str, offset: usize, limit: usize, complete: bool) -> Self {
        let all: Vec<&str> = text.lines().collect();
        let total = all.len();
        let start = offset.saturating_sub(1).min(total);
        let end = start.saturating_add(limit).min(total);
        let lines = (start..end).map(|i| (i + 1, all[i].to_owned())).collect();
        Self {
            lines,
            total,
            complete,
        }
    }
}

/// Reads and windows one file. Returns `Ok(None)` for a missing path, an
/// [`std::io::ErrorKind::InvalidData`] error for non-UTF-8 content, and
/// `InvalidData`/`other` for non-regular files.
pub(crate) fn read_file_window(
    path: &Path,
    offset: usize,
    limit: usize,
) -> std::io::Result<Option<Window>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let complete = bytes.len() <= MAX_SCAN_BYTES;
    let text = crate::text::decode_capped(bytes, MAX_SCAN_BYTES)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8"))?;
    Ok(Some(Window::from_text(&text, offset, limit, complete)))
}

/// Decodes an artifact body as UTF-8, cutting at the scan cap.
///
/// Returns `None` for bodies that are not valid UTF-8 (the artifact holds
/// bytes, which need not be text).
pub(crate) fn decode_body(body: Vec<u8>) -> Option<(String, bool)> {
    let complete = body.len() <= MAX_SCAN_BYTES;
    let text = crate::text::decode_capped(body, MAX_SCAN_BYTES)?;
    Some((text, complete))
}

/// Renders a window: a header naming the total, the numbered lines, an explicit
/// hint when more lines remain, then the byte budget. Returns whether anything
/// was cut (echoing or exceeding the window both count).
pub(crate) fn render_window(
    label: &str,
    window: &Window,
    offset: usize,
    head_bytes: usize,
) -> (String, bool) {
    if window.total == 0 {
        return (format!("{label} is empty"), false);
    }
    let mut text = if window.complete {
        format!("{label}: {} lines\n", window.total)
    } else {
        format!(
            "{label}: at least {} lines (exceeds the {} MiB scan cap)\n",
            window.total,
            MAX_SCAN_BYTES / (1024 * 1024)
        )
    };
    if window.lines.is_empty() {
        // Only reachable when `offset` is past the last line.
        text.push_str(&format!("... [offset {offset} is past the last line]"));
        return (text, false);
    }
    for (number, line) in &window.lines {
        text.push_str(&format!("{number}\t{line}\n"));
    }
    let shown_end = window
        .lines
        .last()
        .map(|(number, _)| *number)
        .unwrap_or(offset);
    let mut truncated = !window.complete;
    if window.complete && window.total > shown_end {
        let remaining = window.total - shown_end;
        text.push_str(&format!(
            "... [{remaining} more lines; read with offset={}]\n",
            shown_end + 1
        ));
        truncated = true;
    }
    let (text, cut) = budget::head_only(&text, head_bytes);
    (text, truncated || cut)
}
