//! The shared exploratory prefix and fleet seeding (item 28).

use std::path::{Path, PathBuf};

use crate::engine::EngineConfig;
use loom_kernel::id::SessionId;

use super::config::{ProviderFactory, WorktreeError};
use super::git::git_with_stdin;
use super::rollout::{capture_patch, run_rollout};
/// Steering input that starts every shared-prefix fork. Identical across
/// children so the first forked request stays byte-stable (item 28).
pub(super) const SHARED_PREFIX_INPUT: &str = "Continue the task above to completion; the earlier turns already happened. Verify your work before finishing.";

/// A seeded shared prefix (item 28): the session every rollout continues
/// and the note the report shows for the once-paid spend.
pub(super) struct SharedPrefix {
    pub(super) session_id: SessionId,
    pub(super) note: String,
}

/// Runs the shared exploratory prefix (item 28) in worktree 0 under a
/// hard `turns` ceiling, then seeds the fleet: the prefix patch is applied
/// to every sibling so each starts from the prefix-final tree, and the
/// session database (WAL sidecars included) is copied so each child
/// continues the same transcript. Worktree 0 keeps running in place.
///
/// Stopping at the turn ceiling is the expected ending, not a failure; only
/// open/provider errors and cancellation abort the orchestration.
pub(super) async fn run_shared_prefix(
    turns: u32,
    worktrees: &[PathBuf],
    base_config: &EngineConfig,
    provider_factory: &ProviderFactory,
    task: &str,
    cancellation: &tokio_util::sync::CancellationToken,
    root: &Path,
) -> Result<SharedPrefix, WorktreeError> {
    let worktree = worktrees[0].clone();
    let mut config = base_config.clone();
    config.workspace = worktree.clone();
    config.db_path = worktree.join(".loom").join("loom.db");
    config.max_turns = turns;
    let (report, session_id) = run_rollout(
        0,
        worktree.clone(),
        config,
        provider_factory.clone(),
        task.to_owned(),
        cancellation,
        None,
        false,
    )
    .await;
    if cancellation.is_cancelled() {
        return Err(WorktreeError::new("shared prefix cancelled"));
    }
    if let Some(reason) = report.end_reason.as_deref()
        && (reason.starts_with("open_error") || reason.starts_with("provider_error"))
    {
        return Err(WorktreeError::new(format!(
            "shared prefix failed in worktree 0: {reason}"
        )));
    }
    let session_id = session_id
        .ok_or_else(|| WorktreeError::new("shared prefix produced no session to continue"))?;
    // Stage the session database before `capture_patch` deletes the
    // worktree's `.loom` directory; the WAL sidecars carry committed
    // turns that a dropped connection may not have checkpointed.
    let staged = root.join("prefix-db");
    stage_session_db(&worktree.join(".loom"), &staged).await?;
    let patch = capture_patch(&worktree).await;
    seed_worktree(&worktree, &staged, "").await?;
    for sibling in worktrees.iter().skip(1) {
        seed_worktree(sibling, &staged, &patch).await?;
    }
    Ok(SharedPrefix {
        session_id,
        note: format!(
            "session {session_id} (ceiling {turns}): {}",
            report.cost_note
        ),
    })
}

/// Copies the session database and its WAL sidecars from `loom_dir` into
/// `staged` so siblings can be seeded from one durable snapshot.
async fn stage_session_db(loom_dir: &Path, staged: &Path) -> Result<(), WorktreeError> {
    tokio::fs::create_dir_all(staged)
        .await
        .map_err(|err| WorktreeError::new(format!("create {}: {err}", staged.display())))?;
    for name in ["loom.db", "loom.db-wal", "loom.db-shm"] {
        let source = loom_dir.join(name);
        if !tokio::fs::try_exists(&source).await.unwrap_or(false) {
            // Sidecars are optional; the database itself is not.
            if name == "loom.db" {
                return Err(WorktreeError::new(format!(
                    "stage {} from {}",
                    name,
                    loom_dir.display()
                )));
            }
            continue;
        }
        let dest = staged.join(name);
        if let Err(err) = tokio::fs::copy(&source, &dest).await {
            return Err(WorktreeError::new(format!(
                "stage {} to {}: {err}",
                name,
                dest.display()
            )));
        }
    }
    Ok(())
}

/// Seeds one worktree: restores the session database (a missing `.loom`
/// in worktree 0 is expected after `capture_patch`) and applies the prefix
/// patch when there is one. Worktree 0 gets an empty patch — its tree
/// already carries the prefix changes.
pub(super) async fn seed_worktree(
    worktree: &Path,
    staged: &Path,
    patch: &str,
) -> Result<(), WorktreeError> {
    let loom_dir = worktree.join(".loom");
    tokio::fs::create_dir_all(&loom_dir)
        .await
        .map_err(|err| WorktreeError::new(format!("create {}: {err}", loom_dir.display())))?;
    for name in ["loom.db", "loom.db-wal", "loom.db-shm"] {
        let source = staged.join(name);
        if tokio::fs::try_exists(&source).await.unwrap_or(false) {
            let dest = loom_dir.join(name);
            tokio::fs::copy(&source, &dest).await.map_err(|err| {
                WorktreeError::new(format!(
                    "copy {} to {}: {err}",
                    source.display(),
                    dest.display()
                ))
            })?;
        }
    }
    if !patch.is_empty() {
        git_with_stdin(worktree, &["apply", "--whitespace=nowarn"], patch)
            .await
            .map_err(|err| {
                WorktreeError::new(format!(
                    "seed prefix patch in {}: {err}",
                    worktree.display()
                ))
            })?;
    }
    Ok(())
}
