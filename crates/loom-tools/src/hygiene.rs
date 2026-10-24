//! Result-channel hygiene (Invariant-Labs exfil class).
//!
//! [`crate::mcp::sanitize_text`] normalizes *descriptions* (whitespace
//! collapses — they are prose). Tool *results* are often code: layout
//! must survive. `strip_invisible` removes only characters that are
//! invisible in every context — the zero-width/bidi/soft-hyphen family
//! used to hide payloads past human eyes (trailing-space padding,
//! homoglyph smuggling) — and keeps tabs, newlines, and carriage
//! returns so diffs, code, and tables render unchanged.
//!
//! Semantic result-content attacks (prose that addresses the agent)
//! are a documented residual: deterministic detection over natural
//! language is a noise generator, not a control.

/// Characters invisible in every rendering context: C0/C1 controls
/// (except the layout trio `\t`, `\n`, `\r`), soft hyphen, Arabic
/// letter mark, Mongolian vowel separator, the zero-width family,
/// bidi embedding/override controls, word joiner and invisible
/// operators, and the BOM.
pub(crate) fn is_invisible(c: char) -> bool {
    if c.is_control() && !matches!(c, '\t' | '\n' | '\r') {
        return true;
    }
    matches!(
        c,
        '\u{00AD}'       // soft hyphen
        | '\u{061C}'     // arabic letter mark
        | '\u{180E}'     // mongolian vowel separator
        | '\u{200B}'..='\u{200F}' // zero-width space..right-to-left mark
        | '\u{202A}'..='\u{202E}' // bidi embedding..pop directional
        | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}' // isolated bidi overrides
        | '\u{FEFF}'     // zero-width no-break space (BOM)
    )
}

/// Strips every [`is_invisible`] character, preserving all whitespace
/// layout. Applied to MCP and LSP tool results before they enter the
/// transcript as data.
pub(crate) fn strip_invisible(text: &str) -> String {
    text.chars().filter(|c| !is_invisible(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_invisible_but_keeps_layout() {
        let raw = "line1\u{200B}\n\tindented\u{202E}code\u{FEFF} x\u{00AD}y";
        let clean = strip_invisible(raw);
        assert_eq!(clean, "line1\n\tindentedcode xy");
    }

    #[test]
    fn keeps_tabs_newlines_carriage_returns() {
        assert_eq!(strip_invisible("a\tb\nc\r\nd"), "a\tb\nc\r\nd");
    }

    #[test]
    fn strips_c0_and_c1_controls() {
        assert_eq!(strip_invisible("a\u{0}b\u{7}c\u{9B}d"), "abcd");
    }

    #[test]
    fn plain_text_untouched() {
        assert_eq!(
            strip_invisible("fn main() {} // é 中 😀"),
            "fn main() {} // é 中 😀"
        );
    }
}
