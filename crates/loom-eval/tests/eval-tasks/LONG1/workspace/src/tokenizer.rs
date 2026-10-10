//! Chapter 3: source text to positioned tokens.

/// One lexed instruction with its source offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub offset: usize,
    pub kind: Kind,
}

/// The eight instructions of the alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Inc,
    Dec,
    Next,
    Prev,
    Read,
    Write,
    LoopOpen,
    LoopClose,
}

/// Lexes source into tokens, ignoring every character outside the
/// alphabet — including `#` comment lines (chapter 2).
pub fn tokenize(source: &str) -> Vec<Token> {
    source
        .char_indices()
        .filter_map(|(offset, ch)| {
            let kind = match ch {
                '+' => Kind::Inc,
                '-' => Kind::Dec,
                '>' => Kind::Next,
                '<' => Kind::Prev,
                ',' => Kind::Read,
                '.' => Kind::Write,
                '[' => Kind::LoopOpen,
                ']' => Kind::LoopClose,
                _ => return None,
            };
            Some(Token { offset, kind })
        })
        .collect()
}
