//! Task outcome bookkeeping (§14): the efficiency summary, the
//! fast-path solve record, and the per-task accounting it reports.

use super::RemainingBudget;
use crate::context::ContextCensus;
use crate::id::TaskId;
use crate::model::{StopReason, Usage};
use crate::session::Session;
use std::time::Duration;

/// Efficiency accounting for one completed task (§14).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummary {
    /// The id of the task's [`TaskRecord`].
    pub task_id: TaskId,
    /// Model turns that reached a commit.
    pub turns: u32,
    /// Model requests issued, including retried attempts.
    pub model_requests: u32,
    /// Prompt tokens summed over completed requests.
    pub input_tokens: u64,
    /// Generated tokens summed over completed requests.
    pub output_tokens: u64,
    /// Cached prompt tokens summed over completed requests, when reported.
    pub cached_input_tokens: Option<u64>,
    /// Tool calls that returned [`ToolStatus::Success`].
    pub successful_tool_calls: u32,
    /// Tool calls that returned [`ToolStatus::Error`].
    pub failed_tool_calls: u32,
    /// Wall-clock duration of the task.
    pub wall_time: Duration,
    /// Events the sink failed to deliver (best-effort observability, §11).
    pub events_dropped: u64,
    /// Budget headroom left when the task ended. A field is `None` when that
    /// resource was unbounded; it is `Some(0)` when the ceiling was reached.
    pub remaining_budget: RemainingBudget,
    /// Per-source census of the final compiled request (ADR-016). Default
    /// (all zero) if the task ended before any request was compiled (e.g.
    /// cancelled, or over budget at turn one).
    pub last_context_census: ContextCensus,
}

impl TaskSummary {
    /// Creates an empty summary for the specified task.
    ///
    /// This is useful to construct incremental summaries in frontends and
    /// tests. A zero `last_context_census` here is only a default value; it is
    /// measured only when `model_requests` is non-zero.
    #[must_use]
    pub fn empty(task_id: TaskId) -> Self {
        Self {
            task_id,
            turns: 0,
            model_requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: None,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            wall_time: Duration::ZERO,
            events_dropped: 0,
            remaining_budget: RemainingBudget::default(),
            last_context_census: ContextCensus::default(),
        }
    }
}

/// What the engine's fast path proved, handed to
/// [`Agent::record_fast_path`].
///
/// Built by the engine after a deterministic solve applied and verified
/// green; the note becomes the task's single assistant item, so the
/// claim and its evidence survive into continuation sessions.
#[derive(Debug, Clone)]
pub struct FastPathSolve {
    /// The analyzer's classification (`"rename"`, `"constant"`,
    /// `"format"`), used for event attribution.
    pub kind: String,
    /// Files the fast path rewrote before verification.
    pub files_changed: u32,
    /// Human-readable proof recorded as the assistant item: what the
    /// pattern matched, what changed, and what verified it green.
    pub note: String,
}

/// Per-task efficiency accumulators (§14). Kept separate from the
/// [`TaskRecord`] so the record is read immutably during compilation and
/// written back only at commit points.
#[derive(Default)]
pub(super) struct Accounting {
    pub(super) turns: u32,
    pub(super) model_requests: u32,
    pub(super) usage: Usage,
    pub(super) successful_tool_calls: u32,
    pub(super) failed_tool_calls: u32,
}

impl Accounting {
    /// Tool calls the loop executed: successes plus failures. Refused
    /// repeats of a failed call never ran, so they do not count.
    pub(super) fn tool_calls(&self) -> u32 {
        self.successful_tool_calls
            .saturating_add(self.failed_tool_calls)
    }

    /// Mirrors the current totals into the session's task record.
    pub(super) fn sync(&self, session: &mut Session, task_id: TaskId) {
        if let Some(record) = session.task_mut(task_id) {
            record.turns = self.turns;
            record.usage = self.usage;
        }
    }
}

/// A stable `snake_case` token for a stop reason, for protocol errors and the
/// `UnrecoverableStop` end reason.
pub(super) fn stop_reason_token(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolCalls => "tool_calls",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ContextLimit => "context_limit",
        StopReason::ContentFilter => "content_filter",
    }
}

/// A duration's whole milliseconds, saturating at `u64::MAX`.
pub(super) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
