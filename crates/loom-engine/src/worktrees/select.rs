//! Deterministic selection: the rank key, the winner pick, and the item 29
//! execution-based pre-filter.

use std::path::Path;
use std::sync::Arc;

use loom_tools::policy::CommandPolicy;
use loom_tools::verify::{VerifyTool, detect_ecosystem};
use tokio::sync::Semaphore;

use super::config::RolloutReport;
/// The deterministic rank key (item 21): completion first, then fewest
/// markers, then fewest turns, then fewest model requests. The rollout
/// index breaks any remaining tie, so selection is reproducible from the
/// report alone.
pub(super) fn rank_key(run: &RolloutReport) -> (bool, u64, u32, u32) {
    (
        !matches!(
            run.end_reason.as_deref(),
            Some("end_turn") | Some("fast_path")
        ),
        run.markers,
        run.summary.as_ref().map(|s| s.turns).unwrap_or(u32::MAX),
        run.summary
            .as_ref()
            .map(|s| s.model_requests)
            .unwrap_or(u32::MAX),
    )
}

/// Picks the winner: candidates need a non-empty patch and must not have
/// failed the item 29 selection suite; ranking is the [`rank_key`] order
/// with the rollout index as the final tie-break.
pub(super) fn winner_index(runs: &[RolloutReport]) -> Option<usize> {
    runs.iter()
        .filter(|run| !run.patch.trim().is_empty() && run.suite_passed != Some(false))
        .min_by_key(|run| (rank_key(run), run.index))
        .map(|run| run.index)
}

/// Item 29 selection pass: run the fixed-table suite in every candidate
/// worktree that produced a patch, record the verdict on the report, and
/// return the note for the selection record. `Some(false)` verdicts drop
/// the candidate in [`winner_index`]; `None` verdicts never do. The
/// rollout semaphore bounds suite concurrency exactly like the rollouts.
pub(super) async fn run_selection_suites(
    runs: &mut [RolloutReport],
    repo_root: &Path,
    semaphore: &Arc<Semaphore>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    // Repositories without a fixed-table suite stay on pure ranking; a
    // `.loom/verify.toml` runner is worktree-local state the parent
    // cannot rely on here, so detection is the parent's table alone.
    detect_ecosystem(repo_root)?;
    let policy = match CommandPolicy::from_manifest(repo_root) {
        Ok(policy) => policy,
        Err(_) => {
            return Some("execution filter skipped: policy manifest unreadable".to_owned());
        }
    };
    let mut tasks = Vec::new();
    for run in runs.iter() {
        if run.patch.trim().is_empty() {
            continue;
        }
        let permit_source = semaphore.clone();
        let worktree = run.worktree.clone();
        let tool = VerifyTool::with_policy(policy.clone());
        let token = cancellation.clone();
        tasks.push((
            run.index,
            tokio::spawn(async move {
                let _permit = permit_source.acquire_owned().await;
                tool.suite_verdict(&worktree, &token).await
            }),
        ));
    }
    let mut measured = 0usize;
    let mut dropped = 0usize;
    for (index, task) in tasks {
        // A join failure or missing verdict is "no evidence": the
        // candidate keeps its rank.
        let verdict = task.await.ok().flatten();
        if let Some(run) = runs.iter_mut().find(|run| run.index == index) {
            run.suite_passed = verdict;
        }
        measured += usize::from(verdict.is_some());
        dropped += usize::from(verdict == Some(false));
    }
    Some(format!(
        "execution filter: {measured} suite verdict(s), {dropped} candidate(s) dropped"
    ))
}
