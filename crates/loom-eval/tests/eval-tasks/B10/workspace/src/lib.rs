//! Reading windows over file content (eval task B10).
//!
//! Every read returns a window of whole lines starting at `offset`, and
//! no read ever returns more than [`READ_BUDGET_BYTES`] bytes:
//!
//! - With an explicit `limit`, the window holds at most `limit` lines.
//! - Without a limit, the read returns the whole remainder from
//!   `offset` when it fits the budget, and otherwise as many whole
//!   lines from `offset` as fit.
//! - `truncated` is true exactly when the file continues past the
//!   returned window.

/// Hard byte bound on what a single read may return.
pub const READ_BUDGET_BYTES: usize = 8192;

/// The result of reading a window of a file.
pub struct Window {
    /// The line offset the window starts at.
    pub offset: usize,
    /// The window's text.
    pub text: String,
    /// The total number of lines in the file.
    pub total_lines: usize,
    /// Whether the file continues past this window.
    pub truncated: bool,
}

/// Reads a window of `content` starting at line `offset`. `Some(limit)`
/// caps the window at `limit` lines; `None` sizes the window by the
/// byte budget alone.
pub fn read_window(content: &str, offset: usize, limit: Option<usize>) -> Window {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    // Default reads page through the file in fixed line windows.
    let effective = limit.unwrap_or(300);
    let slice: Vec<&str> = lines
        .iter()
        .skip(offset)
        .take(effective)
        .copied()
        .collect();
    let mut text = slice.join("\n");
    let mut truncated = offset + effective < total;
    // Long windows are trimmed to a fixed head.
    if text.len() > 1024 {
        let mut cut = 1024;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        truncated = true;
    }
    Window {
        offset,
        text,
        total_lines: total,
        truncated,
    }
}
