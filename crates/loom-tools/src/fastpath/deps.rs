//! The dependency-update intent: rewrite plain `name = "old"` entries in
//! the root `Cargo.toml` dependency tables and verify with
//! `cargo metadata --no-deps --offline` (parse-validity is what a
//! no-network sandbox can prove).

use std::path::Path;

use crate::policy::CommandPolicy;
/// Applies the dependency-update intent to the workspace root
/// `Cargo.toml`: plain `name = "old"` entries in the three dependency
/// tables are rewritten, the manifest is re-parsed with
/// `cargo metadata --no-deps --offline`, and any failure reverts.
/// Complex entries (inline tables, `.workspace` inheritance, dotted
/// keys) are left untouched; a crate with no plain entry declines.
pub(super) async fn apply_dependency_update(
    workspace: &Path,
    policy: &CommandPolicy,
    crate_name: &str,
    to_version: &str,
) -> Option<FastPathOutcome> {
    if crate::verify::detect_ecosystem(workspace) != Some(crate::verify::Ecosystem::Cargo) {
        return None;
    }
    let manifest_path = workspace.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path).ok()?;
    // CRLF manifests are declined: line rebuilding assumes '\n' endings.
    if text.contains("\r\n") {
        return None;
    }
    let (rewritten, changes) = edit_manifest(&text, crate_name, to_version);
    if changes.len() > 64 {
        return None;
    }
    // Zero plain entries: the crate is absent from the dependency tables
    // or only present in complex shapes — either way, decline.
    if changes.is_empty() {
        return None;
    }
    // Every plain site already carries the target: a clean no-op solve,
    // verified by the manifest's current parse (the unchanged file).
    if changes.iter().all(|(old, new)| old == new) {
        return Some(FastPathOutcome {
            kind: "dependency",
            files_changed: 0,
            note: format!(
                "Fast path (dependency): `{crate_name}` is already at \
                 {to_version} in Cargo.toml; nothing to change."
            ),
        });
    }
    let snapshot = Snapshot {
        path: manifest_path.clone(),
        bytes: std::fs::read(&manifest_path).ok()?,
    };
    if std::fs::write(&manifest_path, &rewritten).is_err() {
        restore(std::slice::from_ref(&snapshot));
        return None;
    }
    if !cargo_ok(
        workspace,
        policy,
        &[
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ],
    )
    .await
    {
        restore(std::slice::from_ref(&snapshot));
        return None;
    }
    let old = changes
        .iter()
        .map(|(old, _)| old.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(FastPathOutcome {
        kind: "dependency",
        files_changed: 1,
        note: format!(
            "Fast path (dependency): `{crate_name}` {old} → {to_version} in \
             Cargo.toml ({} site(s)); verified with `cargo metadata \
             --no-deps --offline`.",
            changes.len()
        ),
    })
}

use super::grammar::parse_crate_name;
use super::verify::cargo_ok;
use super::workspace::{Snapshot, restore};

use super::FastPathOutcome;

/// Rewrites plain `name = "version"` entries for `crate_name` inside
/// `[dependencies]`, `[dev-dependencies]`, and `[build-dependencies]`
/// tables, returning the new text and the per-site `(old, new)` pairs.
/// Pure: snapshot/verify live in the caller. Target tables only —
/// `[package]`, `[features]`, `target.'…'.dependencies` and the rest are
/// passed through untouched, so an edit can never reach a non-dependency
/// key.
pub(super) fn edit_manifest(
    text: &str,
    crate_name: &str,
    to_version: &str,
) -> (String, Vec<(String, String)>) {
    let mut out = String::with_capacity(text.len());
    let mut changes = Vec::new();
    let mut in_dep_table = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let table = &trimmed[1..trimmed.len() - 1];
            in_dep_table = matches!(
                table,
                "dependencies" | "dev-dependencies" | "build-dependencies"
            );
            out.push_str(line);
            continue;
        }
        if in_dep_table
            && let Some((old, rewritten)) = rewrite_dep_line(trimmed, crate_name, to_version)
        {
            let indent_len = body.len() - body.trim_start().len();
            let indent = &body[..indent_len];
            out.push_str(indent);
            out.push_str(&rewritten);
            if line.ends_with('\n') {
                out.push('\n');
            }
            changes.push((old, to_version.to_owned()));
            continue;
        }
        out.push_str(line);
    }
    (out, changes)
}

/// Rewrites one dependency line when it is exactly `name = "version"`
/// (after trimming): returns the old version and the rewritten line.
/// Inline tables, dotted keys, and non-plain shapes return `None`.
pub(super) fn rewrite_dep_line(
    line: &str,
    crate_name: &str,
    to_version: &str,
) -> Option<(String, String)> {
    let (ident, rest) = parse_crate_name(line.trim_start())?;
    if ident != crate_name {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    let value = rest.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.len() > 64 {
        return None;
    }
    if !inner
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if !inner
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
    {
        return None;
    }
    Some((inner.to_owned(), format!("{crate_name} = \"{to_version}\"")))
}
