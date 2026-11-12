//! Orchestration data contracts: configs, reports, and the error type.

use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::EngineError;
use loom_kernel::agent::TaskSummary;
use loom_kernel::model::ModelProvider;
/// Builds one provider instance per rollout. Providers are moved into
/// engines, so N rollouts need N instances; the production caller resolves
/// a credential once and reconstructs the (cheap, built-in-table) adapter
/// per child, while tests script different models per index.
pub type ProviderFactory =
    Arc<dyn Fn(usize) -> Result<Box<dyn ModelProvider>, EngineError> + Send + Sync>;

/// Knobs for [`best_of_n`]. `n` clamps to `2..=8` (below two there is
/// nothing to select; above eight the token spend dwarfs the selection
/// lift at current prices). `concurrency` clamps to `1..=n`.
#[derive(Debug, Clone)]
pub struct BestOfNConfig {
    /// Number of parallel rollouts.
    pub n: usize,
    /// Maximum rollouts in flight (semaphore permits).
    pub concurrency: usize,
    /// Keep worktrees (and the winning patch's siblings) for inspection.
    pub keep_worktrees: bool,
    /// Fork rollouts from a shared exploratory prefix of this many turns
    /// (item 28): one prefix rollout runs in worktree 0, then every
    /// rollout continues its session. `None` (default) runs every child
    /// from turn 0 as before.
    pub shared_prefix_turns: Option<u32>,
    /// Vary each rollout's harness instead of its model (item 24):
    /// child 0 keeps the config unchanged while later children cycle
    /// through terse (halved tool budgets, `Low` reasoning effort) and
    /// verbose (doubled tool budgets, wider edit echo) variants. Default
    /// off — identical children maximize prefix-cache reuse.
    pub vary_harness: bool,
    /// Run the fixed-table suite in every candidate worktree before
    /// ranking (item 29) and drop failing candidates. Deterministic
    /// spend only — no model call. Default on; opt out when the suite
    /// is too slow to run per rollout.
    pub verify_selection: bool,
}

impl Default for BestOfNConfig {
    fn default() -> Self {
        Self {
            n: 3,
            concurrency: 3,
            keep_worktrees: false,
            shared_prefix_turns: None,
            vary_harness: false,
            verify_selection: true,
        }
    }
}

/// One rollout's outcome, whether it completed, failed, or was cancelled.
#[derive(Debug, Clone)]
pub struct RolloutReport {
    /// Rollout index (`0..n`), stable across the report.
    pub index: usize,
    /// The worktree the child ran in (removed on cleanup).
    pub worktree: PathBuf,
    /// The task record's `end_reason` token (`end_turn`, `fast_path`,
    /// `max_turns`, `cancelled`, ...), or an orchestration-level label
    /// (`provider_error`, `open_error`, `run_error: ...`).
    pub end_reason: Option<String>,
    /// The loop's [`TaskSummary`] when the run returned one.
    pub summary: Option<TaskSummary>,
    /// Failure markers: loop nudges + failed tool calls + one for a failed
    /// task end. Lower is better; the Harness² all-fail selector.
    pub markers: u64,
    /// The rollout's full diff (`git diff --cached`), empty when nothing
    /// changed or the run never started.
    pub patch: String,
    /// Human-readable spend footer (turns, requests, tokens) so multi-rollout
    /// cost is visible at the point of use, scout-footer style.
    pub cost_note: String,
    /// Fixed-table suite verdict from the item 29 selection pass:
    /// `Some(false)` = the suite failed and the candidate is dropped
    /// from selection; `None` = not measured (no patch, filter off, or
    /// no verdict — never a drop).
    pub suite_passed: Option<bool>,
}

/// The whole orchestration result: per-rollout reports, the selected
/// winner, and whether its patch applied.
#[derive(Debug, Clone)]
pub struct BestOfNReport {
    /// One report per rollout, index order.
    pub runs: Vec<RolloutReport>,
    /// The winning rollout index, or `None` when no rollout produced a
    /// patch (or all were cancelled).
    pub winner: Option<usize>,
    /// Whether the winning patch was applied to the parent workspace.
    pub applied: bool,
    /// Why the winner's patch did not apply (git's stderr tail), or the
    /// no-candidate note.
    pub apply_note: String,
    /// Where the worktrees lived; removed unless kept for forensics.
    pub worktrees_root: PathBuf,
    /// The shared-prefix spend note (turns, requests, tokens, session id)
    /// when `shared_prefix_turns` produced a seeded prefix, else `None`.
    pub prefix_note: Option<String>,
    /// Set when `vary_harness` diversified the fleet's configs (item 24):
    /// describes the child cycle, else `None`.
    pub harness_note: Option<String>,
    /// The item 29 execution-filter note (verdicts, drops, rank ties)
    /// when the selection suite ran, else `None`.
    pub selection_note: Option<String>,
}

/// Errors that abort the orchestration before or between rollouts. Child
/// failures are reports, not errors.
#[derive(Debug, Clone)]
pub struct WorktreeError {
    /// The failure description.
    pub message: String,
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorktreeError {}

impl WorktreeError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
