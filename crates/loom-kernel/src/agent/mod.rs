//! Agent orchestration: driving the model–tool loop deterministically.
//!
//! [`Agent::run_task`] is the turn loop of architecture.md §13: it compiles
//! bounded context through the [`ContextCompiler`], streams a response
//! through the [`ModelProvider`] with bounded, classification-based retries
//! ([`RetryPolicy`], §6.3), executes any requested tool calls through the
//! tool engine ([`tools`], §10), and commits each turn atomically to the
//! [`Session`] transcript (§6.1).
//!
//! The loop is a deterministic controller; the model is the reasoning
//! component. It contains no provider-specific branches beyond the
//! capability and response semantics defined by `loom-kernel`, and it never
//! imports a concrete provider or tool (ADR-001): both arrive as
//! trait objects and a registry.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::{
    AgentConfig, CompiledContext, ContextBudget, ContextCensus, ContextCompiler, ContextError,
    ContextInput, FactsSource, NoFacts, PruneCheckpoint,
};
use crate::event::{EventKind, EventSink};
use crate::id::{SessionId, TaskId};
use crate::model::{
    Message, ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider,
    ModelRequest, ModelResponse, StopReason, Usage,
};
use crate::session::{PlannedTurn, Session, TaskRecord, TerminalTaskStatus};
use crate::store::{ArtifactStore, SessionStore, StoreError};
use crate::tool::{ToolContext, ToolRegistry};
use crate::tool_cache::ToolCache;
use crate::transcript::{AssistantMessage, Item, ToolCall, ToolFailure, ToolResult, ToolStatus};

pub mod retry;
pub mod tools;

pub use retry::RetryPolicy;

use tools::{MAX_TOOL_OUTPUT_BYTES, ToolBatch, ToolExposure};

/// Default maximum number of model turns per task when nothing else is
/// configured.
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Default output-token reservation: the share of the context window held
/// back for the model's reply (§8). Used verbatim when the model reports
/// no output limit, and as the floor of the automatic derivation
/// ([`OutputReservation::Auto`]) otherwise. Configurable via
/// [`Agent::with_reserved_output_tokens`].
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 8_192;

/// Hard ceiling on a single model turn's output reservation, whatever the
/// model declares. Task budgets (§14) bound aggregate spend; this only
/// shapes individual turns, so a model advertising a 200K-token output
/// ceiling cannot dump it all in one go.
pub const MAX_RESERVED_OUTPUT_TOKENS: u32 = 32_768;

/// How the per-turn output reservation is chosen (§8).
///
/// `Auto` (the default) derives it from the model's capabilities: the
/// model's declared output maximum when it reports one, floored at
/// [`DEFAULT_RESERVED_OUTPUT_TOKENS`] and capped at
/// [`MAX_RESERVED_OUTPUT_TOKENS`], then clamped to half the context
/// window so the projection always keeps room for input. `Fixed` pins an
/// explicit value — still window-clamped, since a reservation that
/// starves input is a misconfiguration, not a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputReservation {
    /// Derive from model capabilities (default).
    Auto,
    /// Explicit reservation, clamped to half the context window.
    Fixed(u32),
}

/// Resolves the output reservation actually used for a task's requests.
///
/// The derived value feeds [`ContextBudget::from_window`] and bounds
/// `max_tokens` on each request (the compiler applies the model's own
/// output limit on top, so a smaller declared maximum always wins).
#[must_use]
pub fn derive_reserved_output_tokens(
    capabilities: &ModelCapabilities,
    reservation: OutputReservation,
) -> u32 {
    let window_half = capabilities.max_context_tokens / 2;
    match reservation {
        OutputReservation::Fixed(reserved) => reserved.min(window_half),
        OutputReservation::Auto => DEFAULT_RESERVED_OUTPUT_TOKENS
            .max(capabilities.max_output_tokens.unwrap_or(0))
            .min(MAX_RESERVED_OUTPUT_TOKENS)
            .min(window_half),
    }
}

/// Default per-tool-call timeout: the bound on the agent's wait for a tool
/// result (§6.6). Configurable via [`Agent::with_tool_timeout`].
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// Default bound on concurrent read-only tool calls (§10).
pub const DEFAULT_MAX_TOOL_CONCURRENCY: usize = tools::DEFAULT_MAX_CONCURRENCY;

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

    /// Stable snake_case token recorded as the task's `end_reason`.
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
            TaskError::UnknownModel { .. } => "unknown_model",
        }
    }
}

/// Cooperative stop control for one task run: the operator's two
/// granularities (§6).
///
/// [`cancel`](TaskControl::cancel) is the hard stop that already existed:
/// the in-flight model stream is dropped, tool calls receive cancelled
/// fillers, and the task ends as
/// [`Cancelled`](TaskError::Cancelled). The session survives — every
/// committed item was persisted before the abort — so "abort the call"
/// never means "lose the session".
///
/// [`pause`](TaskControl::pause) is the graceful stop: the in-flight
/// turn (model reply and tool batch) runs to completion and commits,
/// and the loop stops at the next turn boundary with
/// [`Paused`](TaskError::Paused). A second pause adds nothing; a later
/// [`cancel`](TaskControl::cancel) still wins mid-turn.
///
/// Clones share state, so a UI thread can hold one copy while the
/// driving task consumes another.
#[derive(Clone, Default, Debug)]
pub struct TaskControl {
    /// The hard stop honored by the stream and tool paths (§6.5–§6.6).
    hard: CancellationToken,
    /// The graceful stop, observed only at turn boundaries.
    soft: CancellationToken,
}

impl TaskControl {
    /// A fresh control with neither stop fired.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Abort now: mid-stream, mid-tool, wherever the task is.
    pub fn cancel(&self) {
        self.hard.cancel();
    }

    /// Stop after the current turn completes. Checked at turn
    /// boundaries only — the in-flight turn always finishes.
    pub fn pause(&self) {
        self.soft.cancel();
    }

    /// Whether the hard stop has fired.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.hard.is_cancelled()
    }

    /// Whether a graceful pause has been requested.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.soft.is_cancelled()
    }

    /// The hard-stop token, for callers that wire a cancellation into
    /// existing `CancellationToken` consumers (tool contexts, stream
    /// selects). The pause has no token form on purpose: it is a
    /// turn-boundary decision, not something to race against I/O.
    #[must_use]
    pub fn hard_token(&self) -> CancellationToken {
        self.hard.clone()
    }
}

impl From<CancellationToken> for TaskControl {
    /// Wraps a bare hard-stop token; the result can never pause.
    fn from(hard: CancellationToken) -> Self {
        Self {
            hard,
            soft: CancellationToken::new(),
        }
    }
}

/// Maps a store failure to the task-level persistence error (ADR-012).
fn persistence(error: StoreError) -> TaskError {
    TaskError::Persistence(error.to_string())
}

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

/// A task's resource envelope (§14).
///
/// Every ceiling is optional: `None` means the resource is unbounded, which
/// is the default. The loop checks the envelope once per turn, before
/// compiling the next request, so a ceiling can be exceeded by at most the
/// unit of work in flight when it was crossed — one model response, or one
/// tool batch. A task that completes inside that turn still succeeds.
///
/// The turn ceiling is separate: it stays [`Agent::with_max_turns`], because
/// it is a loop-shape knob rather than a cost bound.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskBudget {
    /// Ceiling on prompt (input) tokens summed over the task.
    pub max_input_tokens: Option<u64>,
    /// Ceiling on generated (output) tokens summed over the task.
    pub max_output_tokens: Option<u64>,
    /// Ceiling on `input + output` tokens summed over the task.
    pub max_total_tokens: Option<u64>,
    /// Ceiling on tool calls the loop executes. Refused repeats of a failed
    /// call do not run and do not count.
    pub max_tool_calls: Option<u32>,
    /// Ceiling on the task's wall-clock duration.
    pub max_wall_time: Option<Duration>,
}

impl TaskBudget {
    /// An envelope with no ceilings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the input-token ceiling.
    #[must_use]
    pub fn with_max_input_tokens(mut self, max_input_tokens: u64) -> Self {
        self.max_input_tokens = Some(max_input_tokens);
        self
    }

    /// Sets the output-token ceiling.
    #[must_use]
    pub fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// Sets the total-token ceiling (`input + output`).
    #[must_use]
    pub fn with_max_total_tokens(mut self, max_total_tokens: u64) -> Self {
        self.max_total_tokens = Some(max_total_tokens);
        self
    }

    /// Sets the tool-call ceiling.
    #[must_use]
    pub fn with_max_tool_calls(mut self, max_tool_calls: u32) -> Self {
        self.max_tool_calls = Some(max_tool_calls);
        self
    }

    /// Sets the wall-clock ceiling.
    #[must_use]
    pub fn with_max_wall_time(mut self, max_wall_time: Duration) -> Self {
        self.max_wall_time = Some(max_wall_time);
        self
    }

    /// The name of the first exhausted limit, if any.
    ///
    /// Deterministic order so the reported [`TaskError::BudgetExhausted`] is
    /// stable when several limits are reached at once: total tokens, then
    /// input, then output, then tool calls, then wall time.
    fn exhausted(&self, accounting: &Accounting, elapsed: Duration) -> Option<&'static str> {
        let limits = [
            (
                self.max_total_tokens,
                accounting
                    .usage
                    .input_tokens
                    .saturating_add(accounting.usage.output_tokens),
                "total_tokens",
            ),
            (
                self.max_input_tokens,
                accounting.usage.input_tokens,
                "input_tokens",
            ),
            (
                self.max_output_tokens,
                accounting.usage.output_tokens,
                "output_tokens",
            ),
            (
                self.max_tool_calls.map(u64::from),
                u64::from(accounting.tool_calls()),
                "tool_calls",
            ),
        ];
        for (max, used, name) in limits {
            if let Some(max) = max
                && used >= max
            {
                return Some(name);
            }
        }
        if let Some(max) = self.max_wall_time
            && elapsed >= max
        {
            return Some("wall_time");
        }
        None
    }

    /// Headroom left after `accounting` and `elapsed`.
    fn remaining(&self, accounting: &Accounting, elapsed: Duration) -> RemainingBudget {
        let rem_u64 = |max: Option<u64>, used: u64| max.map(|max| max.saturating_sub(used));
        RemainingBudget {
            input_tokens: rem_u64(self.max_input_tokens, accounting.usage.input_tokens),
            output_tokens: rem_u64(self.max_output_tokens, accounting.usage.output_tokens),
            total_tokens: rem_u64(
                self.max_total_tokens,
                accounting
                    .usage
                    .input_tokens
                    .saturating_add(accounting.usage.output_tokens),
            ),
            tool_calls: self
                .max_tool_calls
                .map(|max| max.saturating_sub(accounting.tool_calls())),
            wall_time: self.max_wall_time.map(|max| max.saturating_sub(elapsed)),
        }
    }
}

/// Budget headroom when a task ended, surfaced in [`TaskSummary`].
///
/// Each field is `None` when the corresponding ceiling was not set, and
/// `Some(0)` when the ceiling was reached (or passed).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemainingBudget {
    /// Input tokens still allowed, when bounded.
    pub input_tokens: Option<u64>,
    /// Output tokens still allowed, when bounded.
    pub output_tokens: Option<u64>,
    /// Total tokens still allowed, when bounded.
    pub total_tokens: Option<u64>,
    /// Tool calls still allowed, when bounded.
    pub tool_calls: Option<u32>,
    /// Wall time still allowed, when bounded.
    pub wall_time: Option<Duration>,
}

/// The deterministic controller of the model–tool loop (§13).
///
/// Constructed from the four seams it depends on — a [`ModelProvider`], a
/// [`ContextCompiler`], a [`ToolRegistry`], and an [`AgentConfig`] — plus the
/// model identifier whose capabilities are resolved per task (§6.4). All
/// other knobs have documented defaults and builder setters.
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    compiler: Box<dyn ContextCompiler>,
    registry: ToolRegistry,
    config: AgentConfig,
    model: String,
    retry: RetryPolicy,
    max_turns: u32,
    budget: TaskBudget,
    output_reservation: OutputReservation,
    tool_timeout: Duration,
    max_tool_concurrency: usize,
    store: Option<Box<dyn SessionStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    facts: Arc<dyn FactsSource>,
    cache: Arc<Mutex<ToolCache>>,
    /// Whether the loop's evidence-shaped feedback is active (roadmap
    /// item 1's bare-loop arm turns it off): the completion-gate
    /// refusal, the failure-ledger repeat refusal, and the
    /// recent-changes evidence handed to tools. `false` reduces the
    /// loop to stream → execute → commit with no loop-owned reactions.
    loop_shaping: bool,
}

impl Agent {
    /// Assembles an agent from its seams, using documented defaults for the
    /// retry policy, turn ceiling, output reservation, tool timeout, and
    /// tool concurrency.
    #[must_use]
    pub fn new(
        provider: Box<dyn ModelProvider>,
        compiler: Box<dyn ContextCompiler>,
        registry: ToolRegistry,
        config: AgentConfig,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            compiler,
            registry,
            config,
            model: model.into(),
            retry: RetryPolicy::default(),
            max_turns: DEFAULT_MAX_TURNS,
            budget: TaskBudget::default(),
            output_reservation: OutputReservation::Auto,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            max_tool_concurrency: DEFAULT_MAX_TOOL_CONCURRENCY,
            store: None,
            artifacts: None,
            facts: Arc::new(NoFacts),
            cache: Arc::new(Mutex::new(ToolCache::new())),
            loop_shaping: true,
        }
    }

    /// Replaces the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the maximum number of model turns (minimum 1).
    #[must_use]
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns.max(1);
        self
    }

    /// Sets an explicit output-token reservation (§8), overriding the
    /// automatic [`OutputReservation::Auto`] derivation. The value is
    /// still clamped to half the model's context window.
    #[must_use]
    pub fn with_reserved_output_tokens(mut self, reserved_output_tokens: u32) -> Self {
        self.output_reservation = OutputReservation::Fixed(reserved_output_tokens);
        self
    }

    /// Sets the task's resource envelope (§14). Unset ceilings are unbounded.
    #[must_use]
    pub fn with_budget(mut self, budget: TaskBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the per-tool-call timeout (§6.6).
    #[must_use]
    pub fn with_tool_timeout(mut self, tool_timeout: Duration) -> Self {
        self.tool_timeout = tool_timeout;
        self
    }

    /// Sets the bound on concurrent read-only tool calls (§10).
    #[must_use]
    pub fn with_max_tool_concurrency(mut self, max_tool_concurrency: usize) -> Self {
        self.max_tool_concurrency = max_tool_concurrency.max(1);
        self
    }

    /// Attaches a durable [`SessionStore`] (§§6.1, 18; ADR-012).
    ///
    /// With a store attached the loop is **persist-then-commit**: each item,
    /// task record, and turn is written to the store *before* the in-memory
    /// [`Session`] mirrors it, so a crash can never lose the record of an
    /// effect that already ran. A store failure returns
    /// [`TaskError::Persistence`] and leaves the in-memory session unchanged
    /// for that step. Without a store the loop behaves exactly as before
    /// (in-memory only).
    #[must_use]
    pub fn with_store(mut self, store: Box<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Attaches an artifact store (§§12, ADR-012 decision 3).
    ///
    /// With a store attached, a tool result too large for the inline budget is
    /// spilled to the store (content-addressed) and the transcript keeps a
    /// compact [`ArtifactRef`](crate::artifact::ArtifactRef) the model can
    /// refetch with `artifact_read`. Without one, oversized output is cut as
    /// before.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Attaches a repository-facts source (2E, ADR-015).
    ///
    /// The source is scanned **once per task**, at task start, and the
    /// resulting [`KnownFacts`](crate::context::KnownFacts) is projected as a
    /// stable system message after the system prompt. Because the snapshot is
    /// fixed for the task, the cached prefix is never churned mid-task. With
    /// no source attached the block is empty and the projection is unchanged.
    #[must_use]
    pub fn with_facts_source(mut self, facts: Box<dyn FactsSource>) -> Self {
        self.facts = Arc::from(facts);
        self
    }

    /// Disables the loop's evidence-shaped feedback (the bare-loop arm,
    /// roadmap item 1): the completion gate stops refusing EndTurns with
    /// unverified mutations, the failure ledger stops refusing identical
    /// repeats of failed calls, and tools stop receiving the
    /// recent-changes list. The default (`true`) is the production loop.
    #[must_use]
    pub fn with_loop_shaping(mut self, loop_shaping: bool) -> Self {
        self.loop_shaping = loop_shaping;
        self
    }

    /// The model identifier this agent targets.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Loads a persisted session from the attached store, if any.
    ///
    /// Returns `Ok(None)` when no store is attached or the session is unknown,
    /// so a frontend can treat "no persistence" and "not found" alike when
    /// deciding whether to start fresh.
    pub fn load_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        match &self.store {
            Some(store) => store.load(id),
            None => Ok(None),
        }
    }

    /// Lists persisted session ids (creation order), empty without a store.
    pub fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        match &self.store {
            Some(store) => store.session_ids(),
            None => Ok(Vec::new()),
        }
    }

    /// Runs one task against `session`, appending its input and committing
    /// each turn as it completes.
    ///
    /// Returns a [`TaskSummary`] on terminal success (`EndTurn`). Every other
    /// terminal state — including cancellation and a graceful pause —
    /// returns a [`TaskError`]; the [`TaskRecord`] in `session` carries the
    /// same outcome.
    ///
    /// The loop never blocks on the event sink and never treats delivery
    /// failure as a task failure (§11): events are best-effort, the
    /// transcript is the durable record.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        let started = Instant::now();

        // 1. The input becomes the transcript's next user item. Persisted to
        //    the store *before* the in-memory push (ADR-012,
        //    persist-then-commit). `push_item` only fails for an assistant
        //    item carrying tool calls, so the mapped branch is unreachable
        //    here; mapping it keeps the loop panic-free and the failure
        //    honest if it ever became reachable.
        let user = Item::User { text: input.into() };
        if let Some(store) = &self.store {
            store.ensure_session(&session.id).map_err(persistence)?;
            store
                .append_item(&session.id, session.next_index(), &user)
                .map_err(persistence)?;
        }
        session
            .push_item(user)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 2. The task record, persisted then mirrored before its start event.
        let task_id = TaskId::new();
        let record = TaskRecord::new(task_id, now_millis());
        self.persist_task(session, &record)?;
        session.push_task(record);
        sink.emit(task_id, EventKind::TaskStarted);

        // 3. The turn loop; the epilogue below always finalizes the record.
        let mut accounting = Accounting::default();
        let outcome = self
            .run_turns(session, task_id, sink, control, &mut accounting, started)
            .await;

        // 4. Finalize and summarize.
        self.finalize_task(session, task_id, &accounting, outcome, started, sink)
    }

    /// Records a deterministic solve that used zero model calls (roadmap
    /// item 3's fast paths).
    ///
    /// The engine analyzed the instruction, applied mechanical edits,
    /// and verified them green before any provider traffic existed; this
    /// method gives that solve the same transcript and bookkeeping
    /// discipline as a model-driven one — persist-then-commit items, a
    /// task record finished with `end_reason = "fast_path"`, the
    /// [`EventKind::FastPathHit`] marker, and a [`TaskSummary`] whose
    /// zeroed accounting is the proof of the zero-invocation claim. The
    /// solve's note becomes the task's single assistant item, so
    /// continuation sessions see what changed and why.
    pub async fn record_fast_path(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        solve: FastPathSolve,
        sink: &dyn EventSink,
    ) -> Result<TaskSummary, TaskError> {
        let started = Instant::now();

        // 1. The input becomes the transcript's next user item,
        //    persist-then-commit, exactly as `run_task` does.
        let user = Item::User { text: input.into() };
        if let Some(store) = &self.store {
            store.ensure_session(&session.id).map_err(persistence)?;
            store
                .append_item(&session.id, session.next_index(), &user)
                .map_err(persistence)?;
        }
        session
            .push_item(user)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 2. The task record and its start events; the marker names the
        //    deterministic classification for attribution.
        let task_id = TaskId::new();
        let record = TaskRecord::new(task_id, now_millis());
        self.persist_task(session, &record)?;
        session.push_task(record);
        sink.emit(task_id, EventKind::TaskStarted);
        sink.emit(
            task_id,
            EventKind::FastPathHit {
                kind: solve.kind.clone(),
                files_changed: solve.files_changed,
            },
        );

        // 3. The solve's proof text is the task's only assistant item —
        //    no tool calls ran through the loop, so nothing else joins
        //    the transcript.
        let assistant = Item::Assistant(AssistantMessage {
            content: solve.note,
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        });
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &assistant)
                .map_err(persistence)?;
        }
        session
            .push_item(assistant)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 4. Finalize: zeroed usage, `fast_path` end reason, terminal
        //    event, and the summary carrying the zero-invocation proof.
        if let Some(record) = session.task_mut(task_id) {
            record.turns = 0;
            record.finish(
                TerminalTaskStatus::Completed,
                now_millis(),
                "fast_path".to_owned(),
            );
        }
        let finished = session
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned();
        if let Some(record) = finished {
            self.persist_task(session, &record)?;
        }
        sink.emit(task_id, EventKind::TaskCompleted);
        let summary = TaskSummary {
            task_id,
            turns: 0,
            model_requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: None,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            wall_time: started.elapsed(),
            events_dropped: sink.dropped(),
            remaining_budget: self
                .budget
                .remaining(&Accounting::default(), started.elapsed()),
            last_context_census: ContextCensus::default(),
        };
        Ok(summary)
    }

    /// Finalizes a finished task: finish + persist the record, emit the
    /// terminal event, and build the [`TaskSummary`]. The record is
    /// finished before the summary is returned so callers can read
    /// either; `events_dropped` is sampled after the terminal emit so a
    /// closed sink's drop of that event is counted too (§11).
    fn finalize_task(
        &self,
        session: &mut Session,
        task_id: TaskId,
        accounting: &Accounting,
        outcome: Result<ContextCensus, TaskError>,
        started: Instant,
        sink: &dyn EventSink,
    ) -> Result<TaskSummary, TaskError> {
        let wall_time = started.elapsed();
        let status = match &outcome {
            Ok(_) => TerminalTaskStatus::Completed,
            Err(error) if error.is_cancelled() => TerminalTaskStatus::Cancelled,
            Err(error) if error.is_paused() => TerminalTaskStatus::Paused,
            Err(_) => TerminalTaskStatus::Failed,
        };
        let end_reason = match &outcome {
            Ok(_) => "end_turn".to_owned(),
            Err(error) => error.end_reason().to_owned(),
        };
        if let Some(record) = session.task_mut(task_id) {
            record.turns = accounting.turns;
            record.usage = accounting.usage;
            record.finish(status, now_millis(), end_reason);
        }
        // Persist the terminal record before announcing it: a store failure
        // fails the task, and no terminal event claims a completion the store
        // never saw.
        let finished = session
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned();
        if let Some(record) = finished {
            self.persist_task(session, &record)?;
        }
        match &outcome {
            Ok(_) => sink.emit(task_id, EventKind::TaskCompleted),
            Err(error) if error.is_cancelled() => sink.emit(task_id, EventKind::TaskCancelled),
            Err(error) if error.is_paused() => sink.emit(task_id, EventKind::TaskPaused),
            Err(error) => sink.emit(
                task_id,
                EventKind::TaskFailed {
                    reason: error.to_string(),
                },
            ),
        }
        let events_dropped = sink.dropped();
        let last_context_census = outcome.as_ref().ok().copied().unwrap_or_default();

        let summary = TaskSummary {
            task_id,
            turns: accounting.turns,
            model_requests: accounting.model_requests,
            input_tokens: accounting.usage.input_tokens,
            output_tokens: accounting.usage.output_tokens,
            cached_input_tokens: accounting.usage.cached_input_tokens,
            successful_tool_calls: accounting.successful_tool_calls,
            failed_tool_calls: accounting.failed_tool_calls,
            wall_time,
            events_dropped,
            remaining_budget: self.budget.remaining(accounting, wall_time),
            last_context_census,
        };

        outcome.map(|_| summary)
    }

    /// The turn loop proper (§13). Returns the final compiled request's
    /// census on `EndTurn`, or the error that ended the task.
    async fn run_turns(
        &self,
        session: &mut Session,
        task_id: TaskId,
        sink: &dyn EventSink,
        control: &TaskControl,
        accounting: &mut Accounting,
        started: Instant,
    ) -> Result<ContextCensus, TaskError> {
        // Capabilities are resolved once: in Iteration 1 the model is fixed
        // for the task (§6.4). An unknown model fails before the first
        // request rather than guessing limits.
        let capabilities =
            self.provider
                .capabilities(&self.model)
                .ok_or_else(|| TaskError::UnknownModel {
                    model: self.model.clone(),
                })?;
        let budget = self.task_budget(&capabilities)?;

        // Repository facts for this task (2E, ADR-015): captured exactly once,
        // before the first compile, so the projected block is byte-stable for
        // the whole task (§8.1.1) and "what changed" never costs a model turn.
        let facts = self.scan_facts().await?;

        // Pruning checkpoint for this task's turn loop (ADR-011): the
        // compiler returns a frozen decision set when its trigger fires,
        // and the loop feeds it back on later turns so the projection stays
        // byte-stable between triggers (one cache miss per trigger). It is
        // per-task loop state, not agent state.
        let mut prune_checkpoint: Option<PruneCheckpoint> = None;

        // Loop-owned reaction to tool failures (§13): the loop, not the
        // model, decides whether a failed attempt may be repeated. An
        // identical call is refused until some tool call succeeds, so a
        // failure forces a change of approach instead of trial and error.
        let mut failures = FailureLedger::default();

        // Completion gate (roadmap item 8): an EndTurn is refused once
        // while unverified mutations are outstanding, so "done" always
        // refers to a state some green check actually covered.
        let mut completion = CompletionGate::default();

        // The hard-stop token the stream and tool paths select on
        // (§6.5–§6.6); the pause is a turn-boundary decision and never
        // races against I/O.
        let hard = control.hard_token();

        for turn in 1..=self.max_turns {
            // A pause requested before the task or during the previous
            // turn's tool batch stops here: the in-flight turn has fully
            // committed, nothing is dropped, and the session is
            // resumable. The user's signal wins over further spending.
            if control.is_paused() {
                return Err(TaskError::Paused);
            }

            // Enforce the task's resource envelope before spending anything
            // further (§14). Checked once per turn, so the unit in flight when
            // a ceiling was crossed still completed and stays committed.
            if let Some(limit) = self.budget.exhausted(accounting, started.elapsed()) {
                return Err(TaskError::BudgetExhausted { limit });
            }

            sink.emit(task_id, EventKind::TurnStarted { turn });

            // a. Compile the model request from durable state (§8). The
            //    compiler owns every context decision; the loop only passes
            //    it the inputs. When even the compiler's rescue arm cannot
            //    fit the projection and handoff compaction is armed, the
            //    loop spends one model call on a goal-directed summary and
            //    retries the compile on the handoff view (roadmap item
            //    12's last resort — once per task; a second overflow is
            //    the task's error).
            let compiled = match self.compile_turn(
                session,
                task_id,
                budget,
                &capabilities,
                &facts,
                &prune_checkpoint,
            ) {
                Ok(compiled) => compiled,
                Err(overrun @ TaskError::Context(ContextError::BudgetExceeded { .. }))
                    if self.config.handoff_compaction
                        && !prune_checkpoint
                            .as_ref()
                            .is_some_and(|frozen| frozen.handoff.is_some()) =>
                {
                    let Some(checkpoint) = self
                        .attempt_handoff(session, task_id, sink, &hard, accounting, turn)
                        .await?
                    else {
                        return Err(overrun);
                    };
                    prune_checkpoint = Some(checkpoint);
                    self.compile_turn(
                        session,
                        task_id,
                        budget,
                        &capabilities,
                        &facts,
                        &prune_checkpoint,
                    )?
                }
                Err(error) => return Err(error),
            };
            // Carry the pruning checkpoint forward (ADR-011): it is task
            // state, not request state, so it lives on the loop and is fed
            // back in on the next turn. Between triggers it is unchanged,
            // so the projection stays byte-stable for prefix caching.
            prune_checkpoint = compiled.prune_checkpoint;
            // A new freeze is an observable mechanism decision (item
            // 20.1): attribute it at the moment it happens, not on every
            // later turn the frozen stubs merely re-apply.
            if let Some(report) = compiled.prune_report {
                sink.emit(task_id, EventKind::ContextPruned { turn, report });
            }
            // Partial move: `request` moves out, `census` (Copy) stays readable
            // for the `EndTurn` return.
            let stubbed_calls = compiled.stubbed_calls;
            let request = compiled.request;

            // b. Stream with bounded retry (§6.3). Only a `Completed` event
            //    produces a response; failed attempts leave no trace.
            let response = self
                .stream_with_retry(&request, task_id, sink, &hard, accounting)
                .await?;
            accounting.usage = accounting.usage + response.usage;
            let assistant = response.message;
            let has_calls = !assistant.tool_calls.is_empty();

            // c. Inspect the stop reason (§5.1, §13).
            match assistant.stop_reason {
                StopReason::EndTurn => {
                    if has_calls {
                        return Err(TaskError::Protocol(
                            "EndTurn response carried tool calls".to_owned(),
                        ));
                    }
                    // Completion gate (roadmap item 8): refuse the finish
                    // once when files changed since the last green check,
                    // quoting the changed paths and the original task.
                    // One refusal only: the model may insist, and the
                    // loop never deadlocks with a task that cannot be
                    // verified.
                    if self.refuse_completion_once(
                        session,
                        task_id,
                        turn,
                        &mut completion,
                        accounting,
                        &assistant,
                    )? {
                        continue;
                    }
                    self.commit(session, assistant, Vec::new())?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    return Ok(compiled.census);
                }
                StopReason::ToolCalls => {
                    if !has_calls {
                        return Err(TaskError::Protocol(
                            "ToolCalls stop reason without tool calls".to_owned(),
                        ));
                    }
                    // The session's todo list travels through the batch as
                    // a shared handle the `todo` tool mutates (roadmap
                    // item 8 phase B); the snapshot below is copied back
                    // after the turn commits.
                    let todo_revision = session.todos.revision;
                    let todos = Arc::new(Mutex::new(session.todos.clone()));
                    let results = self
                        .execute_batch(
                            &assistant.tool_calls,
                            &request,
                            &stubbed_calls,
                            task_id,
                            sink,
                            &hard,
                            accounting,
                            &mut failures,
                            &recent_changes(session),
                            Arc::clone(&todos),
                        )
                        .await;
                    // The gate learns from every result before the turn
                    // commits: mutations bump the epoch, green checks pin
                    // it. A fused edit (write + green check in one result)
                    // lands verified.
                    for result in &results {
                        completion.note(result);
                    }
                    // A batch interrupted by cancellation still commits its
                    // completed results plus cancelled fillers (§6.1, §10).
                    self.commit(session, assistant, results)?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    // Copy the (possibly mutated) todo state back, and —
                    // only when it changed — re-append the rendered list
                    // at the transcript tail. This runs before the
                    // cancellation check on purpose: a changed list is
                    // durable state the next session load must see.
                    let todos = todos
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if todos.revision != todo_revision {
                        session.todos = todos;
                        self.append_todo_block(session)?;
                        sink.emit(
                            task_id,
                            EventKind::TodoUpdated {
                                revision: session.todos.revision,
                                items: session.todos.items.clone(),
                            },
                        );
                    }
                    if control.is_cancelled() {
                        return Err(TaskError::Cancelled);
                    }
                    // A pause that fired during the batch stops here too:
                    // the batch committed, and the user's signal wins
                    // over the max-turns bookkeeping below.
                    if control.is_paused() {
                        return Err(TaskError::Paused);
                    }
                    if turn == self.max_turns {
                        return Err(TaskError::MaxTurns {
                            max_turns: self.max_turns,
                        });
                    }
                    // Otherwise continue to the next turn.
                }
                other => {
                    // `max_tokens` / `context_limit` / `content_filter`
                    // (and any future reason). The reply is committed first
                    // so it stays inspectable, then the task fails.
                    if has_calls {
                        return Err(TaskError::Protocol(format!(
                            "{} response carried tool calls",
                            stop_reason_token(other)
                        )));
                    }
                    self.commit(session, assistant, Vec::new())?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    return Err(TaskError::UnrecoverableStop {
                        stop_reason: stop_reason_token(other),
                    });
                }
            }
        }

        // The loop only exits without returning when every turn requested
        // tools and none was the last — impossible: `turn == max_turns` is
        // handled inside, so reaching here means `max_turns` was zero, which
        // the builder forbids. Report it as a turn-limit failure regardless.
        Err(TaskError::MaxTurns {
            max_turns: self.max_turns,
        })
    }

    /// The task's input budget: the window less the reserved output
    /// (§6.4), derived once per task.
    fn task_budget(&self, capabilities: &ModelCapabilities) -> Result<ContextBudget, TaskError> {
        let reserved_output_tokens =
            derive_reserved_output_tokens(capabilities, self.output_reservation);
        Ok(ContextBudget::from_window(
            capabilities.max_context_tokens,
            reserved_output_tokens,
        )?)
    }

    /// Scans repository facts on the blocking pool (2E, ADR-015): the
    /// scan walks the tree and shells out to git — blocking work, never
    /// on this async worker.
    async fn scan_facts(&self) -> Result<crate::context::KnownFacts, TaskError> {
        let source = Arc::clone(&self.facts);
        tokio::task::spawn_blocking(move || source.scan())
            .await
            .map_err(|_| TaskError::Internal("facts scan panicked".to_owned()))
    }

    /// Compiles one turn's model request from durable state (§8): the
    /// compiler owns every context decision; the loop only passes it
    /// the inputs.
    fn compile_turn(
        &self,
        session: &Session,
        task_id: TaskId,
        budget: ContextBudget,
        capabilities: &ModelCapabilities,
        facts: &crate::context::KnownFacts,
        prune_checkpoint: &Option<PruneCheckpoint>,
    ) -> Result<CompiledContext, TaskError> {
        let task = session
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| TaskError::Protocol("task record missing".to_owned()))?;
        self.compiler
            .compile(ContextInput {
                session,
                tools: &self.registry,
                task,
                budget,
                capabilities,
                model: &self.model,
                config: &self.config,
                facts,
                prune_checkpoint: prune_checkpoint.as_ref(),
            })
            .map_err(|error| error.into())
    }

    /// The refusal half of the completion gate (roadmap item 8): when
    /// files changed since the last green check and nothing was refused
    /// yet, commit the assistant's reply, append a refusal as a normal
    /// user item — persisted, projected, and answered like any other
    /// turn, quoting the changed paths and the original task — and let
    /// the loop take one more turn. Returns `true` when the refusal
    /// fired; the caller then continues instead of finishing.
    fn refuse_completion_once(
        &self,
        session: &mut Session,
        task_id: TaskId,
        turn: u32,
        completion: &mut CompletionGate,
        accounting: &mut Accounting,
        assistant: &AssistantMessage,
    ) -> Result<bool, TaskError> {
        if !self.loop_shaping
            || !(completion.pending() && !completion.refused && turn < self.max_turns)
        {
            return Ok(false);
        }
        completion.refused = true;
        self.commit(session, assistant.clone(), Vec::new())?;
        let reminder = Item::User {
            text: completion_refusal(session),
        };
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &reminder)
                .map_err(persistence)?;
        }
        session
            .push_item(reminder)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        accounting.turns = turn;
        accounting.sync(session, task_id);
        Ok(true)
    }

    /// The LLM handoff (roadmap item 12's last resort): builds a
    /// deterministic digest of the task and recent history, spends one
    /// model call asking for a goal-directed summary, and freezes it
    /// into a pruning checkpoint whose projection is brief + handoff +
    /// tail. The call bills like any other (usage accounted, events
    /// emitted, retries honored). `None` when the reply is unusable —
    /// wrong stop reason, tool calls, or empty text — in which case the
    /// caller surfaces the original budget overrun.
    async fn attempt_handoff(
        &self,
        session: &Session,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
        turn: u32,
    ) -> Result<Option<PruneCheckpoint>, TaskError> {
        let digest = handoff_digest(session);
        let request = ModelRequest {
            model: self.model.clone(),
            messages: vec![
                Message::System {
                    text: HANDOFF_PROMPT.to_owned(),
                },
                Message::User { text: digest },
            ],
            // No tools: the summary must be plain text, and an
            // EndTurn-with-calls reply is rejected below anyway.
            tools: Vec::new(),
            max_tokens: Some(HANDOFF_MAX_OUTPUT_TOKENS),
            temperature: None,
            stable_prefix_messages: None,
        };
        let response = self
            .stream_with_retry(&request, task_id, sink, cancellation, accounting)
            .await?;
        accounting.usage = accounting.usage + response.usage;
        let summary = response.message;
        if summary.stop_reason != StopReason::EndTurn
            || !summary.tool_calls.is_empty()
            || summary.content.trim().is_empty()
        {
            return Ok(None);
        }
        let chars = summary.content.chars().count();
        sink.emit(task_id, EventKind::ContextHandoff { turn, chars });
        Ok(Some(PruneCheckpoint {
            upto_item: session.transcript.len(),
            stubs: Vec::new(),
            elided_args: Vec::new(),
            elided_users: Vec::new(),
            elided_reasoning: Vec::new(),
            elided_assistant_text: Vec::new(),
            handoff: Some(summary.content),
        }))
    }

    /// Streams one model request with bounded retries, emitting provisional
    /// deltas and retry events as it goes.
    async fn stream_with_retry(
        &self,
        request: &ModelRequest,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
    ) -> Result<ModelResponse, TaskError> {
        let mut attempt: u32 = 1;
        loop {
            accounting.model_requests += 1;
            sink.emit(task_id, EventKind::ModelRequestStarted { attempt });

            match self.stream_once(request, task_id, sink, cancellation).await {
                Attempt::Completed(response) => return Ok(response),
                Attempt::Cancelled => return Err(TaskError::Cancelled),
                Attempt::Failed {
                    error,
                    response_started,
                } => {
                    // Cancellation wins over retry (§6.3): a cancelled
                    // request is not a failure to recover from.
                    if cancellation.is_cancelled() {
                        return Err(TaskError::Cancelled);
                    }
                    if self.retry.should_retry(attempt, &error) {
                        let delay = self.retry.delay(attempt, &error);
                        sink.emit(
                            task_id,
                            EventKind::ModelRetry {
                                attempt,
                                kind: error.kind,
                                delay_ms: duration_millis(delay),
                                response_started,
                            },
                        );
                        if self.sleep_or_cancel(delay, cancellation).await {
                            return Err(TaskError::Cancelled);
                        }
                        attempt = attempt.saturating_add(1);
                    } else {
                        return Err(TaskError::Model(error));
                    }
                }
            }
        }
    }

    /// One attempt: consumes the stream to a single terminal outcome,
    /// emitting deltas. The stream is dropped on cancellation, which is the
    /// only cancellation signal a `ModelStream` carries (§6.6).
    async fn stream_once(
        &self,
        request: &ModelRequest,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Attempt {
        let mut stream = self.provider.stream(request);
        let mut response_started = false;
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    drop(stream);
                    return Attempt::Cancelled;
                }
                next = stream.next() => match next {
                    Some(Ok(ModelEvent::Completed(response))) => {
                        sink.emit(
                            task_id,
                            EventKind::ModelCompleted {
                                response: response.clone(),
                            },
                        );
                        return Attempt::Completed(response);
                    }
                    Some(Ok(ModelEvent::TextDelta(text))) => {
                        response_started = true;
                        sink.emit(task_id, EventKind::TextDelta { text });
                    }
                    Some(Ok(ModelEvent::ReasoningDelta(text))) => {
                        response_started = true;
                        sink.emit(task_id, EventKind::ReasoningDelta { text });
                    }
                    Some(Err(error)) => {
                        drop(stream);
                        return Attempt::Failed { error, response_started };
                    }
                    None => {
                        // A stream that ends without `Completed` breaks the
                        // provider contract; treat it as a protocol error.
                        drop(stream);
                        return Attempt::Failed {
                            error: ModelError::new(
                                ModelErrorKind::Protocol,
                                "model stream ended without ModelCompleted",
                            ),
                            response_started,
                        };
                    }
                }
            }
        }
    }

    /// Waits for `delay` unless the task is cancelled first. Returns `true`
    /// when cancellation won.
    async fn sleep_or_cancel(&self, delay: Duration, cancellation: &CancellationToken) -> bool {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => true,
            () = tokio::time::sleep(delay) => false,
        }
    }

    /// Executes one assistant turn's tool calls through the tool engine
    /// (§10), emitting start/complete events and counting outcomes.
    ///
    /// Runs are bounded and order-preserving inside [`ToolBatch`]; the
    /// per-call events are emitted in call order (all starts before the
    /// batch, completes after), which is exact for the sequential path and
    /// an Iteration 1 approximation under read-only concurrency. Only calls
    /// that actually run produce start/complete events: a refused repeat
    /// emits none, and its `RepeatedCall` result is committed to the
    /// transcript instead.
    ///
    /// The loop — not the model — owns the reaction to a failure (§13):
    /// `failures` refuses an exact repeat of a call that already failed
    /// (returning [`ToolFailure::RepeatedCall`] without invoking the tool)
    /// until some call succeeds, and every failure the model sees carries a
    /// deterministic next-step hint.
    #[allow(clippy::too_many_arguments)]
    async fn execute_batch(
        &self,
        calls: &[ToolCall],
        request: &ModelRequest,
        stubbed: &[String],
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
        failures: &mut FailureLedger,
        recent_changes: &[String],
        todos: Arc<Mutex<crate::session::TodoList>>,
    ) -> Vec<ToolResult> {
        let exposure = ToolExposure::from_names(request.tools.iter().map(|spec| spec.name.clone()));

        // Partition into calls the loop refuses (an exact repeat of a
        // failure with no success since) and calls that may run. A refused
        // call never reaches the tool engine, so it starts nothing and emits
        // no tool events; its `RepeatedCall` result is committed instead.
        let mut runnable: Vec<ToolCall> = Vec::new();
        let mut runnable_indices: Vec<usize> = Vec::new();
        let mut is_runnable = vec![false; calls.len()];
        for (index, call) in calls.iter().enumerate() {
            if !(self.loop_shaping && failures.refuses(call)) {
                runnable_indices.push(index);
                runnable.push(call.clone());
                is_runnable[index] = true;
            }
        }
        for call in &runnable {
            sink.emit(
                task_id,
                EventKind::ToolStarted {
                    name: call.name.clone(),
                    call_id: call.id.clone(),
                },
            );
        }

        // Call ids whose results the model still sees in this request
        // (post-projection). The tool engine serves a cache hit for one of
        // these as a cheap `unchanged since call <id>` stub (roadmap
        // item 6): the full content is already in view. `visible_call_ids`
        // excludes any id whose body a pruning pass replaced with a stub.
        let projected_calls = visible_call_ids(request, stubbed);

        let context = ToolContext::new(
            self.config.workspace_root.clone(),
            cancellation.clone(),
            self.tool_timeout,
        )
        .with_artifacts(self.artifacts.clone())
        .with_recent_changes(if self.loop_shaping {
            recent_changes.to_vec()
        } else {
            Vec::new()
        })
        .with_cache(Some(self.cache.clone()))
        .with_projected_calls(projected_calls)
        .with_todos(Some(todos));
        let batch = ToolBatch::new(&self.registry, &exposure)
            .with_max_concurrency(self.max_tool_concurrency);
        let ran = batch.execute(&runnable, &context).await;
        // Dedupe key for harness notices within this batch: one event per
        // distinct (category, detail), not one per parallel call.
        let mut emitted_notices: HashSet<(String, String)> = HashSet::new();

        // Refused calls were never handed to the engine; reconstruct every
        // result in call order from the executed set plus refusal fillers.
        let mut executed: HashMap<usize, ToolResult> =
            runnable_indices.iter().copied().zip(ran).collect();
        let mut results: Vec<ToolResult> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                executed
                    .remove(&index)
                    .unwrap_or_else(|| repeated_call_result(call))
            })
            .collect();

        for (index, (call, result)) in calls.iter().zip(results.iter_mut()).enumerate() {
            match result.status {
                ToolStatus::Success => {
                    accounting.successful_tool_calls += 1;
                    // Progress: a previously failed call is a legitimate
                    // retry again once the workspace may have changed.
                    failures.clear();
                }
                ToolStatus::Error(kind) => {
                    accounting.failed_tool_calls += 1;
                    failures.record(call);
                    append_failure_hint(&mut result.content, kind);
                }
                // A cancelled call is neither a success nor a failure of the
                // tool; the task outcome records the cancellation.
                ToolStatus::Cancelled { .. } => {}
            }
            // Pair each start with a completion: refused calls emitted no
            // start, so they emit no completion either.
            if is_runnable[index] {
                sink.emit(
                    task_id,
                    EventKind::ToolCompleted {
                        name: call.name.clone(),
                        call_id: call.id.clone(),
                    },
                );
                // Harness observations ride the result into the trace:
                // one event per distinct notice in this batch (a healthy
                // default run attaches none).
                for notice in &result.notices {
                    let key = (notice.category.clone(), notice.detail.clone());
                    if emitted_notices.insert(key) {
                        sink.emit(
                            task_id,
                            EventKind::HarnessNotice {
                                category: notice.category.clone(),
                                detail: notice.detail.clone(),
                            },
                        );
                    }
                }
            }
        }
        results
    }

    /// The single atomic commit point of a turn (§6.1): the assistant item
    /// and one result per call, or nothing on a validation failure.
    ///
    /// With a store attached this is persist-then-commit: the planned turn is
    /// written first (one store transaction), then applied in memory. A store
    /// failure returns [`TaskError::Persistence`] and the transcript is
    /// untouched (ADR-012).
    fn commit(
        &self,
        session: &mut Session,
        assistant: AssistantMessage,
        results: Vec<ToolResult>,
    ) -> Result<(), TaskError> {
        let planned: PlannedTurn = session
            .plan_turn(assistant, results)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        if let Some(store) = &self.store {
            store
                .append_turn(&session.id, &planned)
                .map_err(persistence)?;
        }
        session.apply_planned_turn(planned);
        Ok(())
    }

    /// Mirrors a task record to the store, if one is attached.
    fn persist_task(&self, session: &Session, record: &TaskRecord) -> Result<(), TaskError> {
        if let Some(store) = &self.store {
            store
                .upsert_task(&session.id, record)
                .map_err(persistence)?;
        }
        Ok(())
    }

    /// Re-appends the current todo list at the transcript tail (roadmap
    /// item 8 phase B): the loop's half of the tail projection rule.
    ///
    /// Called only when a turn's tool batch changed the list, so the
    /// freshest plan block lands at the tail exactly at the moments it
    /// changed — never duplicated on quiet turns. Because the transcript
    /// is append-only, the projected prefix stays byte-identical for
    /// provider caching, and every earlier block simply rides in
    /// history (where the pruning compiler's user elision may eventually
    /// reclaim it — the last user item, which a fresh block is, is
    /// protected). Persist-then-commit like every other item; an empty
    /// list projects nothing.
    fn append_todo_block(&self, session: &mut Session) -> Result<(), TaskError> {
        if session.todos.is_empty() {
            return Ok(());
        }
        let block = Item::User {
            text: session.todos.render(),
        };
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &block)
                .map_err(persistence)?;
        }
        session
            .push_item(block)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        Ok(())
    }
}

/// Tracks the calls that failed during one task so the loop can refuse an
/// exact repeat (§13). Cleared whenever any tool call succeeds, because a
/// new success means the workspace may have changed and a previously failing
/// call may now be legitimate (e.g. write a file, then edit it).
#[derive(Default)]
struct FailureLedger {
    failed: HashSet<(String, String)>,
}

impl FailureLedger {
    /// Whether `call` exactly repeats a recorded failure with no success since.
    fn refuses(&self, call: &ToolCall) -> bool {
        self.failed.contains(&ledger_key(call))
    }

    /// Records a failed call.
    fn record(&mut self, call: &ToolCall) {
        self.failed.insert(ledger_key(call));
    }

    /// Forgets every failure: some tool call has succeeded since.
    fn clear(&mut self) {
        self.failed.clear();
    }
}

/// Identity of a call for the failure ledger: tool name plus canonical
/// argument JSON. `Value` object keys are ordered, so equality is
/// deterministic — this relies on `serde_json`'s map ordering. The
/// `preserve_order` feature must stay off anywhere in the workspace, or two
/// calls differing only in key order would key differently and evade the
/// refusal.
fn ledger_key(call: &ToolCall) -> (String, String) {
    (
        call.name.clone(),
        serde_json::to_string(&*call.arguments).unwrap_or_default(),
    )
}

/// Completion-gate state for one task (roadmap item 8).
///
/// Tracks whether any file mutations are outstanding — changed after the
/// last green check the model ran. "Green check" is detected from the
/// result shape every command-bearing tool shares (`$ cmd` first, an
/// `exit status: 0` line), covering `verify`, `run_command`, and the fused
/// edit check alike.
#[derive(Default)]
struct CompletionGate {
    /// Mutation epoch: bumped on every committed file write or delete.
    mutations: u64,
    /// The epoch that had a green check; `None` when nothing ever passed.
    verified_at: Option<u64>,
    /// Whether the one-shot refusal already fired.
    refused: bool,
}

impl CompletionGate {
    /// Learns one committed tool result.
    fn note(&mut self, result: &ToolResult) {
        if !result.effects.files_written.is_empty() || !result.effects.files_deleted.is_empty() {
            self.mutations += 1;
        }
        if green_check(result) {
            self.verified_at = Some(self.mutations);
        }
    }

    /// Whether unverified mutations are outstanding.
    fn pending(&self) -> bool {
        self.mutations > 0 && self.verified_at != Some(self.mutations)
    }
}

/// Whether a result is a green command run: successful, command-shaped
/// (`$ …`), and reporting exit status 0.
/// System prompt for the one-shot LLM handoff (roadmap item 12's last
/// resort). Goal-directed by design: the summary names the objective,
/// decisions, open problems, and relevant files, not a narrative.
const HANDOFF_PROMPT: &str = "You are compacting an agent session that outgrew its context \
window. Write a handoff summary the same agent can continue from as if \
no history was lost. Include, in this order: (1) the task objective as \
currently understood, including any re-scoping; (2) decisions already \
made and why; (3) open problems and failed approaches, with the exact \
errors; (4) files and paths that matter, with their current state; (5) \
the single next step. Be terse and concrete — paths, commands, \
identifiers, not prose. Plain text only.";

/// Output ceiling for the handoff summary request.
const HANDOFF_MAX_OUTPUT_TOKENS: u32 = 1024;

/// The deterministic digest the handoff request carries: the live task
/// brief, the session's opening brief when it differs, the trailing
/// assistant texts, the trailing failed tool results, and every path
/// the session touched. Fixed caps keep the digest small even when the
/// transcript is enormous — that is the point of the handoff.
fn handoff_digest(session: &Session) -> String {
    let cap = |text: &str, max: usize| -> String {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= max {
            text.to_owned()
        } else {
            chars[..max].iter().collect()
        }
    };
    let mut digest = String::new();

    // The live task brief: the last user item that is real input
    // (run_task appends the task text; elision never takes the last
    // user item, so it is intact). Kernel-appended todo blocks
    // (roadmap item 8) are projection artifacts, not instructions —
    // skip them so the digest's brief is never the plan list itself.
    let last_user = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::User { text } if !text.starts_with(crate::session::TODO_BLOCK_HEADER) => {
                Some(text.clone())
            }
            _ => None,
        })
        .unwrap_or_default();
    let brief = session.transcript.first().and_then(|item| match item {
        Item::User { text } => Some(text.clone()),
        _ => None,
    });
    digest.push_str("## Task\n");
    digest.push_str(&cap(&last_user, 2_000));
    digest.push('\n');
    if let Some(brief) = brief
        && brief != last_user
    {
        digest.push_str("\n## Session brief\n");
        digest.push_str(&cap(&brief, 1_000));
        digest.push('\n');
    }

    // Trailing assistant texts — the freshest reasoning survives.
    let texts: Vec<&str> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(assistant)
                if assistant.stop_reason != StopReason::ContentFilter
                    && !assistant.content.trim().is_empty() =>
            {
                Some(assistant.content.as_str())
            }
            _ => None,
        })
        .collect();
    if !texts.is_empty() {
        digest.push_str("\n## Recent assistant messages (oldest first)\n");
        for text in texts.iter().rev().take(4).rev() {
            digest.push_str("- ");
            digest.push_str(&cap(text, 500));
            digest.push('\n');
        }
    }

    // Trailing failed tool results — the exact errors to not repeat.
    let failures: Vec<&str> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.status != ToolStatus::Success => {
                Some(result.content.as_str())
            }
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        digest.push_str("\n## Recent failed tool results (oldest first)\n");
        for content in failures.iter().rev().take(3).rev() {
            digest.push_str("- ");
            digest.push_str(&cap(content, 300));
            digest.push('\n');
        }
    }

    // Every path the session touched — deduplicated, first-seen order.
    let mut touched: Vec<&str> = Vec::new();
    for item in &session.transcript {
        if let Item::ToolResult(result) = item {
            for path in result
                .effects
                .files_written
                .iter()
                .chain(&result.effects.files_deleted)
            {
                if !touched.contains(&path.as_str()) {
                    touched.push(path.as_str());
                }
            }
        }
    }
    if !touched.is_empty() {
        digest.push_str("\n## Files touched\n");
        for path in touched.iter().take(40) {
            digest.push_str("- ");
            digest.push_str(path);
            digest.push('\n');
        }
    }

    digest
}

fn green_check(result: &ToolResult) -> bool {
    if !matches!(result.status, ToolStatus::Success) {
        return false;
    }
    let mut command_shaped = false;
    for line in result.content.lines() {
        if line.starts_with("$ ") {
            command_shaped = true;
        }
        if line.trim() == "exit status: 0" {
            return command_shaped;
        }
    }
    false
}

/// The one-shot refusal text: changed paths plus the original task,
/// head-bounded so an oversized instruction cannot balloon the turn.
fn completion_refusal(session: &Session) -> String {
    let paths = recent_changes(session);
    let instruction = session
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let instruction_head = instruction.chars().take(800).collect::<String>();
    format!(
        "Hold on — these files changed since the last green check: {}. \
         Before finishing, run the relevant checks (verify, cargo test, \
         cargo check) and fix what fails. If the checks pass, the task is \
         complete. Original task: {}",
        paths.join(", "),
        instruction_head,
    )
}

/// Workspace-relative paths the task has mutated so far (2F, ADR-017).
///
/// Derived from the recorded [`ToolEffects`](crate::tool::ToolEffects) on
/// each committed tool result — never diffed and never model-declared — so
/// verification links a failure to what the loop actually changed. Deduped
/// oldest-first and capped, most recent retained. Effects from calls in the
/// *current* batch are appended to the shared [`ToolContext`] list as those
/// calls finish (`ToolContext::note_effects`), covering same-batch writes.
fn recent_changes(session: &Session) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for item in &session.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        for path in result
            .effects
            .files_written
            .iter()
            .chain(result.effects.files_deleted.iter())
        {
            if seen.insert(path.clone()) {
                paths.push(path.clone());
            }
        }
    }
    if paths.len() > crate::tool::MAX_RECENT_CHANGES {
        paths.drain(..paths.len() - crate::tool::MAX_RECENT_CHANGES);
    }
    paths
}

/// The loop's refusal result for a repeated failed call: the tool never runs,
/// so no side effect occurs, and the model is told to change approach.
fn repeated_call_result(call: &ToolCall) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        ToolStatus::Error(ToolFailure::RepeatedCall),
        format!(
            "error: you already called \"{}\" with these exact arguments and it failed; \
             repeating it unchanged is not permitted. Reassess before calling again: read \
             the failure above, change the arguments, or use a different tool.",
            call.name
        ),
        false,
    )
}

/// Call ids whose results the model still sees in `request`, post-projection.
///
/// The tool engine serves a cache hit for one of these as a cheap
/// `unchanged since call <id>` stub (roadmap item 6): the full content is
/// already in view, so re-serving a bounded duplicate spends context twice.
///
/// A pruned result is the exception, and it is why `stubbed` is subtracted.
/// Pruning rewrites a result's body to a one-line "re-run the tool" note
/// while **keeping its call id** (`context::pruning::apply_stubs`), so the
/// id still appears in the projection even though its content is gone.
/// Serving `unchanged since call X` for such an id points the model at a
/// body it can no longer read — it would be told the content is available
/// at a call whose body is a stub. Excluding those ids makes a repeat read
/// fall through to the full, re-bounded cached content, which is exactly
/// what the model needs to recover what pruning took away.
fn visible_call_ids(request: &ModelRequest, stubbed: &[String]) -> HashSet<String> {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.call_id.as_str().to_owned()),
            _ => None,
        })
        .filter(|call_id| !stubbed.iter().any(|stubbed| stubbed == call_id))
        .collect()
}

/// Appends a deterministic next-step hint to a failure the model will see.
/// The engine (`execute_call`) is untouched, so its content contract is
/// unchanged; the loop layers guidance on top.
fn append_failure_hint(content: &mut String, kind: ToolFailure) {
    let hint = match kind {
        ToolFailure::InvalidInput => {
            "Hint: correct the arguments described above before calling again."
        }
        ToolFailure::Execution => {
            "Hint: inspect the error above and change your approach; the same call will be refused."
        }
        ToolFailure::Timeout => {
            "Hint: the call timed out; narrow the operation or change approach."
        }
        ToolFailure::UnknownTool | ToolFailure::NotExposed => {
            "Hint: call one of the available tools listed above."
        }
        // The refusal result already explains itself.
        ToolFailure::RepeatedCall => return,
    };
    // The engine bounded `content` before this, so reserve room for the hint
    // and keep the result within the same output bound.
    let needed = hint.len() + 1;
    if content.len() + needed > MAX_TOOL_OUTPUT_BYTES {
        let mut cut = MAX_TOOL_OUTPUT_BYTES.saturating_sub(needed);
        while cut > 0 && !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    content.push('\n');
    content.push_str(hint);
}

/// How one model attempt ended.
enum Attempt {
    /// The provider reported completion with an authoritative response.
    Completed(ModelResponse),
    /// The task's cancellation token won against the stream.
    Cancelled,
    /// The attempt failed; `response_started` records whether any delta had
    /// arrived before the failure.
    Failed {
        error: ModelError,
        response_started: bool,
    },
}

/// Per-task efficiency accumulators (§14). Kept separate from the
/// [`TaskRecord`] so the record is read immutably during compilation and
/// written back only at commit points.
#[derive(Default)]
struct Accounting {
    turns: u32,
    model_requests: u32,
    usage: Usage,
    successful_tool_calls: u32,
    failed_tool_calls: u32,
}

impl Accounting {
    /// Tool calls the loop executed: successes plus failures. Refused
    /// repeats of a failed call never ran, so they do not count.
    fn tool_calls(&self) -> u32 {
        self.successful_tool_calls
            .saturating_add(self.failed_tool_calls)
    }

    /// Mirrors the current totals into the session's task record.
    fn sync(&self, session: &mut Session, task_id: TaskId) {
        if let Some(record) = session.task_mut(task_id) {
            record.turns = self.turns;
            record.usage = self.usage;
        }
    }
}

/// A stable snake_case token for a stop reason, for protocol errors and the
/// `UnrecoverableStop` end reason.
fn stop_reason_token(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolCalls => "tool_calls",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ContextLimit => "context_limit",
        StopReason::ContentFilter => "content_filter",
    }
}

/// Milliseconds since the Unix epoch, saturating rather than panicking on a
/// pre-epoch or overflowing clock.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A duration's whole milliseconds, saturating at `u64::MAX`.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionGate, DEFAULT_RESERVED_OUTPUT_TOKENS, MAX_RESERVED_OUTPUT_TOKENS,
        OutputReservation, completion_refusal, derive_reserved_output_tokens, green_check,
        visible_call_ids,
    };
    use crate::model::{Message, ModelCapabilities, ModelRequest, StopReason};
    use crate::session::Session;
    use crate::transcript::{Item, ToolResult, ToolStatus};
    use std::collections::HashSet;

    #[test]
    fn auto_honors_a_declared_output_maximum_up_to_the_hard_cap() {
        // OpenRouter free-tier shape: declares 235_929 on a 262_144 window.
        let caps = ModelCapabilities::new(262_144).with_max_output_tokens(235_929);
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Auto),
            MAX_RESERVED_OUTPUT_TOKENS,
        );
        // Declaring exactly the default floor changes nothing.
        let caps = ModelCapabilities::new(128_000).with_max_output_tokens(8_192);
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Auto),
            DEFAULT_RESERVED_OUTPUT_TOKENS,
        );
    }

    #[test]
    fn auto_keeps_half_the_window_for_input() {
        // A declaration larger than half the window is clamped.
        let caps = ModelCapabilities::new(10_000).with_max_output_tokens(60_000);
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Auto),
            5_000
        );
        // No declared maximum: the default floor, still window-clamped.
        let caps = ModelCapabilities::new(10_000);
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Auto),
            5_000
        );
    }

    #[test]
    fn fixed_is_authoritative_but_window_clamped() {
        let caps = ModelCapabilities::new(10_000).with_max_output_tokens(60_000);
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Fixed(4_096)),
            4_096,
        );
        assert_eq!(
            derive_reserved_output_tokens(&caps, OutputReservation::Fixed(60_000)),
            5_000,
        );
    }

    fn result(call_id: &str, content: &str) -> Message {
        Message::ToolResult(ToolResult::new(
            crate::transcript::ToolCallId::try_from(call_id.to_owned()).unwrap(),
            ToolStatus::Success,
            content.to_owned(),
            false,
        ))
    }

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            model: "m".into(),
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            stable_prefix_messages: None,
        }
    }

    #[test]
    fn visible_call_ids_keeps_unstubbed_results() {
        let req = request(vec![result("call-a", "body"), result("call-b", "body")]);
        let ids = visible_call_ids(&req, &[]);
        assert_eq!(
            ids,
            ["call-a".to_owned(), "call-b".to_owned()]
                .into_iter()
                .collect::<HashSet<_>>(),
            "every projected result is still visible when nothing is stubbed"
        );
    }

    #[test]
    fn visible_call_ids_drops_a_pruned_result() {
        // Pruning keeps the call id but rewrites the body to a stub, so the
        // id is in the projection while the content is gone. Serving
        // `unchanged since call <id>` for it would point the model at a
        // body it can no longer read.
        let req = request(vec![
            result("call-a", "[pruned] aged out; re-run the tool"),
            result("call-b", "body"),
        ]);
        let ids = visible_call_ids(&req, &["call-a".to_owned()]);
        assert_eq!(
            ids,
            ["call-b".to_owned()].into_iter().collect::<HashSet<_>>(),
            "a stubbed call id must not be treated as still-visible"
        );
    }

    #[test]
    fn visible_call_ids_ignores_unmatched_stub_ids() {
        // A stub for a call that is no longer projected must not suppress
        // an unstubbed result that happens to share the projection.
        let req = request(vec![result("call-b", "body")]);
        let ids = visible_call_ids(&req, &["call-gone".to_owned()]);
        assert_eq!(
            ids,
            ["call-b".to_owned()].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn visible_call_ids_skips_non_tool_result_messages() {
        let req = request(vec![
            Message::User {
                text: "task".to_owned(),
            },
            result("call-a", "body"),
            Message::Assistant(crate::transcript::AssistantMessage {
                content: "done".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            }),
        ]);
        let ids = visible_call_ids(&req, &[]);
        assert_eq!(
            ids,
            ["call-a".to_owned()].into_iter().collect::<HashSet<_>>()
        );
    }

    fn gate_result(content: &str, written: &[&str]) -> ToolResult {
        let mut effects = crate::tool::ToolEffects::default();
        for path in written {
            effects = effects.with_file_written((*path).to_owned());
        }
        let mut r = ToolResult::new(
            crate::transcript::ToolCallId::try_from("call_1".to_owned()).unwrap(),
            ToolStatus::Success,
            content.to_owned(),
            false,
        );
        r.effects = effects;
        r
    }

    #[test]
    fn the_gate_demands_a_green_check_after_mutations() {
        let mut gate = CompletionGate::default();
        assert!(!gate.pending(), "no mutations yet: nothing to verify");
        gate.note(&gate_result("$ cargo check\nexit status: 0\n", &[]));
        assert!(!gate.pending(), "green without mutations is not pending");
        gate.note(&gate_result("edited src/lib.rs", &["src/lib.rs"]));
        assert!(gate.pending(), "a write after green is outstanding");
        gate.note(&gate_result("$ cargo test\nexit status: 0\n", &[]));
        assert!(!gate.pending(), "green after the write settles it");
        gate.note(&gate_result("$ cargo test\nexit status: 101\n", &[]));
        assert!(
            !gate.pending(),
            "a failing check alone does not reopen the gate: no files changed since the green"
        );
        gate.note(&gate_result("edited src/main.rs", &["src/main.rs"]));
        assert!(gate.pending(), "a write after the failing check reopens it");
    }

    #[test]
    fn a_fused_write_with_a_green_check_lands_verified() {
        let mut gate = CompletionGate::default();
        gate.note(&gate_result(
            "edited src/lib.rs\n--- verify ---\n$ cargo check\nexit status: 0",
            &["src/lib.rs"],
        ));
        assert!(!gate.pending(), "effects bump before the green pin");
    }

    #[test]
    fn green_checks_must_be_command_shaped() {
        // A tool result that merely mentions the marker is not a check.
        assert!(!green_check(&gate_result("exit status: 0", &[])));
        assert!(green_check(&gate_result(
            "$ cargo test\nexit status: 0",
            &[]
        )));
    }

    #[test]
    fn the_refusal_quotes_changed_paths_and_the_task() {
        let mut session = Session::new(crate::id::SessionId::new());
        session
            .push_item(Item::User {
                text: "fix the bug".to_owned(),
            })
            .unwrap();
        session
            .push_item(Item::ToolResult(gate_result(
                "edited src/lib.rs",
                &["src/lib.rs"],
            )))
            .unwrap();
        let text = completion_refusal(&session);
        assert!(text.contains("src/lib.rs"), "{text}");
        assert!(text.contains("fix the bug"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
    }
}
