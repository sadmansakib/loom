//! The task-level error of §5.1/§13 and the store-failure mapping
//! of ADR-012: every terminal state short of success, with stable
//! `end_reason` tokens.

use crate::context::ContextError;
use crate::model::ModelError;
use crate::store::StoreError;

/// Why a task ended without completing successfully.
///
/// This is the task-level error of §5.1/§13. Cancellation is included but is
/// distinguished by [`TaskError::is_cancelled`]: a cancelled task is not a
/// failure of the work, it is the user stopping it.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The context compiler refused to build a request (e.g. the projection
    /// would overflow the budget). Nothing was sent.
    #[error("context compilation failed: {0}")]
    Context(#[from] ContextError),
    /// A model request failed with a non-retryable error, or exhausted its
    /// retry budget. No partial output was committed.
    #[error("model request failed: {0}")]
    Model(#[from] ModelError),
    /// The model returned a response that violates the wire contract even
    /// after adapter normalization (e.g. `EndTurn` with tool calls, or
    /// `ToolCalls` with none). Nothing was committed for that turn.
    #[error("model response violated the protocol: {0}")]
    Protocol(String),
    /// A session invariant broke during a commit. Unreachable in Iteration 1
    /// — the loop always builds valid call↔result pairs — but kept distinct
    /// from the model-facing [`TaskError::Protocol`] so the recorded end
    /// reason stays honest.
    #[error("session invariant violated: {0}")]
    Internal(String),
    /// The task reached `max_turns` while the model kept requesting tools.
    #[error("task exceeded the maximum of {max_turns} turns")]
    MaxTurns {
        /// The configured turn ceiling.
        max_turns: u32,
    },
    /// The task's [`TaskBudget`] was exhausted: a token, tool-call, or
    /// wall-time limit was reached.
    ///
    /// The budget is checked at the top of each turn, so the unit of work in
    /// flight when the ceiling was crossed (one model response, or one tool
    /// batch) still completed and stays committed.
    #[error("task exceeded its {limit} budget")]
    BudgetExhausted {
        /// Which limit was reached: `input_tokens`, `output_tokens`,
        /// `total_tokens`, `tool_calls`, or `wall_time`.
        limit: &'static str,
    },
    /// The task's cancellation token fired.
    #[error("task was cancelled")]
    Cancelled,
    /// The operator paused the loop between turns: the in-flight turn
    /// (model reply and tool results) fully committed before the stop,
    /// and the session is resumable. Distinct from
    /// [`TaskError::Cancelled`], which aborts mid-flight.
    #[error("task was paused between turns")]
    Paused,
    /// The model stopped for a reason Iteration 1 does not recover from
    /// (`max_tokens`, `context_limit`, `content_filter`). The assistant item
    /// is still committed so the truncated or filtered reply stays
    /// inspectable (§13).
    #[error("model stopped with `{stop_reason}`; recovery is not implemented in iteration 1")]
    UnrecoverableStop {
        /// Which stop reason ended the task.
        stop_reason: &'static str,
    },
    /// A durable [`SessionStore`](crate::store::SessionStore) failed while
    /// persisting the session (ADR-012, persist-then-commit). The step that
    /// failed is not mirrored in the in-memory session, so nothing is applied
    /// on top of a store that did not accept it.
    #[error("session store failed: {0}")]
    Persistence(String),
    /// Another writer holds the session's single-writer lease (ADR-008):
    /// a second loom process is driving this session (or a crashed one's
    /// lease has not expired yet). Nothing was persisted — the task
    /// refused before its first write rather than interleaving.
    #[error("session is leased by another writer ({owner}); lease expires at {expires_at_millis}")]
    SessionLeased {
        /// The lease holder that won.
        owner: String,
        /// When that lease lapses, if not renewed (Unix millis).
        expires_at_millis: u64,
    },
    /// The configured model is unknown to the provider, so its capabilities
    /// could not be resolved. The task fails before its first request
    /// (§6.4).
    #[error("model `{model}` is unknown to the provider")]
    UnknownModel {
        /// The model identifier that could not be resolved.
        model: String,
    },
}

impl TaskError {
    /// Whether this error is cancellation rather than failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, TaskError::Cancelled)
    }

    /// Whether this error is a graceful between-turns pause: not a
    /// failure, and unlike cancellation nothing in flight was aborted.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(self, TaskError::Paused)
    }

    /// Stable `snake_case` token recorded as the task's `end_reason`.
    #[must_use]
    pub fn end_reason(&self) -> &str {
        match self {
            TaskError::Context(_) => "context_error",
            TaskError::Model(_) => "model_error",
            TaskError::Protocol(_) => "protocol",
            TaskError::Internal(_) => "internal",
            TaskError::MaxTurns { .. } => "max_turns",
            TaskError::BudgetExhausted { .. } => "budget_exhausted",
            TaskError::Cancelled => "user_cancelled",
            TaskError::Paused => "paused",
            TaskError::UnrecoverableStop { stop_reason } => stop_reason,
            TaskError::Persistence(_) => "persistence",
            TaskError::SessionLeased { .. } => "session_leased",
            TaskError::UnknownModel { .. } => "unknown_model",
        }
    }
}

/// Maps a store failure to the task-level persistence error (ADR-012).
// By value by design: used as a `.map_err(persistence)` fn pointer,
// which passes the error by value.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn persistence(error: StoreError) -> TaskError {
    TaskError::Persistence(error.to_string())
}
