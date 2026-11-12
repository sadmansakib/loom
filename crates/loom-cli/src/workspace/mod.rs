//! Workspace snapshots for the rewind surface (`docs/rewind-design.md`
//! §4 tiers 1–2 and §5 safety rule).
//!
//! Three operations, all pure process concerns (the engine never touches
//! git directly outside worktrees; this is the CLI adapter's seam):
//!
//! - [`snapshot_start`] runs at `loom run` start, before the task can
//!   touch the tree, and captures the workspace's git HEAD when — and
//!   only when — the checkout was clean. A dirty or non-git start is
//!   recorded as such, so a later restore refuses with the real reason
//!   instead of guessing (tier 0).
//! - [`plan_restore`] / [`apply_restore`] turn a recorded
//!   [`StartCheckpoint`] back into tree state (tier 1: `git reset` to
//!   the clean start). Refusal beats best-effort: every claim the
//!   restore makes is checked first, and the mutation writes a recovery
//!   snapshot before it resets anything (invariant 2).
//! - [`plan_boundary_restore`] / [`apply_boundary_restore`] restore to
//!   any transcript boundary from the per-turn pre-images tools capture
//!   at execution time (tier 2). No git is involved: every claim is a
//!   recorded effect, so the same restore works on a dirty checkout or
//!   a workspace with no repository at all.

use std::path::Path;

mod boundary;
mod restore;
mod snapshot;
#[cfg(test)]
mod tests;

pub use boundary::*;
pub use restore::*;
pub use snapshot::*;

/// Runs `git -C <root> <args>`, returning trimmed stdout.
///
/// The error message is git's stderr when it has one — the caller
/// surfaces it, and a git that is missing or unhappy should never be
/// papered over here. Every invocation is hardened
/// (`loom_core::engine::hostgit`): the repo's own config cannot name
/// programs for git to execute.
fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = std::process::Command::new("git")
        .args(loom_core::engine::hostgit::CONFIG_ARGS)
        .args(loom_core::engine::hostgit::neutralize_filters(root))
        .env(
            loom_core::engine::hostgit::ENV_NOSYSTEM.0,
            loom_core::engine::hostgit::ENV_NOSYSTEM.1,
        )
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("running git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or_default())
        } else {
            stderr
        })
    }
}
