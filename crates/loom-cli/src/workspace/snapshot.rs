//! Session-start snapshot capture (tier 0).

use std::path::Path;

use loom_core::kernel::store::StartCheckpoint;

use super::run_git;

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
