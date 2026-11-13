//! The git half of the scan: branch and changed-file state, read
//! through the hardened host-git invocation seam.

use std::path::Path;
use std::process::Command;

use loom_kernel::context::GitState;

/// Reads branch and changed-file state from git, degrading to empty on any
/// failure (including `git` not being installed).
pub(super) fn git_state(root: &Path) -> GitState {
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|out| out.trim().to_owned())
        .filter(|branch| !branch.is_empty());
    let changed_files = run_git(root, &["status", "--porcelain"])
        .map(|out| parse_porcelain(&out))
        .unwrap_or_default();
    GitState::new(branch, changed_files)
}

/// Runs `git -C <root> <args>` and returns stdout on success.
///
/// Every invocation is hardened (see [`crate::hostgit`]): the repo's
/// own config cannot name programs for git to execute.
pub(super) fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(crate::hostgit::CONFIG_ARGS)
        .args(crate::hostgit::neutralize_filters(root))
        .env(
            crate::hostgit::ENV_NOSYSTEM.0,
            crate::hostgit::ENV_NOSYSTEM.1,
        )
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Extracts changed paths from `git status --porcelain` output.
///
/// Handles the rename form (`R  old -> new`), taking the destination, and
/// sorts/dedupes for determinism. Paths that git quotes (they contain `"`,
/// `\`, or control bytes — `core.quotepath=false` only stops *non-ASCII*
/// being quoted) are C-unquoted, so quoted and unquoted spellings of the
/// same path land on one entry.
pub(super) fn parse_porcelain(output: &str) -> Vec<String> {
    let mut paths: Vec<String> = output
        .lines()
        .filter_map(|line| {
            // `XY <path>`: the status is two fixed columns then a space.
            let rest = line.get(3..)?;
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let path = unquote_git_path(path.trim());
            (!path.is_empty()).then_some(path)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Decodes one git-quoted path: `"…"` with C escapes (`\t`, `\n`, `\r`,
/// `\"`, `\\`, `\NNN` octal naming raw bytes). Unquoted input passes through
/// unchanged. Octal escapes are bytes, not code points, so the result is
/// re-decoded as UTF-8; invalid sequences (from raw non-UTF-8 paths)
/// degrade via lossy conversion rather than a wrong path.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_owned();
    };
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    let mut buf = [0_u8; 4];
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('a') => out.push(0x07),
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('v') => out.push(0x0b),
            Some('"') => out.push(b'"'),
            // A lone trailing backslash is a literal backslash.
            Some('\\') | None => out.push(b'\\'),
            Some(first @ '0'..='7') => {
                // Exactly three octal digits name one raw byte.
                let mut value = first.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.next().and_then(|d| d.to_digit(8)) {
                        Some(digit) => value = value * 8 + digit,
                        None => break,
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                // three octal digits are ≤ 0o777, always a valid u8
                out.push(value as u8);
            }
            Some(other) => out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes()),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
