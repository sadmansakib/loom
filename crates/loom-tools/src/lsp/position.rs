//! Position handling for the LSP tools (docs/lsp-design.md §6).
//!
//! LSP speaks zero-based `line/character` positions where the character
//! offset is counted in UTF-16 code units unless the server negotiated
//! UTF-8. loom's tool surface is drift-tolerant and human-shaped: inputs
//! are 1-based `line` + optional `column`, or a `symbol` substring the
//! tool locates in the file; outputs are `path:line:col` 1-based.

/// A 1-based position on the tool surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePosition {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column in characters, if known.
    pub column: Option<u32>,
}

/// Locates the first line containing `symbol` and returns its 1-based
/// line plus the 1-based character column where the symbol starts.
///
/// Character (not byte) indexing keeps multibyte files correct.
pub fn locate_symbol(lines: &[&str], symbol: &str) -> Option<SurfacePosition> {
    if symbol.is_empty() {
        return None;
    }
    for (idx, line) in lines.iter().enumerate() {
        let mut byte_idx = 0usize;
        for (char_idx, ch) in line.chars().enumerate() {
            if line[byte_idx..].starts_with(symbol) {
                return Some(SurfacePosition {
                    line: idx as u32 + 1,
                    column: Some(char_idx as u32 + 1),
                });
            }
            byte_idx += ch.len_utf8();
        }
    }
    None
}

/// Converts a 1-based (line, column-char) surface position to an LSP
/// zero-based position encoded for `utf16` servers.
pub fn surface_to_utf16(line: u32, column_chars: u32, text_of_line: &str) -> lsp_types::Position {
    let mut units = 0u32;
    for (i, ch) in text_of_line.chars().enumerate() {
        if i as u32 >= column_chars.saturating_sub(1) {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    lsp_types::Position {
        line: line.saturating_sub(1),
        character: units,
    }
}

/// Converts a 1-based (line, column-char) surface position to an LSP
/// zero-based position for servers that negotiated `utf-8` (the
/// character offset passes through unchanged).
pub fn surface_to_utf8(line: u32, column_chars: u32) -> lsp_types::Position {
    lsp_types::Position {
        line: line.saturating_sub(1),
        character: column_chars.saturating_sub(1),
    }
}

/// Converts an LSP zero-based position into a 1-based
/// `line:column` render for tool output.
pub fn render_position(pos: lsp_types::Position) -> String {
    format!("{}:{}", pos.line + 1, pos.character + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_first_occurrence_in_chars() {
        let lines = vec!["let x = 1;", "let 变量 = x + 2;", "let y = x;"];
        let hit = locate_symbol(&lines, "变量").expect("symbol present");
        assert_eq!(hit.line, 2);
        assert_eq!(hit.column, Some(5));
    }

    #[test]
    fn locates_plain_ascii_symbol() {
        let lines = vec!["fn main() {}", "    let x = 1;"];
        let hit = locate_symbol(&lines, "let x").expect("symbol present");
        assert_eq!(hit.line, 2);
        assert_eq!(hit.column, Some(5));
    }

    #[test]
    fn missing_symbol_is_none() {
        assert!(locate_symbol(&["fn main() {}"], "nope").is_none());
    }

    #[test]
    fn empty_symbol_is_none() {
        assert!(locate_symbol(&["fn main() {}"], "").is_none());
    }

    #[test]
    fn utf16_conversion_counts_surrogate_pairs() {
        // 😀 is one char but two UTF-16 units; column 6 = after 5 chars.
        let line = "ab😀cd 😀";
        let pos = surface_to_utf16(1, 6, line);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 6); // a,b,😀(2),c,d = 6 units
        let pos = surface_to_utf16(1, 4, line);
        assert_eq!(pos.character, 4); // after a,b,😀 (1+1+2 units)
    }

    #[test]
    fn utf8_conversion_passes_chars_through() {
        let pos = surface_to_utf8(3, 7);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 6);
    }

    #[test]
    fn render_is_one_based() {
        assert_eq!(render_position(lsp_types::Position::new(0, 0)), "1:1");
    }
}
