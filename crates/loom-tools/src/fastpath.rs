//! The deterministic fast paths (roadmap item 3).
//!
//! A narrow class of mechanical instructions — identifier renames,
//! single-site constant changes, and codebase formatting — can be
//! solved with **zero** model calls: analyze the instruction, apply
//! the edits, verify green, and hand the proof to the engine to record.
//! The contract is *never worse than the model loop*:
//!
//! 1. The analyzer is strict — anchored grammars over the whole task
//!    text, exact-case identifiers, literal values that must match the
//!    workspace's current state. Any surprise (ambiguity, a colliding
//!    name, a stale "from" value) declines instead of guessing.
//! 2. Application is snapshot-guarded — every file's original bytes are
//!    captured before the first write, and any failure (I/O, parse,
//!    verification) restores them byte-exact before falling through to
//!    the agent loop, which starts from the untouched workspace.
//! 3. Verification is the workspace's own compiler: `cargo check`
//!    (and `cargo fmt --check` for the format path) must exit green
//!    before the solve is claimed. v1 is Cargo-only — the pattern
//!    classes are Rust-typed, and other ecosystems grow their own
//!    verifiers when their patterns arrive.
//!
//! The engine calls [`try_fast_path`] before spawning the agent turn
//! loop; `None` (no match, declined, or failed-and-reverted) means the
//! task runs exactly as it would have without this module.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_kernel::tool::ToolContext;
use tokio_util::sync::CancellationToken;

use crate::exec::{ExecLimits, run_bounded};
use crate::policy::CommandPolicy;
use crate::sandbox::{Sandbox, SandboxGrants};

/// What a successful fast path proved, for the transcript and events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathOutcome {
    /// The analyzer's classification: `"rename"`, `"constant"`, or
    /// `"format"`.
    pub kind: &'static str,
    /// Files the fast path rewrote before verification.
    pub files_changed: u32,
    /// Human-readable proof recorded as the task's assistant item:
    /// what the pattern matched, what changed, what verified it green.
    pub note: String,
}

/// A literal value as it can appear in a task instruction and in a
/// `const` declaration. Rendering is round-trip-exact: the token the
/// analyzer parsed from the instruction must equal the token in the
/// source, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Literal {
    Int(String),
    Bool(bool),
    Str(String),
}

impl Literal {
    fn render(&self) -> String {
        match self {
            Self::Int(text) => text.clone(),
            Self::Bool(value) => value.to_string(),
            Self::Str(value) => format!("{value:?}"),
        }
    }
}

/// One recognized mechanical instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Intent {
    Rename {
        from: String,
        to: String,
    },
    Constant {
        name: String,
        from: Option<Literal>,
        to: Literal,
    },
    Format,
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
    "format the code",
    "format the codebase",
    "format the crate",
    "format the rust code",
    "format the project",
    "run rustfmt on the codebase",
    "apply rustfmt to the codebase",
    "rustfmt the codebase",
];

/// Parses an identifier: `[A-Za-z_][A-Za-z0-9_]*`, returning it with the
/// unconsumed rest.
fn parse_ident(text: &str) -> Option<(&str, &str)> {
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
fn parse_literal(text: &str) -> Option<(Literal, &str)> {
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
/// given the remaining text, drops a leading and trailing '`' pair.
fn strip_ticks(text: &str) -> &str {
    text.trim_matches('`')
}

/// Recognizes the strict fast-path grammars over the whole task text.
fn analyze(task: &str) -> Option<Intent> {
    let text = task.trim().trim_end_matches('.');
    let text = {
        let lowered = text.to_ascii_lowercase();
        if lowered.starts_with("please ") {
            &text[7..]
        } else {
            text
        }
    };

    // rename [the][noun] `from` to `to`
    if text.len() >= 7 && text[..7].eq_ignore_ascii_case("rename ") {
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
        return Some(Intent::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    // change [the][constant] NAME from LIT to LIT
    if text.len() >= 7 && text[..7].eq_ignore_ascii_case("change ") {
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
        return Some(Intent::Constant {
            name: name.to_owned(),
            from: Some(from),
            to,
        });
    }

    // set [the][constant] NAME to LIT
    if text.len() >= 4 && text[..4].eq_ignore_ascii_case("set ") {
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
        return Some(Intent::Constant {
            name: name.to_owned(),
            from: None,
            to,
        });
    }

    if FORMAT_PHRASES
        .iter()
        .any(|phrase| text.eq_ignore_ascii_case(phrase))
    {
        return Some(Intent::Format);
    }

    None
}

/// One captured original file, for byte-exact restore on failure.
struct Snapshot {
    path: PathBuf,
    bytes: Vec<u8>,
}

/// Restores every captured file to its original bytes.
fn restore(snapshots: &[Snapshot]) {
    for snapshot in snapshots {
        if let Err(error) = std::fs::write(&snapshot.path, &snapshot.bytes) {
            tracing::warn!(
                path = %snapshot.path.display(),
                error = %error,
                "fast path revert failed"
            );
        }
    }
}

/// Collects the workspace's Rust source files: gitignore-aware walk,
/// with `target/` and dot-directories always skipped.
fn rust_files(workspace: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(workspace);
    builder.hidden(true).git_ignore(true).parents(false);
    let walker =
        builder.filter_entry(|entry| !(entry.depth() > 0 && entry.file_name() == "target"));
    let mut files = Vec::new();
    for entry in walker.build().flatten() {
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.path().extension().is_some_and(|ext| ext == "rs")
        {
            files.push(entry.into_path());
        }
    }
    files
}

/// Whether `needle` at `at` in `hay` stands alone (word boundaries both
/// sides). Identifiers are ASCII, so byte checks are sound.
fn word_bounded(hay: &str, needle: &str, at: usize) -> bool {
    let bytes = hay.as_bytes();
    let head = at
        .checked_sub(1)
        .is_none_or(|prev| !is_word_byte(bytes[prev]));
    let tail = bytes
        .get(at + needle.len())
        .is_none_or(|next| !is_word_byte(*next));
    head && tail
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Replaces whole-word occurrences of `from` with `to`, returning the
/// new text and the replacement count.
fn replace_word(text: &str, from: &str, to: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(from) {
        let at = cursor + found;
        if word_bounded(text, from, at) {
            out.push_str(&text[cursor..at]);
            out.push_str(to);
            cursor = at + from.len();
            count += 1;
        } else {
            let boundary = at + from.len();
            out.push_str(&text[cursor..boundary]);
            cursor = boundary;
        }
    }
    out.push_str(&text[cursor..]);
    (out, count)
}

/// Counts whole-word occurrences of `needle` in `text`.
fn count_word(text: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(needle) {
        let at = cursor + found;
        if word_bounded(text, needle, at) {
            count += 1;
        }
        cursor = at + needle.len();
    }
    count
}

/// Runs `cargo <args…>` under the same policy, sandbox, and bounds the
/// `run_command` tool uses; green exit means verified.
async fn cargo_ok(workspace: &Path, policy: &CommandPolicy, args: &[&str]) -> bool {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let rule = policy.rule("cargo");
    let limits = ExecLimits {
        stream_window: rule.and_then(|rule| rule.max_output_bytes),
        scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
    };
    let grants = SandboxGrants::from_rule(rule);
    let sandbox = Sandbox::resolve(workspace, grants);
    let ctx = ToolContext::new(
        workspace.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(300),
    );
    match run_bounded("cargo", &owned, &ctx, &limits, &sandbox).await {
        Ok(output) => output.status.success(),
        Err(error) => {
            tracing::debug!(error = %error, "fast path verification could not run");
            false
        }
    }
}

/// Applies the rename intent: collision-checked, whole-word, verified.
async fn apply_rename(
    workspace: &Path,
    policy: &CommandPolicy,
    from: &str,
    to: &str,
) -> Option<FastPathOutcome> {
    let files = rust_files(workspace);
    let mut touched: Vec<(PathBuf, String)> = Vec::new();
    let mut occurrences = 0;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let count = count_word(&text, from);
        if count == 0 {
            continue;
        }
        if count_word(&text, to) > 0 {
            // The target name already exists in a touched file: a
            // blind replace would shadow it. Decline.
            return None;
        }
        occurrences += count;
        touched.push((path.clone(), text));
    }
    if occurrences == 0 || occurrences > 500 {
        return None;
    }
    // A pre-existing target name anywhere in the workspace is ambiguity,
    // not just in the touched files.
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if count_word(&text, to) > 0 {
            return None;
        }
    }
    let snapshots: Vec<Snapshot> = touched
        .iter()
        .filter_map(|(path, _)| {
            std::fs::read(path).ok().map(|bytes| Snapshot {
                path: path.clone(),
                bytes,
            })
        })
        .collect();
    for (path, text) in &touched {
        let (rewritten, _) = replace_word(text, from, to);
        if std::fs::write(path, rewritten).is_err() {
            restore(&snapshots);
            return None;
        }
    }
    if !cargo_ok(workspace, policy, &["check"]).await {
        restore(&snapshots);
        return None;
    }
    Some(FastPathOutcome {
        kind: "rename",
        files_changed: touched.len() as u32,
        note: format!(
            "Fast path (rename): `{from}` → `{to}` across {} file(s), \
             {occurrences} occurrence(s); verified with `cargo check`.",
            touched.len()
        ),
    })
}

/// Applies the constant intent at its unique declaration site.
async fn apply_constant(
    workspace: &Path,
    policy: &CommandPolicy,
    name: &str,
    from: &Option<Literal>,
    to: &Literal,
) -> Option<FastPathOutcome> {
    let files = rust_files(workspace);
    let mut sites: Vec<(PathBuf, String, usize, usize, Literal)> = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // CRLF files are declined: `str::lines` folds `\r` invisibly,
        // so byte offsets derived from line lengths would drift.
        if text.contains("\r\n") {
            continue;
        }
        // Each line can hold at most one `const NAME` declaration in v1;
        // multi-line declarations are declined (the literal must be on
        // the same line as the '=' for the round-trip to be checkable).
        let mut byte_offset = 0;
        for line in text.lines() {
            if let Some(site) = const_on_line(line, name) {
                sites.push((
                    path.clone(),
                    text.clone(),
                    byte_offset + site.0,
                    byte_offset + site.1,
                    site.2,
                ));
            }
            byte_offset += line.len() + 1;
        }
    }
    if sites.len() != 1 {
        // Zero or multiple declaration sites: ambiguity the fast path
        // does not resolve.
        return None;
    }
    let (path, text, literal_start, literal_end, current) = sites.pop()?;
    if let Some(expected) = from
        && expected.render() != current.render() {
            // The instruction's "from" value disagrees with the source:
            // stale instruction or moved code — decline.
            return None;
        }
    let snapshot = Snapshot {
        path: path.clone(),
        bytes: std::fs::read(&path).ok()?,
    };
    let rewritten = format!(
        "{}{}{}",
        &text[..literal_start],
        to.render(),
        &text[literal_end..]
    );
    if std::fs::write(&path, rewritten).is_err() {
        restore(std::slice::from_ref(&snapshot));
        return None;
    }
    if !cargo_ok(workspace, policy, &["check"]).await {
        restore(std::slice::from_ref(&snapshot));
        return None;
    }
    Some(FastPathOutcome {
        kind: "constant",
        files_changed: 1,
        note: format!(
            "Fast path (constant): `{name}` {} → {} in {}; verified with \
             `cargo check`.",
            current.render(),
            to.render(),
            path.strip_prefix(workspace).unwrap_or(&path).display()
        ),
    })
}

/// Finds `const NAME: … = LIT` on one line, returning the literal's
/// byte range within the line and its parsed value.
fn const_on_line(line: &str, name: &str) -> Option<(usize, usize, Literal)> {
    let mut cursor = 0;
    while let Some(found) = line[cursor..].find("const") {
        let at = cursor + found;
        cursor = at + 5;
        if !word_bounded(line, "const", at) {
            continue;
        }
        let rest = line[cursor..].trim_start();
        let (candidate, after) = parse_ident(rest)?;
        if candidate != name {
            continue;
        }
        let after_name = after.trim_start();
        if !after_name.starts_with(':') {
            // Not a typed const declaration (e.g. `const fn`).
            continue;
        }
        let equals = after_name.find('=')?;
        let value_text = after_name[equals + 1..].trim_start();
        let value_offset = line.len() - value_text.len();
        let (literal, rest) = parse_literal(value_text)?;
        if !rest.trim_start().starts_with(';') {
            continue;
        }
        return Some((value_offset, value_offset + literal.render().len(), literal));
    }
    None
}

/// Applies the format intent: `cargo fmt`, then format-idempotence and
/// compile verification.
async fn apply_format(workspace: &Path, policy: &CommandPolicy) -> Option<FastPathOutcome> {
    if crate::verify::detect_ecosystem(workspace) != Some(crate::verify::Ecosystem::Cargo) {
        return None;
    }
    // Pre-check names the files fmt would touch, so the snapshot is
    // exactly the revert set.
    let check = cargo_output(workspace, policy, &["fmt", "--check"]).await?;
    let unformatted = check
        .lines()
        .filter(|line| line.starts_with("Diff in "))
        .count() as u32;
    if unformatted == 0 {
        return Some(FastPathOutcome {
            kind: "format",
            files_changed: 0,
            note: "Fast path (format): the codebase is already \
                   rustfmt-clean; nothing to change. Verified with \
                   `cargo fmt --check`."
                .to_owned(),
        });
    }
    let snapshots: Vec<Snapshot> = rust_files(workspace)
        .into_iter()
        .filter_map(|path| {
            std::fs::read(&path)
                .ok()
                .map(|bytes| Snapshot { path, bytes })
        })
        .collect();
    if !cargo_ok(workspace, policy, &["fmt"]).await {
        restore(&snapshots);
        return None;
    }
    let green = cargo_ok(workspace, policy, &["fmt", "--check"]).await
        && cargo_ok(workspace, policy, &["check"]).await;
    if !green {
        restore(&snapshots);
        return None;
    }
    Some(FastPathOutcome {
        kind: "format",
        files_changed: unformatted,
        note: format!(
            "Fast path (format): `cargo fmt` applied to {unformatted} file(s); \
             verified with `cargo fmt --check` and `cargo check`."
        ),
    })
}

/// Runs `cargo <args…>` capturing stdout, when the run exits at all.
async fn cargo_output(workspace: &Path, policy: &CommandPolicy, args: &[&str]) -> Option<String> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let rule = policy.rule("cargo");
    let limits = ExecLimits {
        stream_window: rule.and_then(|rule| rule.max_output_bytes),
        scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
    };
    let grants = SandboxGrants::from_rule(rule);
    let sandbox = Sandbox::resolve(workspace, grants);
    let ctx = ToolContext::new(
        workspace.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(300),
    );
    let output = run_bounded("cargo", &owned, &ctx, &limits, &sandbox)
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Analyzes the task and, on a strict match, applies and verifies the
/// mechanical solve. `None` — no grammar match, a declined guard, or a
/// failure whose writes were reverted — means "run the agent loop".
///
/// Never-worse contract: on every `None` path the workspace is
/// byte-identical to what it was on entry.
pub async fn try_fast_path(
    workspace: &Path,
    task: &str,
    policy: &CommandPolicy,
) -> Option<FastPathOutcome> {
    match analyze(task)? {
        Intent::Format => apply_format(workspace, policy).await,
        Intent::Rename { from, to } => apply_rename(workspace, policy, &from, &to).await,
        Intent::Constant { name, from, to } => {
            apply_constant(workspace, policy, &name, &from, &to).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename(from: &str, to: &str) -> Option<Intent> {
        Some(Intent::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        })
    }

    #[test]
    fn analyze_rename_grammar() {
        assert_eq!(
            analyze("Rename parse_opts to parse_options"),
            rename("parse_opts", "parse_options")
        );
        assert_eq!(
            analyze("please rename the function `parse_opts` to `parse_options`."),
            rename("parse_opts", "parse_options")
        );
        assert_eq!(
            analyze("RENAME the variable count to total"),
            rename("count", "total")
        );
    }

    #[test]
    fn analyze_rename_rejects_noise() {
        assert_eq!(
            analyze("rename parse_opts to parse_options and add tests"),
            None
        );
        assert_eq!(analyze("rename fn to match"), None);
        assert_eq!(analyze("rename parse_opts to parse_opts"), None);
        assert_eq!(analyze("renamed parse_opts to parse_options"), None);
    }

    #[test]
    fn analyze_constant_grammar() {
        assert_eq!(
            analyze("Change MAX_RETRIES from 3 to 5"),
            Some(Intent::Constant {
                name: "MAX_RETRIES".to_owned(),
                from: Some(Literal::Int("3".to_owned())),
                to: Literal::Int("5".to_owned()),
            })
        );
        assert_eq!(
            analyze("please set the constant GREETING to \"hi\"."),
            Some(Intent::Constant {
                name: "GREETING".to_owned(),
                from: None,
                to: Literal::Str("hi".to_owned()),
            })
        );
        assert_eq!(
            analyze("set ENABLED to true"),
            Some(Intent::Constant {
                name: "ENABLED".to_owned(),
                from: None,
                to: Literal::Bool(true),
            })
        );
        assert_eq!(
            analyze("change OFFSET from -1 to -2"),
            Some(Intent::Constant {
                name: "OFFSET".to_owned(),
                from: Some(Literal::Int("-1".to_owned())),
                to: Literal::Int("-2".to_owned()),
            })
        );
    }

    #[test]
    fn analyze_constant_rejects_noise() {
        assert_eq!(analyze("change everything from broken to working"), None);
        assert_eq!(analyze("set the table for dinner"), None);
        assert_eq!(
            analyze("change MAX_RETRIES from 3 to 5 and update docs"),
            None
        );
    }

    #[test]
    fn analyze_format_grammar() {
        assert_eq!(analyze("run rustfmt"), Some(Intent::Format));
        assert_eq!(analyze("cargo fmt"), Some(Intent::Format));
        assert_eq!(analyze("Please format the codebase."), Some(Intent::Format));
        assert_eq!(analyze("format the disk"), None);
    }

    #[test]
    fn replace_word_respects_boundaries() {
        let (text, count) = replace_word("let a = 1; let ab = 2; // alpha", "a", "z");
        assert_eq!(count, 1);
        assert_eq!(text, "let z = 1; let ab = 2; // alpha");
        let (text, count) = replace_word(
            "parse_opts(); parse_opts_x(); parse_opts()",
            "parse_opts",
            "new_name",
        );
        assert_eq!(count, 2);
        assert_eq!(text, "new_name(); parse_opts_x(); new_name()");
    }

    #[test]
    fn literal_round_trip() {
        assert_eq!(
            parse_literal("42;"),
            Some((Literal::Int("42".to_owned()), ";"))
        );
        assert_eq!(
            parse_literal("\"hi\" rest"),
            Some((Literal::Str("hi".to_owned()), " rest"))
        );
        assert_eq!(parse_literal("true,"), Some((Literal::Bool(true), ",")));
        assert_eq!(parse_literal("truely"), None);
        assert_eq!(parse_literal("\"esc\\\"aped\""), None);
    }

    #[test]
    fn const_on_line_finds_the_literal() {
        let line = "pub const MAX_RETRIES: u32 = 3;";
        let (start, end, literal) = const_on_line(line, "MAX_RETRIES").unwrap();
        assert_eq!(&line[start..end], "3");
        assert_eq!(literal, Literal::Int("3".to_owned()));
        assert!(const_on_line(line, "OTHER").is_none());
        assert!(const_on_line("const fn helper() {}", "helper").is_none());
        let line = "    const GREETING: &str = \"hello\";";
        let (start, end, literal) = const_on_line(line, "GREETING").unwrap();
        assert_eq!(&line[start..end], "\"hello\"");
        assert_eq!(literal, Literal::Str("hello".to_owned()));
    }
}
