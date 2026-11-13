//! Ingestion hygiene: invisible-character stripping, description caps,
//! cross-server reference rejection, imperative-phrase flagging, and
//! SHA-256 pinning of tool definitions.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Hygiene
// ---------------------------------------------------------------------------

/// Whether a character is stripped from server-provided text: C0/C1
/// control characters and the format (invisible) characters used to hide
/// instructions or exfiltration channels — zero-width, bidi, BOM, soft
/// hyphen (Invariant Labs' tool-poisoning vectors). Shared with the
/// result-channel stripper in [`crate::hygiene`].
use crate::hygiene::is_invisible;

use super::DESC_MAX_CHARS;

/// Strips invisible characters and collapses whitespace runs to single
/// spaces — the deterministic normal form all ingest checks run against.
#[must_use]
pub fn sanitize_text(raw: &str) -> String {
    let stripped: String = raw.chars().filter(|c| !is_invisible(*c)).collect();
    let mut out = String::with_capacity(stripped.len());
    let mut in_space = false;
    for c in stripped.chars() {
        if c.is_whitespace() {
            in_space = true;
        } else {
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(c);
        }
    }
    out
}

/// Caps a sanitized description at [`DESC_MAX_CHARS`] on a char boundary.
#[must_use]
pub fn truncate_description(desc: &str) -> String {
    if desc.chars().count() <= DESC_MAX_CHARS {
        return desc.to_owned();
    }
    let mut capped: String = desc.chars().take(DESC_MAX_CHARS).collect();
    capped.push_str(" …[truncated by loom]");
    capped
}

/// Whether a description references another declared server or its
/// tools — the tool-shadowing signature (mcp-scan E002): one server's
/// description redirecting the agent toward another server's surface.
pub(super) fn contains_cross_server_reference(desc: &str, own: &str, others: &[String]) -> bool {
    let haystack = desc.to_lowercase();
    others
        .iter()
        .filter(|other| other.as_str() != own)
        .any(|other| haystack.contains(&other.to_lowercase()))
}

/// Phrases that mark a description as issuing instructions to the agent
/// rather than describing a tool (mcp-scan E003 class).
const IMPERATIVE_PHRASES: [&str; 14] = [
    "ignore previous",
    "ignore all previous",
    "ignore above",
    "disregard previous",
    "disregard above",
    "you must",
    "you should always",
    "do not tell",
    "don't tell",
    "do not reveal",
    "don't reveal",
    "before you respond",
    "without informing the user",
    "exfiltrate",
];

/// Whether the description contains agent-directed imperative phrasing.
pub(super) fn contains_imperative(desc: &str) -> bool {
    let haystack = desc.to_lowercase();
    IMPERATIVE_PHRASES
        .iter()
        .any(|phrase| haystack.contains(phrase))
}

/// Converts a `*`/`?` glob into an anchored regex string.
pub(super) fn glob_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

/// SHA-256 pin over the canonical tool identity: the server's spawn
/// argv (binary and arguments — a swapped server binary must
/// quarantine like a swapped tool definition, review item 2.5),
/// server, name, sanitized description, compact schema, joined by
/// unit separators.
pub(super) fn pin_hash(
    server: &str,
    argv: &[String],
    name: &str,
    desc: &str,
    schema: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(server.as_bytes());
    hasher.update([0x1f]);
    for part in argv {
        hasher.update(part.as_bytes());
        hasher.update([0x1f]);
    }
    for part in [name, desc, schema] {
        hasher.update(part.as_bytes());
        hasher.update([0x1f]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Warns when the server argv uses a fetch-and-run runner without a
/// version pin: `npx -y pkg` / `uvx pkg` resolve to *whatever the
/// registry serves today*, so the pinned hash guards a moving target.
pub(super) fn warn_if_unpinned_runner(name: &str, command: &[String]) {
    let Some(program) = command.first() else {
        return;
    };
    let runner = std::path::Path::new(program)
        .file_name()
        .map(|file| file.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !matches!(runner.as_str(), "npx" | "uvx" | "pnpx") {
        return;
    }
    let version_pinned = command
        .iter()
        .skip(1)
        .any(|arg| is_version_pinned_package(arg));
    if !version_pinned {
        tracing::warn!(
            server = name,
            argv = command.join(" "),
            "MCP server runs through {runner} without a version pin: the binary the lock hashes is whatever the registry serves today; pin the package (e.g. `pkg@1.2.3`)"
        );
    }
}

/// True for `pkg@1.2.3` (version-pinned) but not for `@scope/pkg`
/// (scope marker only) — the tail after the last `@` must start with
/// a digit.
pub(super) fn is_version_pinned_package(arg: &str) -> bool {
    let Some((head, tail)) = arg.rsplit_once('@') else {
        return false;
    };
    !head.is_empty() && tail.chars().next().is_some_and(|c| c.is_ascii_digit())
}
