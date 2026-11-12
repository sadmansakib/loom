//! Tier-1 restore to the clean session-start commit.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use loom_core::kernel::store::StartCheckpoint;

use super::run_git;

/// A restore that passed every provability check.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// The recorded workspace (from the checkpoint, not the caller's
    /// cwd — a rewind may run from anywhere).
    pub workspace: PathBuf,
    /// The commit `git reset --hard` will target.
    pub head: String,
}

/// Checks everything a tier-1 restore claims before touching anything
/// (invariant 2: refusal beats best-effort).
///
/// Refuses, with the checkpoint's own reason, when no restore point was
/// ever provable. Refuses when the recorded workspace no longer exists,
/// is no longer a git work tree, or the recorded commit is no longer
/// present locally (reset cannot invent objects).
pub fn plan_restore(checkpoint: &StartCheckpoint) -> Result<RestorePlan> {
    let Some(head) = checkpoint.head.clone() else {
        bail!(
            "no provable workspace state to restore: {}",
            checkpoint
                .reason
                .as_deref()
                .unwrap_or("no start checkpoint head")
        );
    };
    if !checkpoint.workspace.is_dir() {
        bail!(
            "recorded workspace {} no longer exists",
            checkpoint.workspace.display()
        );
    }
    if !matches!(
        run_git(
            &checkpoint.workspace,
            &["rev-parse", "--is-inside-work-tree"]
        )
        .as_deref(),
        Ok("true")
    ) {
        bail!(
            "recorded workspace {} is no longer a git work tree",
            checkpoint.workspace.display()
        );
    }
    if let Err(error) = run_git(
        &checkpoint.workspace,
        &["cat-file", "-e", &format!("{head}^{{commit}}")],
    ) {
        bail!(
            "start commit {head} is not available in {} ({error}); \
             it may have been garbage-collected",
            checkpoint.workspace.display()
        );
    }
    Ok(RestorePlan {
        workspace: checkpoint.workspace.clone(),
        head,
    })
}

/// Writes the pre-restore recovery snapshot and applies the plan:
/// `git reset --hard <head>` + `git clean -fd`, excluding `.loom` so
/// the snapshot itself survives the clean.
///
/// The snapshot stages everything (`git add -A`) and writes `git diff
/// --cached` to `<workspace>/.loom/rewind/<millis>.patch` — the full
/// content of what the restore is about to discard, kept indefinitely
/// (never garbage-collected with run artifacts). Returns its path.
pub fn apply_restore(plan: &RestorePlan) -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let rewind_dir = plan.workspace.join(".loom").join("rewind");
    std::fs::create_dir_all(&rewind_dir)
        .with_context(|| format!("creating {}", rewind_dir.display()))?;
    // Review item 2.8: recovery snapshots are machine-local state; a
    // blanket `git add -A` (by the model or the operator) must not
    // commit them. Local excludes only — the tracked .gitignore is
    // never touched.
    loom_core::engine::worktrees::exclude_state_from_git(&plan.workspace, &[".loom/rewind/"]);
    let patch = rewind_dir.join(format!("{timestamp}.patch"));

    run_git(&plan.workspace, &["add", "-A"])
        .map_err(|error| anyhow::anyhow!("staging for the recovery snapshot: {error}"))?;
    let diff = match run_git(
        &plan.workspace,
        // `--no-ext-diff --no-textconv`: the cached diff feeds the
        // rewind snapshot, and both flags can name repo-config
        // *programs* (`diff.<driver>.command`) — the one diff site
        // must not become a config-driven exec (see `hostgit`).
        &["diff", "--cached", "--no-ext-diff", "--no-textconv"],
    ) {
        Ok(diff) => diff,
        // Staging happened; undo it so a failed snapshot leaves the
        // index as it was (reset --hard would rewrite it anyway, but a
        // refusal should not leave side effects).
        Err(error) => {
            let _ = run_git(&plan.workspace, &["reset"]);
            bail!("capturing the recovery snapshot: {error}");
        }
    };
    std::fs::write(&patch, &diff).with_context(|| format!("writing {}", patch.display()))?;

    if let Err(error) = run_git(&plan.workspace, &["reset", "--hard", &plan.head]) {
        let _ = run_git(&plan.workspace, &["reset"]);
        bail!(
            "resetting {} to {} failed after the recovery snapshot at {} was written: {error}",
            plan.workspace.display(),
            plan.head,
            patch.display()
        );
    }
    if let Err(error) = run_git(&plan.workspace, &["clean", "-fd", "-e", ".loom"]) {
        bail!(
            "tree was reset to {} but cleaning untracked files failed: {error} \
             (recovery snapshot at {})",
            plan.head,
            patch.display()
        );
    }
    Ok(patch)
}
