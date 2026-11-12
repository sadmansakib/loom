//! The constant intent: replace a literal at its unique declaration site
//! and prove it with `cargo check`.

use std::path::{Path, PathBuf};

use crate::policy::CommandPolicy;
/// Applies the constant intent at its unique declaration site.
pub(super) async fn apply_constant(
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
        && expected.render() != current.render()
    {
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

use super::grammar::{Literal, parse_ident, parse_literal};
use super::verify::cargo_ok;
use super::workspace::{Snapshot, restore, rust_files, word_bounded};

use super::FastPathOutcome;

/// Finds `const NAME: … = LIT` on one line, returning the literal's
/// byte range within the line and its parsed value.
pub(super) fn const_on_line(line: &str, name: &str) -> Option<(usize, usize, Literal)> {
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
