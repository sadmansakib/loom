//! The fast-path analyzer: the value grammar (`Literal`, `Intent`), the
//! phrase tables, the anchored parsers, and [`analyze`] — the strict
//! whole-task-text recognizer that declines instead of guessing.

/// A literal value as it can appear in a task instruction and in a
/// `const` declaration. Rendering is round-trip-exact: the token the
/// analyzer parsed from the instruction must equal the token in the
/// source, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Literal {
    Int(String),
    Bool(bool),
    Str(String),
}

impl Literal {
    pub(super) fn render(&self) -> String {
        match self {
            Self::Int(text) => text.clone(),
            Self::Bool(value) => value.to_string(),
            Self::Str(value) => format!("{value:?}"),
        }
    }
}

/// One recognized mechanical instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Intent {
    Rename {
        from: String,
        to: String,
    },
    Constant {
        name: String,
        from: Option<Literal>,
        to: Literal,
    },
    /// A rust/rustfmt-specific formatting instruction — never routed to
    /// another ecosystem's formatter.
    FormatRust,
    /// A gofmt-specific formatting instruction.
    FormatGo,
    /// An ecosystem-neutral phrase ("format the codebase"); the
    /// workspace's manifest picks the formatter.
    Format,
    DependencyUpdate {
        crate_name: String,
        to_version: String,
    },
}

impl Intent {
    /// The decline reason when the intent matched but its solver's
    /// guards (or the offline verifier) said no. Rename classifies
    /// its own causes before reaching here (item 34); this arm is
    /// the unclassified fallback.
    pub(super) fn decline_reason(&self) -> &'static str {
        match self {
            Self::Rename { .. } => "intent-rename-unsolved",
            Self::Constant { .. } => "intent-constant-unsolved",
            Self::FormatRust | Self::FormatGo | Self::Format => "intent-format-unsolved",
            Self::DependencyUpdate { .. } => "intent-dependency-unsolved",
        }
    }
}

/// Identifiers the fast path refuses to touch: Rust keywords would
/// produce parsing games no mechanical rename should play.
const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "fn", "for", "if", "impl",
    "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static",
    "struct", "super", "trait", "type", "unsafe", "use", "where", "while",
];

/// Task phrases (after the optional `please ` prefix, case-insensitive)
/// that mean "run the formatter".
const FORMAT_PHRASES: &[&str] = &[
    "rustfmt",
    "run rustfmt",
    "apply rustfmt",
    "cargo fmt",
    "run cargo fmt",
    "apply cargo fmt",
    "cargo rustfmt",
    "format the rust code",
    "run rustfmt on the codebase",
    "apply rustfmt to the codebase",
    "rustfmt the codebase",
];

/// Ecosystem-neutral formatting phrases: the manifest decides which
/// formatter runs.
const GENERIC_FORMAT_PHRASES: &[&str] = &[
    "format the code",
    "format the codebase",
    "format the crate",
    "format the project",
];

/// gofmt-specific phrases.
const GO_FORMAT_PHRASES: &[&str] = &[
    "run gofmt",
    "apply gofmt",
    "gofmt the codebase",
    "gofmt the code",
    "format the go code",
];

/// Parses an identifier: `[A-Za-z_][A-Za-z0-9_]*`, returning it with the
/// unconsumed rest.
pub(super) fn parse_ident(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let is_head = |byte: u8| byte.is_ascii_alphabetic() || byte == b'_';
    let is_tail = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    if bytes.first().is_none_or(|byte| !is_head(*byte)) {
        return None;
    }
    let end = bytes
        .iter()
        .position(|byte| !is_tail(*byte))
        .unwrap_or(bytes.len());
    Some((&text[..end], &text[end..]))
}

/// Parses a literal: integer, boolean, or plain double-quoted string.
/// Integer consumption is explicit digit scanning — identifier parsing
/// would happily stop at `3` inside `3.5` and misreport the span.
pub(super) fn parse_literal(text: &str) -> Option<(Literal, &str)> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        // No escape handling in v1: a backslash means the instruction
        // wants a literal shape the round-trip cannot guarantee.
        let value = &rest[..end];
        if value.contains('\\') {
            return None;
        }
        return Some((Literal::Str(value.to_owned()), &rest[end + 1..]));
    }
    let negative = trimmed.starts_with('-');
    let digits_text = if negative { &trimmed[1..] } else { trimmed };
    let end = digits_text
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits_text.len());
    if end > 0 {
        let digits = &digits_text[..end];
        let rendered = if negative {
            format!("-{digits}")
        } else {
            digits.to_owned()
        };
        return Some((Literal::Int(rendered), &digits_text[end..]));
    }
    for word in ["true", "false"] {
        if let Some(rest) = trimmed.strip_prefix(word)
            && (rest.is_empty()
                || rest.starts_with(|byte: char| !byte.is_ascii_alphanumeric() && byte != '_'))
        {
            return Some((Literal::Bool(word == "true"), rest));
        }
    }
    None
}

/// Strips one optional, case-insensitive keyword prefix followed by a
/// space; returns the rest of the original-cased text.
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> &'a str {
    if text.len() >= keyword.len() && text[..keyword.len()].eq_ignore_ascii_case(keyword) {
        &text[keyword.len()..]
    } else {
        text
    }
}

/// Strips an optional grammar noun ("the ", "function ", …).
fn strip_noun(text: &str) -> &str {
    let mut rest = strip_keyword(text, "the ");
    for noun in [
        "function",
        "variable",
        "struct",
        "enum",
        "constant",
        "method",
        "trait",
        "module",
        "identifier",
    ] {
        let after = strip_keyword(rest, &format!("{noun} "));
        if after.len() != rest.len() {
            rest = after;
            break;
        }
    }
    rest
}

/// Strips optional backticks around an already-parsed span boundary:
/// given the remaining text, drops a leading and trailing backtick pair.
fn strip_ticks(text: &str) -> &str {
    text.trim_matches('`')
}

/// Parses a crate name token: `[A-Za-z0-9_-]+` (Cargo dependency keys,
/// hyphens included), returning it with the unconsumed rest.
pub(super) fn parse_crate_name(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let end = bytes
        .iter()
        .position(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-'))
        .unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    Some((&text[..end], &text[end..]))
}

/// Parses a version-requirement token: a leading digit then
/// `[0-9A-Za-z.+-]`, at most 64 bytes. Wildcards (`*`), carets, and
/// `latest` fail the leading-digit check and are declined.
fn parse_version(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let end = bytes
        .iter()
        .position(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')))
        .unwrap_or(bytes.len());
    if end > 64 {
        return None;
    }
    Some((&text[..end], &text[end..]))
}

/// Recognizes the strict fast-path grammars over the whole task text.
pub(super) fn analyze(task: &str) -> Option<Intent> {
    let text = task.trim().trim_end_matches('.');
    let text = {
        let lowered = text.to_ascii_lowercase();
        if lowered.starts_with("please ") {
            &text[7..]
        } else {
            text
        }
    };

    parse_rename_phrase(text)
        .or_else(|| parse_change_phrase(text))
        .or_else(|| parse_set_phrase(text))
        .or_else(|| parse_dependency_phrase(text))
        .or_else(|| match_format_phrases(text))
}

fn match_format_phrases(text: &str) -> Option<Intent> {
    if FORMAT_PHRASES
        .iter()
        .any(|phrase| text.eq_ignore_ascii_case(phrase))
    {
        return Some(Intent::FormatRust);
    }
    if GO_FORMAT_PHRASES
        .iter()
        .any(|phrase| text.eq_ignore_ascii_case(phrase))
    {
        return Some(Intent::FormatGo);
    }
    if GENERIC_FORMAT_PHRASES
        .iter()
        .any(|phrase| text.eq_ignore_ascii_case(phrase))
    {
        return Some(Intent::Format);
    }
    None
}

/// Parses the rename phrase: rename [the][noun] FROM to TO.
fn parse_rename_phrase(text: &str) -> Option<Intent> {
    if text.len() < 7 || !text[..7].eq_ignore_ascii_case("rename ") {
        return None;
    }
    let rest = strip_noun(&text[7..]);
    let rest = strip_ticks(rest.trim_start());
    let (from, rest) = parse_ident(rest)?;
    let rest = strip_ticks(rest);
    let rest = rest.trim_start();
    if rest.len() < 3 || !rest[..2].eq_ignore_ascii_case("to") {
        return None;
    }
    let after_to = &rest[2..];
    if !(after_to.starts_with(' ') || after_to.starts_with('`')) {
        return None;
    }
    let rest = after_to.trim_start();
    let rest = strip_noun(rest);
    let rest = strip_ticks(rest.trim_start());
    let (to, rest) = parse_ident(rest)?;
    let rest = strip_ticks(rest).trim_start();
    if !rest.is_empty() {
        return None;
    }
    if from == to || KEYWORDS.contains(&from) || KEYWORDS.contains(&to) {
        return None;
    }
    Some(Intent::Rename {
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

/// Parses `change [the][constant] NAME from LIT to LIT`.
fn parse_change_phrase(text: &str) -> Option<Intent> {
    if text.len() < 7 || !text[..7].eq_ignore_ascii_case("change ") {
        return None;
    }
    let rest = strip_noun(&text[7..]);
    let (name, rest) = parse_ident(rest.trim_start())?;
    let rest = rest.trim_start();
    let lowered = rest.to_ascii_lowercase();
    if !lowered.starts_with("from ") {
        return None;
    }
    let (from, rest) = parse_literal(&rest[5..])?;
    let rest = rest.trim_start();
    let lowered = rest.to_ascii_lowercase();
    if !lowered.starts_with("to ") {
        return None;
    }
    let (to, rest) = parse_literal(&rest[3..])?;
    if !rest.trim().is_empty() || KEYWORDS.contains(&name) {
        return None;
    }
    Some(Intent::Constant {
        name: name.to_owned(),
        from: Some(from),
        to,
    })
}

/// Parses `set [the][constant] NAME to LIT`.
fn parse_set_phrase(text: &str) -> Option<Intent> {
    if text.len() < 4 || !text[..4].eq_ignore_ascii_case("set ") {
        return None;
    }
    let rest = strip_noun(&text[4..]);
    let (name, rest) = parse_ident(rest.trim_start())?;
    let rest = rest.trim_start();
    let lowered = rest.to_ascii_lowercase();
    if !lowered.starts_with("to ") {
        return None;
    }
    let (to, rest) = parse_literal(&rest[3..])?;
    if !rest.trim().is_empty() || KEYWORDS.contains(&name) {
        return None;
    }
    Some(Intent::Constant {
        name: name.to_owned(),
        from: None,
        to,
    })
}

/// Parses the update/bump phrase: update|bump [the][dependency|crate] NAME to VERSION.
fn parse_dependency_phrase(text: &str) -> Option<Intent> {
    for verb in ["update", "bump"] {
        let prefix = format!("{verb} ");
        if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            let rest = strip_keyword(&text[prefix.len()..], "the ");
            let rest = strip_keyword(rest, "dependency ");
            let rest = strip_keyword(rest, "crate ");
            let rest = strip_ticks(rest.trim_start());
            let (crate_name, rest) = parse_crate_name(rest)?;
            let rest = strip_ticks(rest).trim_start();
            let lowered = rest.to_ascii_lowercase();
            if !lowered.starts_with("to ") {
                return None;
            }
            let after = strip_ticks(rest[3..].trim_start());
            let (version, rest) = parse_version(after)?;
            if !rest.trim().is_empty() {
                return None;
            }
            return Some(Intent::DependencyUpdate {
                crate_name: crate_name.to_owned(),
                to_version: version.to_owned(),
            });
        }
    }
    None
}
