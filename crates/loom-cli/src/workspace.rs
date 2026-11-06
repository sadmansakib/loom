//! Workspace snapshots for the rewind surface (`docs/rewind-design.md`
//! §4 tier 1 and §5 safety rule).
//!
//! Two operations, both pure process concerns (the engine never touches
//! git directly outside worktrees; this is the CLI adapter's seam):
//!
//! - [`snapshot_start`] runs at `loom run` start, before the task can
//!   touch the tree, and captures the workspace's git HEAD when — and
//!   only when — the checkout was clean. A dirty or non-git start is
//!   recorded as such, so a later restore refuses with the real reason
//!   instead of guessing (tier 0).
//! - [`plan_restore`] / [`apply_restore`] turn a recorded
//!   [`StartCheckpoint`] back into tree state. Refusal beats
//!   best-effort: every claim the restore makes is checked first, and
//!   the mutation writes a recovery snapshot before it resets anything
//!   (invariant 2).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use loom_core::kernel::store::StartCheckpoint;

/// Runs `git -C <root> <args>`, returning trimmed stdout.
///
/// The error message is git's stderr when it has one — the caller
/// surfaces it, and a git that is missing or unhappy should never be
/// papered over here.
fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = std::process::Command::new("git")
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

/// Captures the workspace state for a session-start checkpoint.
///
/// Clean checkout → `head` set, the exact tier-1 restore point. Dirty
/// or non-git → `head` absent with the reason; git itself being broken
/// or missing also degrades to a reason (the run proceeds, a later
/// `--restore-workspace` refuses honestly).
pub fn snapshot_start(workspace: &Path) -> StartCheckpoint {
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let inside = run_git(&canonical, &["rev-parse", "--is-inside-work-tree"]);
    let inside = matches!(inside.as_deref(), Ok("true"));
    if !inside {
        return StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some("workspace is not a git repository".to_owned()),
        };
    }
    let head = match run_git(&canonical, &["rev-parse", "--verify", "HEAD"]) {
        Ok(head) if !head.is_empty() => head,
        Ok(_) => {
            return StartCheckpoint {
                workspace: canonical,
                head: None,
                reason: Some("git repository has no commits".to_owned()),
            };
        }
        Err(error) => {
            return StartCheckpoint {
                workspace: canonical,
                head: None,
                reason: Some(format!("reading git HEAD failed: {error}")),
            };
        }
    };
    match run_git(&canonical, &["status", "--porcelain"]) {
        Ok(status) if status.is_empty() => StartCheckpoint {
            workspace: canonical,
            head: Some(head),
            reason: None,
        },
        // One line per change; the count keeps the refusal concrete
        // without dumping paths into a meta row.
        Ok(status) => StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some(format!(
                "workspace was dirty at session start ({} changed path{})",
                status.lines().count(),
                if status.lines().count() == 1 { "" } else { "s" }
            )),
        },
        Err(error) => StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some(format!("reading git status failed: {error}")),
        },
    }
}

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
    let patch = rewind_dir.join(format!("{timestamp}.patch"));

    run_git(&plan.workspace, &["add", "-A"])
        .map_err(|error| anyhow::anyhow!("staging for the recovery snapshot: {error}"))?;
    let diff = match run_git(&plan.workspace, &["diff", "--cached"]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A committed git repo in a temp dir with one tracked file, plus
    /// its HEAD sha.
    fn temp_repo(contents: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("file.txt"), contents).expect("write");
        for args in [
            vec!["init".to_owned()],
            vec![
                "config".to_owned(),
                "user.email".to_owned(),
                "t@loom.test".to_owned(),
            ],
            vec![
                "config".to_owned(),
                "user.name".to_owned(),
                "loom test".to_owned(),
            ],
            vec!["add".to_owned(), "-A".to_owned()],
            vec!["commit".to_owned(), "-m".to_owned(), "base".to_owned()],
        ] {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            run_git(root, &refs).unwrap_or_else(|error| panic!("{}: {error}", args[0]));
        }
        let head = run_git(root, &["rev-parse", "HEAD"]).expect("head");
        (dir, head)
    }

    #[test]
    fn snapshot_start_captures_head_on_a_clean_checkout() {
        let (dir, head) = temp_repo("base\n");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head.as_deref(), Some(head.as_str()));
        assert_eq!(checkpoint.reason, None);
        // Canonicalized: the checkpoint is a restore target, so it must
        // survive the caller passing a symlinked path (macOS /var).
        assert_eq!(
            checkpoint.workspace,
            std::fs::canonicalize(dir.path()).expect("canonicalize")
        );
    }

    #[test]
    fn snapshot_start_refuses_a_dirty_start_with_the_change_count() {
        let (dir, _head) = temp_repo("base\n");
        std::fs::write(dir.path().join("file.txt"), "dirty\n").expect("write");
        std::fs::write(dir.path().join("new.txt"), "untracked\n").expect("write");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head, None);
        let reason = checkpoint.reason.expect("reason");
        assert!(reason.contains("dirty"), "reason said: {reason}");
        assert!(reason.contains("2 changed paths"), "reason said: {reason}");
    }

    #[test]
    fn snapshot_start_refuses_a_workspace_without_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head, None);
        assert!(
            checkpoint
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not a git repository")),
            "reason said: {:?}",
            checkpoint.reason
        );
    }

    #[test]
    fn plan_restore_refuses_everything_it_cannot_prove() {
        let (dir, head) = temp_repo("base\n");

        let dirty = StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: None,
            reason: Some("workspace was dirty at session start (3 changed paths)".to_owned()),
        };
        let error = plan_restore(&dirty).expect_err("dirty checkpoint");
        assert!(
            error.to_string().contains("3 changed paths"),
            "error said: {error}"
        );

        let gone = StartCheckpoint {
            workspace: dir.path().join("gone"),
            head: Some(head.clone()),
            reason: None,
        };
        assert!(plan_restore(&gone).is_err(), "missing workspace");

        let missing_commit = StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: Some("0".repeat(40)),
            reason: None,
        };
        let error = plan_restore(&missing_commit).expect_err("bogus commit");
        assert!(
            error.to_string().contains("not available"),
            "error said: {error}"
        );
    }

    #[test]
    fn apply_restore_resets_the_tree_and_keeps_a_recovery_snapshot() {
        let (dir, head) = temp_repo("base\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write");
        std::fs::write(dir.path().join("untracked.txt"), "throwaway\n").expect("write");

        let plan = plan_restore(&StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: Some(head.clone()),
            reason: None,
        })
        .expect("plan");
        let patch = apply_restore(&plan).expect("restore");

        assert!(patch.is_file(), "snapshot at {}", patch.display());
        let snapshot = std::fs::read_to_string(&patch).expect("read snapshot");
        assert!(
            snapshot.contains("changed") && snapshot.contains("throwaway"),
            "snapshot should hold the discarded content"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt")).expect("read"),
            "base\n"
        );
        assert!(!dir.path().join("untracked.txt").exists());
        // The snapshot dir itself survives the clean (-e .loom).
        assert!(patch.exists());
        assert_eq!(
            run_git(dir.path(), &["rev-parse", "HEAD"]).expect("head"),
            head
        );
    }
}
