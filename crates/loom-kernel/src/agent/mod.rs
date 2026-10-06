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
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::{
    AgentConfig, CompiledContext, ContextBudget, ContextCensus, ContextCompiler, ContextError,
    ContextInput, FactsSource, NoFacts,
};
use crate::event::{EventKind, EventSink};
use crate::id::{SessionId, TaskId};
use crate::model::{
    ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest, ModelResponse, StopReason,
    Usage,
};
use crate::session::{PlannedTurn, Session, TaskRecord, TerminalTaskStatus};
use crate::store::{ArtifactStore, SessionStore, StoreError};
use crate::tool::{ToolContext, ToolRegistry};
use crate::transcript::{AssistantMessage, Item, ToolCall, ToolFailure, ToolResult, ToolStatus};

pub mod retry;
pub mod tools;

pub use retry::RetryPolicy;

use tools::{MAX_TOOL_OUTPUT_BYTES, ToolBatch, ToolExposure};

/// Default maximum number of model turns per task when nothing else is
/// configured.
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Default output-token reservation: the share of the context window held
/// back for the model's reply (§8). It also bounds `max_tokens` on each
/// request whenever the model reports an output limit. Configurable via
/// [`Agent::with_reserved_output_tokens`].
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 8_192;

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
            TaskError::UnrecoverableStop { stop_reason } => stop_reason,
            TaskError::Persistence(_) => "persistence",
            TaskError::UnknownModel { .. } => "unknown_model",
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
    reserved_output_tokens: u32,
    tool_timeout: Duration,
    max_tool_concurrency: usize,
    store: Option<Box<dyn SessionStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    facts: Box<dyn FactsSource>,
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
            reserved_output_tokens: DEFAULT_RESERVED_OUTPUT_TOKENS,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            max_tool_concurrency: DEFAULT_MAX_TOOL_CONCURRENCY,
            store: None,
            artifacts: None,
            facts: Box::new(NoFacts),
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

    /// Sets the output-token reservation (§8).
    #[must_use]
    pub fn with_reserved_output_tokens(mut self, reserved_output_tokens: u32) -> Self {
        self.reserved_output_tokens = reserved_output_tokens;
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
        self.facts = facts;
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
    /// terminal state — including cancellation — returns a [`TaskError`];
    /// the [`TaskRecord`] in `session` carries the same outcome.
    ///
    /// The loop never blocks on the event sink and never treats delivery
    /// failure as a task failure (§11): events are best-effort, the
    /// transcript is the durable record.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
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
            .run_turns(
                session,
                task_id,
                sink,
                cancellation,
                &mut accounting,
                started,
            )
            .await;

        // 4. Finalize: record the outcome, emit the terminal event, and
        //    build the summary. The record is finished before the summary is
        //    returned so callers can read either.
        let wall_time = started.elapsed();
        let status = match &outcome {
            Ok(_) => TerminalTaskStatus::Completed,
            Err(error) if error.is_cancelled() => TerminalTaskStatus::Cancelled,
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
            Err(error) => sink.emit(
                task_id,
                EventKind::TaskFailed {
                    reason: error.to_string(),
                },
            ),
        }
        // Sampled after the terminal emit so a closed sink's drop of that
        // event is counted too (§11).
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
            remaining_budget: self.budget.remaining(&accounting, wall_time),
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
        cancellation: &CancellationToken,
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
        let budget = ContextBudget::from_window(
            capabilities.max_context_tokens,
            self.reserved_output_tokens,
        )?;

        // Repository facts for this task (2E, ADR-015): captured exactly once,
        // before the first compile, so the projected block is byte-stable for
        // the whole task (§8.1.1) and "what changed" never costs a model turn.
        let facts = self.facts.scan();

        // Loop-owned reaction to tool failures (§13): the loop, not the
        // model, decides whether a failed attempt may be repeated. An
        // identical call is refused until some tool call succeeds, so a
        // failure forces a change of approach instead of trial and error.
        let mut failures = FailureLedger::default();

        for turn in 1..=self.max_turns {
            // Enforce the task's resource envelope before spending anything
            // further (§14). Checked once per turn, so the unit in flight when
            // a ceiling was crossed still completed and stays committed.
            if let Some(limit) = self.budget.exhausted(accounting, started.elapsed()) {
                return Err(TaskError::BudgetExhausted { limit });
            }

            sink.emit(task_id, EventKind::TurnStarted { turn });

            // a. Compile the model request from durable state (§8). The
            //    compiler owns every context decision; the loop only passes
            //    it the inputs.
            let compiled: CompiledContext = {
                let task = session
                    .tasks
                    .iter()
                    .find(|task| task.id == task_id)
                    .ok_or_else(|| TaskError::Protocol("task record missing".to_owned()))?;
                self.compiler.compile(ContextInput {
                    session,
                    tools: &self.registry,
                    task,
                    budget,
                    capabilities: &capabilities,
                    model: &self.model,
                    config: &self.config,
                    facts: &facts,
                })?
            };
            // Partial move: `request` moves out, `census` (Copy) stays readable
            // for the `EndTurn` return.
            let request = compiled.request;

            // b. Stream with bounded retry (§6.3). Only a `Completed` event
            //    produces a response; failed attempts leave no trace.
            let response = self
                .stream_with_retry(&request, task_id, sink, cancellation, accounting)
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
                    let results = self
                        .execute_batch(
                            &assistant.tool_calls,
                            &request,
                            task_id,
                            sink,
                            cancellation,
                            accounting,
                            &mut failures,
                            &recent_changes(session),
                        )
                        .await;
                    // A batch interrupted by cancellation still commits its
                    // completed results plus cancelled fillers (§6.1, §10).
                    self.commit(session, assistant, results)?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    if cancellation.is_cancelled() {
                        return Err(TaskError::Cancelled);
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
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
        failures: &mut FailureLedger,
        recent_changes: &[String],
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
            if !failures.refuses(call) {
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

        let context = ToolContext::new(
            self.config.workspace_root.clone(),
            cancellation.clone(),
            self.tool_timeout,
        )
        .with_artifacts(self.artifacts.clone())
        .with_recent_changes(recent_changes.to_vec());
        let batch = ToolBatch::new(&self.registry, &exposure)
            .with_max_concurrency(self.max_tool_concurrency);
        let ran = batch.execute(&runnable, &context).await;

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
