//! Pure projection of Loom's event stream into runtime-facing TUI state.
//!
//! Event data is best-effort; the session transcript remains authoritative.
//! In particular, streamed model deltas live only in [`ProvisionalResponse`]
//! until a `ModelCompleted` event commits the complete assistant message.

use std::collections::{BTreeMap, BTreeSet};

const RECENT_EVENT_LIMIT: usize = 256;

use loom_core::{EventEnvelope, EventKind};

use loom_core::kernel::agent::RemainingBudget;
use loom_core::kernel::context::{
    ContextBudget, ContextCensus, HeuristicEstimator, TokenEstimator,
};
use loom_core::kernel::model::{ModelErrorKind, Usage};
use loom_core::kernel::session::TaskStatus;
/// Transcript facade aliases used by the reducer's public projection types.
pub use loom_core::kernel::transcript::{Item as TranscriptItem, ToolStatus};

/// Provisional streaming content for the currently active model attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionalResponse {
    /// Attempt number which produced the deltas.
    pub attempt: Option<u32>,
    /// Concatenated text deltas; never authoritative until completion.
    pub text: String,
    /// Concatenated reasoning deltas; never authoritative until completion.
    pub reasoning: String,
}

impl ProvisionalResponse {
    fn clear(&mut self) {
        self.attempt = None;
        self.text.clear();
        self.reasoning.clear();
    }

    fn is_empty(&self) -> bool {
        self.attempt.is_none() && self.text.is_empty() && self.reasoning.is_empty()
    }
}

/// Details of one model attempt which ended in a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryRecord {
    /// 1-based attempt that failed.
    pub attempt: u32,
    /// Stable model error classification.
    pub kind: String,
    /// Delay before the next attempt.
    pub delay_ms: u64,
    /// Whether a response had begun before the failure.
    pub response_started: bool,
    /// Turn whose model request was retried.
    pub turn: Option<u32>,
    /// Reasoning tokens discarded from the failed attempt. Estimated: a
    /// failed request reports no usage, so the count is the kernel's
    /// heuristic over the provisional deltas, not a billed figure.
    pub discarded_reasoning_tokens: u64,
    /// Assistant text tokens discarded from the failed attempt. Same
    /// estimate as [`Self::discarded_reasoning_tokens`].
    pub discarded_text_tokens: u64,
    /// HTTP status that produced the failure, when the provider reported one.
    pub http_status: Option<u16>,
}

/// Structured facts carried by a terminal `TaskFailed` event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskFailure {
    reason: String,
    model_kind: Option<ModelErrorKind>,
    http_status: Option<u16>,
    stop_reason: Option<String>,
}

/// A backoff wait between a failed attempt and the next one.
///
/// `now_millis` is injected by [`Reducer::tick`]; the reducer never reads a
/// clock, so replaying the same ticks produces the same countdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryWait {
    /// Attempt that just failed.
    pub failed_attempt: u32,
    /// Attempt that will start when the wait ends.
    pub next_attempt: u32,
    /// Stable model error classification for the failure.
    pub kind: String,
    /// Authoritative wait from `ModelRetry.delay_ms`.
    pub delay_ms: u64,
    /// Turn whose request is waiting.
    pub turn: Option<u32>,
    /// Event timestamp when the wait began.
    pub started_at_millis: u64,
    /// Most recent caller-injected time.
    pub now_millis: u64,
}

impl RetryWait {
    /// Milliseconds left in the wait, saturating at zero.
    #[must_use]
    pub fn remaining_millis(&self) -> u64 {
        self.started_at_millis
            .saturating_add(self.delay_ms)
            .saturating_sub(self.now_millis)
    }

    /// Fraction of the wait already elapsed, in `[0, 1]`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn progress(&self) -> f64 {
        if self.delay_ms == 0 {
            return 1.0;
        }
        let elapsed = self
            .now_millis
            .saturating_sub(self.started_at_millis)
            .min(self.delay_ms);
        elapsed as f64 / self.delay_ms as f64
    }
}

/// Terminal failure shape shown as a card in the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Provider stopped at the output-token ceiling.
    MaxTokens,
    /// Provider account has insufficient balance.
    QuotaExhausted,
    /// Provider filtered the reply.
    ContentFilter,
    /// Request exceeded the context window.
    ContextLimit,
    /// Every allowed attempt failed.
    RetriesExhausted,
    /// Any other task failure.
    Other,
}

/// Presentation-independent facts for one terminal failure card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureState {
    /// Stable failure shape.
    pub kind: FailureKind,
    /// Card headline.
    pub title: String,
    /// Optional qualifier such as `overloaded ×4`.
    pub subtitle: Option<String>,
    /// HTTP status when it can be recovered from the failure reason.
    pub http_status: Option<u16>,
    /// Plain-language explanation.
    pub body: String,
    /// Plain-language next step.
    pub action: String,
    /// Part of `action` rendered as a command.
    pub command: Option<String>,
    /// Last retry classification, for retry-exhausted cards.
    pub retry_kind: Option<String>,
    /// Attempts made before giving up.
    pub attempts: u32,
    /// Completed turns retained by the session.
    pub intact_turn: u32,
    /// Total task wall time at failure.
    pub elapsed_ms: Option<u64>,
}

/// TUI-side view of the effective retry policy for labels and attempt rows.
///
/// Replaced from `ModelRetry.policy` whenever the kernel reports it. The
/// defaults only fill traces written before schema v22 (and keep replay
/// deterministic); they are never used to decide a retry.
///
/// `idle_timeout_ms` is not a retry-policy value: it comes from the
/// provider's own stream liveness window (`ModelRetry.idle_timeout_ms`,
/// schema v23) and is shown in the same panel because both answer "how
/// long before this attempt gives up".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicyView {
    /// Total attempts allowed per model request.
    pub max_attempts: u32,
    /// First retry backoff in milliseconds.
    pub initial_backoff_ms: u64,
    /// Exponential growth factor.
    pub backoff_factor: u32,
    /// Cap on computed backoff in milliseconds.
    pub max_backoff_ms: u64,
    /// Jitter percentage around the computed backoff.
    pub jitter_percent: u32,
    /// Kernel liveness timeout, when one exists.
    pub idle_timeout_ms: Option<u64>,
}

impl Default for RetryPolicyView {
    fn default() -> Self {
        let max_attempts = loom_core::kernel::agent::RetryPolicy::default().max_attempts();
        Self {
            max_attempts,
            initial_backoff_ms: 2_000,
            backoff_factor: 2,
            max_backoff_ms: 30_000,
            jitter_percent: 25,
            idle_timeout_ms: None,
        }
    }
}

impl FailureState {
    #[allow(clippy::too_many_lines)]
    fn classify(
        failure: &TaskFailure,
        retries: &[RetryRecord],
        attempt: Option<u32>,
        policy: &RetryPolicyView,
        intact_turn: u32,
        elapsed_ms: Option<u64>,
    ) -> Self {
        let reason = failure.reason.as_str();
        let lower = reason.to_lowercase();
        let http_status = failure.http_status.or_else(|| http_status(reason));
        let stop_reason = failure.stop_reason.as_deref();
        if stop_reason == Some("max_tokens")
            || lower.contains("max_tokens")
            || lower.contains("max tokens")
        {
            return Self {
                kind: FailureKind::MaxTokens,
                title: "MaxTokens".to_owned(),
                subtitle: None,
                http_status,
                body:
                    "Reply hit the output limit. Partial text kept; tool calls dropped, none run."
                        .to_owned(),
                action: "Raise --max-tokens or narrow the task.".to_owned(),
                command: Some("--max-tokens".to_owned()),
                retry_kind: None,
                attempts: attempt.unwrap_or(1),
                intact_turn,
                elapsed_ms,
            };
        }
        if stop_reason == Some("content_filter")
            || lower.contains("content_filter")
            || lower.contains("content filter")
        {
            return Self {
                kind: FailureKind::ContentFilter,
                title: "ContentFilter".to_owned(),
                subtitle: None,
                http_status,
                body: "Reply was filtered. Stored, but hidden from later turns.".to_owned(),
                action: "Rephrase the task and run it again.".to_owned(),
                command: None,
                retry_kind: None,
                attempts: attempt.unwrap_or(1),
                intact_turn,
                elapsed_ms,
            };
        }
        if failure.model_kind == Some(ModelErrorKind::QuotaExhausted)
            || lower.contains("quota_exhausted")
            || lower.contains("quota exhausted")
        {
            return Self {
                kind: FailureKind::QuotaExhausted,
                title: "QuotaExhausted".to_owned(),
                subtitle: None,
                http_status,
                body: "DeepSeek reports insufficient balance. Not retried.".to_owned(),
                action: "Top up, or loom auth switch deepseek <label>".to_owned(),
                command: Some("loom auth switch deepseek <label>".to_owned()),
                retry_kind: None,
                attempts: attempt.unwrap_or(1),
                intact_turn,
                elapsed_ms,
            };
        }
        if stop_reason == Some("context_limit")
            || failure.model_kind == Some(ModelErrorKind::ContextOverflow)
            || lower.contains("context_limit")
            || lower.contains("context limit")
        {
            return Self {
                kind: FailureKind::ContextLimit,
                title: "ContextLimit".to_owned(),
                subtitle: None,
                http_status,
                body: "The request no longer fits the model's context window.".to_owned(),
                action: "Compact the session or narrow the task.".to_owned(),
                command: Some("/compact".to_owned()),
                retry_kind: None,
                attempts: attempt.unwrap_or(1),
                intact_turn,
                elapsed_ms,
            };
        }
        if let Some(failure) = retries_exhausted(
            failure,
            retries,
            attempt,
            policy,
            intact_turn,
            elapsed_ms,
            http_status,
        ) {
            return failure;
        }
        Self {
            kind: FailureKind::Other,
            title: "Task failed".to_owned(),
            subtitle: None,
            http_status,
            body: reason.to_owned(),
            action: "Review the failure and run the task again.".to_owned(),
            command: Some("/retry".to_owned()),
            retry_kind: None,
            attempts: attempt.unwrap_or(1),
            intact_turn,
            elapsed_ms,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn retries_exhausted(
    failure: &TaskFailure,
    retries: &[RetryRecord],
    attempt: Option<u32>,
    policy: &RetryPolicyView,
    intact_turn: u32,
    elapsed_ms: Option<u64>,
    http_status: Option<u16>,
) -> Option<FailureState> {
    let last = retries.last()?;
    let final_kind = failure
        .model_kind
        .map(|kind| kind.to_string())
        .or_else(|| reason_error_kind(&failure.reason).map(str::to_owned));
    if failure.model_kind.is_none() && !failure.reason.contains("model request failed")
        || final_kind.is_some_and(|kind| !is_retryable_kind(&kind))
    {
        return None;
    }
    let attempts = attempt
        .unwrap_or_else(|| u32::try_from(retries.len()).unwrap_or(u32::MAX))
        .max(u32::try_from(retries.len()).unwrap_or(u32::MAX))
        .max(policy.max_attempts);
    let duration = elapsed_ms.map_or_else(
        || "an unknown time".to_owned(),
        |millis| format!("{} s", millis / 1_000),
    );
    Some(FailureState {
        kind: FailureKind::RetriesExhausted,
        title: "Retries exhausted".to_owned(),
        subtitle: Some(format!("{} ×{attempts}", last.kind)),
        http_status,
        body: format!("Gave up after {duration}. Session intact at turn {intact_turn}."),
        action: "/retry re-run the task".to_owned(),
        command: Some("/retry".to_owned()),
        retry_kind: Some(last.kind.clone()),
        attempts,
        intact_turn,
        elapsed_ms,
    })
}

fn http_status(reason: &str) -> Option<u16> {
    let marker = reason.find("HTTP ")? + "HTTP ".len();
    reason[marker..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn reason_error_kind(reason: &str) -> Option<&str> {
    let start = reason.find('[')? + 1;
    let end = reason[start..].find(']')? + start;
    Some(&reason[start..end])
}

fn is_retryable_kind(kind: &str) -> bool {
    matches!(kind, "transport" | "rate_limited" | "overloaded" | "server")
}

impl RetryPolicyView {
    /// Base backoff before the retry that follows `failed_attempt`.
    #[must_use]
    pub fn base_backoff_ms(&self, failed_attempt: u32) -> u64 {
        let mut millis = u128::from(self.initial_backoff_ms);
        for _ in 1..failed_attempt {
            millis = millis.saturating_mul(u128::from(self.backoff_factor));
            if millis >= u128::from(self.max_backoff_ms) {
                return self.max_backoff_ms;
            }
        }
        u64::try_from(millis)
            .unwrap_or(u64::MAX)
            .min(self.max_backoff_ms)
    }
}

/// Runtime measurements for one agent turn, based only on emitted events.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnMetrics {
    /// Provider-reported usage from completed requests in this turn.
    pub usage: Usage,
    /// Whether any completed request in this turn reported usage metadata.
    pub usage_reported: bool,
    /// Turn start, in Unix epoch milliseconds.
    pub started_at_millis: Option<u64>,
    /// Turn end, in Unix epoch milliseconds.
    pub completed_at_millis: Option<u64>,
}

/// Event-backed hierarchy for one agent execution turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionTurn {
    /// Display key: the engine turn for live work, a session-wide key when
    /// restored history from several tasks is flattened into one timeline.
    pub number: u32,
    /// Engine turn number inside the run, when replayed from recorded events.
    pub engine_turn: Option<u32>,
    /// Restored run ordinal, when this turn belongs to recorded history.
    pub run: Option<u32>,
    /// User-authored follow-ups applied at this turn boundary.
    pub user_inputs: Vec<String>,
    /// Assistant messages and reconciled tool results belonging to this turn.
    pub transcript: Vec<TranscriptItem>,
}

/// One recorded task run in a restored session's history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredRun {
    /// Task identity from the recorded run.
    pub task_id: Option<loom_core::TaskId>,
    /// Terminal state recorded for the run.
    pub status: Option<TaskStatus>,
    /// Number of model turns replayed for the run.
    pub turns: u32,
    /// Failure reason recorded by the run, when one was provided.
    pub failure_reason: Option<String>,
    /// Recorded task start time (Unix epoch milliseconds).
    pub started_at_millis: Option<u64>,
    /// Recorded terminal time (Unix epoch milliseconds).
    pub completed_at_millis: Option<u64>,
}

/// Lifecycle projection for the task currently represented by this reducer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskLifecycle {
    /// Task identity from the latest task event.
    pub task_id: Option<loom_core::TaskId>,
    /// Durable-kernel-compatible lifecycle state.
    pub status: Option<TaskStatus>,
    /// Explicit caller-provided task start time (Unix epoch milliseconds).
    pub started_at_millis: Option<u64>,
    /// Explicit caller-provided terminal time (Unix epoch milliseconds).
    pub completed_at_millis: Option<u64>,
    /// Human-readable failure reason, where one was provided.
    pub failure_reason: Option<String>,
    /// Workspace files expanded into the request, shown separately from model output.
    pub attachments: Vec<FileAttachment>,
    /// User-authored prompts before any mentioned files are expanded.
    pub display_prompts: Vec<String>,
}

/// A workspace file included in a user-authored prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttachment {
    /// Workspace-relative file path.
    pub path: String,
    /// Number of bytes included in the model request.
    pub bytes: u64,
}

/// State for a tool call observed on the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolState {
    /// Tool name reported by the event.
    pub name: String,
    /// Serialized arguments reported when the call started, if available.
    pub arguments: Option<String>,
    /// Current structured status; absent means the call is running.
    pub status: Option<ToolStatus>,
    /// Bounded output head reported when the tool completed.
    pub content_head: Option<String>,
    /// Tool start time from the event stream.
    pub started_at_millis: Option<u64>,
    /// Tool completion time from the event stream.
    pub completed_at_millis: Option<u64>,
}

/// Authoritative context and task-budget snapshot emitted after compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextProgress {
    /// Turn whose request was compiled.
    pub turn: u32,
    /// Input allowance and output reservation for the selected model.
    pub context_budget: ContextBudget,
    /// Compiler-reported request census.
    pub census: ContextCensus,
    /// Remaining task-level headroom at compile time.
    pub remaining_budget: RemainingBudget,
    /// Task elapsed time when this budget snapshot was observed, when task start is known.
    pub elapsed_at_compile: Option<std::time::Duration>,
    /// Tool calls already observed when this budget snapshot was emitted.
    pub tool_calls_used: u64,
}

/// Pure, deterministic view of one Loom task event stream.
///
/// Every time value is supplied to [`Reducer::reduce`] by its caller. The
/// reducer performs no clock, environment, filesystem, or process reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reducer {
    /// The latest task lifecycle state.
    pub task: TaskLifecycle,
    /// Committed transcript projection. For exact history after reload, use
    /// [`Reducer::reconcile_session`]. Event tool completions contain only a
    /// bounded content head, so this projection cannot replace the session.
    pub committed_transcript: Vec<TranscriptItem>,
    /// Whether the transcript came from an existing session rather than the
    /// current task, so idle home must not replace the restored history.
    pub restored: bool,
    /// Run metadata for restored history, keyed by run ordinal.
    pub restored_runs: BTreeMap<u32, RestoredRun>,
    /// Turn hierarchy projected from engine turn and steering events.
    pub execution_turns: BTreeMap<u32, ExecutionTurn>,
    /// Original task instruction, displayed once above execution turns.
    pub original_instruction: Option<String>,
    /// Uncommitted stream fragments for the current attempt.
    pub provisional: ProvisionalResponse,
    /// Retry history in event order.
    pub retries: Vec<RetryRecord>,
    /// Backoff wait currently in progress, if any.
    pub retry_wait: Option<RetryWait>,
    /// Retry policy labels shown by the sidebar.
    pub retry_policy: RetryPolicyView,
    /// Terminal failure card, when the task failed.
    pub failure: Option<FailureState>,
    /// Tool state indexed by call ID.
    pub tools: BTreeMap<String, ToolState>,
    /// Number of tool executions started for this task.
    pub tool_calls_started: u64,
    /// Whether a follow-up has been queued by the runtime/UI.
    pub follow_up_queued: bool,
    /// Most recently observed turn number.
    pub turn: Option<u32>,
    /// Most recently observed request attempt number.
    pub attempt: Option<u32>,
    /// Accumulated provider-reported usage from authoritative model responses.
    pub usage: Usage,
    /// Whether any authoritative response reported non-zero usage or a cost.
    pub usage_reported: bool,
    /// Number of envelopes reduced for this task.
    pub event_count: u64,
    /// Latest event sequence number reported by Loom.
    pub sequence: u64,
    /// Model id confirmed by `CapabilitiesResolved`, when observed.
    pub observed_model: Option<String>,
    /// Latest caller-supplied or event-supplied timestamp, for live
    /// elapsed-time rendering. Purely presentation state.
    pub now_millis: u64,
    /// Most recent event labels, newest first.
    pub recent_events: Vec<String>,
    /// Latest compiled-request and remaining-budget snapshot, if observed.
    pub context_progress: Option<ContextProgress>,
    /// Final successful engine summary, if the run completed successfully.
    pub summary: Option<loom_core::TaskSummary>,
    /// Per-turn measurements from the event stream.
    pub turn_metrics: BTreeMap<u32, TurnMetrics>,
    /// `(turn, summary chars)` for each LLM handoff compaction, in order.
    pub compactions: Vec<(u32, usize)>,
    /// Turn ownership of authoritative assistant items, in transcript order.
    assistant_turns: Vec<u32>,
    /// Parent turn for each observed call ID.
    tool_call_turns: BTreeMap<String, u32>,
    /// Next visible queued prompt consumed by a `SteeringApplied` event.
    applied_display_prompts: usize,
    /// UI expansion toggles for optional transcript details.
    pub expanded: ExpandedDetails,
    /// Tokens in the cacheable request prefix, from the latest compile.
    pub cacheable_prefix_tokens: Option<u64>,
    /// Whether the prefix matched the previous compile. `None` means the
    /// event stream does not yet contain a second sample.
    pub cacheable_prefix_stable: Option<bool>,
    /// Keyboard cursor over turns and tool calls; presentation state only.
    pub selected: Option<Selection>,
    /// Tool call whose result excerpt is expanded.
    pub expanded_result: Option<String>,
}

/// Independently controlled optional details in the execution timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpandedDetails {
    /// Show explicitly provider-emitted reasoning traces.
    pub reasoning: bool,
    /// Show complete tool arguments and available output previews.
    pub tools: bool,
}

/// A selectable row in the live thread, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// A turn node.
    Turn(u32),
    /// One tool call inside a turn.
    ToolCall {
        /// Display key of the parent turn.
        turn: u32,
        /// Correlation id of the call.
        call_id: String,
    },
}

impl Reducer {
    /// Records a locally observed event label (approval decisions) in the
    /// same newest-first list the event stream feeds.
    pub fn note_event(&mut self, label: impl Into<String>) {
        self.recent_events.insert(0, label.into());
        self.recent_events.truncate(RECENT_EVENT_LIMIT);
    }

    /// Adds the user-authored task to the live projection before engine events
    /// arrive. Session reconciliation later replaces this with committed data.
    pub fn begin_task(&mut self, task: impl Into<String>) {
        let task = task.into();
        self.task.display_prompts.push(task.clone());
        self.original_instruction = Some(task.clone());
        self.applied_display_prompts = 1;
        self.committed_transcript
            .push(TranscriptItem::User { text: task });
    }

    /// Records files expanded from `@path` mentions for distinct thread rendering.
    pub fn set_attachments(&mut self, attachments: Vec<FileAttachment>) {
        self.task.attachments = attachments;
    }

    /// Preserves the visible text for a queued follow-up prompt.
    pub fn push_display_prompt(&mut self, prompt: impl Into<String>) {
        self.task.display_prompts.push(prompt.into());
    }

    /// Replaces only the first visible user prompt with the pre-expansion text.
    ///
    /// The engine/session transcript retains the expanded request for durable
    /// history and model context; the interactive thread should show the
    /// user's authored prompt instead.
    pub fn restore_display_prompt(&mut self) {
        if let (Some(display_prompt), Some(TranscriptItem::User { text })) = (
            self.task.display_prompts.first(),
            self.committed_transcript
                .iter_mut()
                .find(|item| matches!(item, TranscriptItem::User { .. })),
        ) {
            text.clone_from(display_prompt);
        }
        let mut visible_follow_up = 1;
        for turn in self.execution_turns.values_mut() {
            for prompt in &mut turn.user_inputs {
                if let Some(display_prompt) = self.task.display_prompts.get(visible_follow_up) {
                    prompt.clone_from(display_prompt);
                }
                visible_follow_up = visible_follow_up.saturating_add(1);
            }
        }
    }

    /// Applies one event using the caller's explicit Unix epoch timestamp.
    ///
    /// `timestamp_millis` is the envelope observation time, used for task
    /// lifecycle, turn, and tool durations; all other state comes solely
    /// from the event and current reducer state.
    #[allow(clippy::too_many_lines)] // one exhaustive event-to-state transition table
    pub fn reduce(&mut self, event: &EventEnvelope, timestamp_millis: u64) {
        self.now_millis = self.now_millis.max(timestamp_millis);
        self.sequence = event.sequence;
        self.event_count = self.event_count.saturating_add(1);
        self.recent_events.insert(
            0,
            format!("#{} {}", event.sequence, event_name(&event.kind)),
        );
        self.recent_events.truncate(RECENT_EVENT_LIMIT);
        match &event.kind {
            EventKind::CapabilitiesResolved { model, .. } => {
                self.observed_model = Some(model.clone());
            }
            EventKind::ContextProgress {
                turn,
                context_budget,
                census,
                remaining_budget,
            } => {
                self.context_progress = Some(ContextProgress {
                    turn: *turn,
                    context_budget: *context_budget,
                    census: *census,
                    remaining_budget: *remaining_budget,
                    elapsed_at_compile: self.task.started_at_millis.map(|started| {
                        std::time::Duration::from_millis(timestamp_millis.saturating_sub(started))
                    }),
                    tool_calls_used: self.tool_calls_started,
                });
                // TODO: the wire format carries no prefix-stability signal.
                // stability here means "the same three prefix categories
                // compiled to the same token count as last request".
                let prefix = u64::from(census.system_prompt)
                    + u64::from(census.repo_facts)
                    + u64::from(census.tool_specs);
                self.cacheable_prefix_stable = self
                    .cacheable_prefix_tokens
                    .map(|previous| previous == prefix);
                self.cacheable_prefix_tokens = Some(prefix);
            }
            EventKind::TaskStarted => {
                self.task.task_id = Some(event.task_id);
                self.task.status = Some(TaskStatus::Running);
                self.task.started_at_millis = Some(timestamp_millis);
                self.task.completed_at_millis = None;
                self.task.failure_reason = None;
                self.selected = None;
                self.expanded_result = None;
                self.cacheable_prefix_tokens = None;
                self.cacheable_prefix_stable = None;
                self.retries.clear();
                self.retry_wait = None;
                self.failure = None;
                self.attempt = None;
                self.provisional.clear();
            }
            EventKind::TurnStarted { turn } => {
                if let Some(previous) = self.turn
                    && let Some(metrics) = self.turn_metrics.get_mut(&previous)
                {
                    metrics.completed_at_millis = Some(timestamp_millis);
                }
                self.turn = Some(*turn);
                self.execution_turns
                    .entry(*turn)
                    .or_insert_with(|| ExecutionTurn {
                        number: *turn,
                        engine_turn: Some(*turn),
                        ..ExecutionTurn::default()
                    });
                self.turn_metrics
                    .entry(*turn)
                    .or_default()
                    .started_at_millis = Some(timestamp_millis);
            }
            EventKind::ModelRequestStarted { attempt } => {
                let attempt = (*attempt).min(self.retry_policy.max_attempts).max(1);
                self.attempt = Some(attempt);
                self.retry_wait = None;
                self.provisional.clear();
                self.provisional.attempt = Some(attempt);
            }
            EventKind::TextDelta { text } => {
                self.provisional.text.push_str(text);
            }
            EventKind::ReasoningDelta { text } => {
                self.provisional.reasoning.push_str(text);
            }
            EventKind::ModelRetry {
                attempt,
                kind,
                delay_ms,
                response_started,
                http_status,
                policy,
                idle_timeout_ms,
            } => {
                if let Some(policy) = policy {
                    self.retry_policy = RetryPolicyView {
                        max_attempts: policy.max_attempts,
                        initial_backoff_ms: policy.initial_backoff_ms,
                        backoff_factor: policy.backoff_factor,
                        max_backoff_ms: policy.max_backoff_ms,
                        jitter_percent: policy.jitter_percent,
                        idle_timeout_ms: None,
                    };
                }
                self.retry_policy.idle_timeout_ms = *idle_timeout_ms;
                // A failed attempt reports no usage, so the discarded count
                // can only be estimated; the kernel's heuristic keeps it
                // deterministic and provider-independent.
                let estimator = HeuristicEstimator;
                let discarded_reasoning_tokens =
                    u64::from(estimator.estimate(&self.provisional.reasoning));
                let discarded_text_tokens = u64::from(estimator.estimate(&self.provisional.text));
                let attempt = (*attempt).min(self.retry_policy.max_attempts).max(1);
                self.retries.push(RetryRecord {
                    attempt,
                    kind: kind.to_string(),
                    delay_ms: *delay_ms,
                    response_started: *response_started,
                    turn: self.turn,
                    discarded_reasoning_tokens,
                    discarded_text_tokens,
                    http_status: *http_status,
                });
                self.provisional.clear();
                let next_attempt = attempt
                    .saturating_add(1)
                    .min(self.retry_policy.max_attempts);
                self.attempt = Some(next_attempt);
                if self.task.status == Some(TaskStatus::Running) {
                    self.retry_wait = Some(RetryWait {
                        failed_attempt: attempt,
                        next_attempt,
                        kind: kind.to_string(),
                        delay_ms: *delay_ms,
                        turn: self.turn,
                        started_at_millis: timestamp_millis,
                        now_millis: timestamp_millis,
                    });
                }
            }
            EventKind::ModelCompleted { response } => {
                // Deltas are intentionally not used here: the complete
                // response is the sole authority for committed output.
                self.provisional.clear();
                self.retry_wait = None;
                self.retries.clear();
                self.usage_reported |= response.usage.input_tokens > 0
                    || response.usage.output_tokens > 0
                    || response.usage.cached_input_tokens.is_some()
                    || response.usage.thinking_tokens.is_some()
                    || response.usage.cache_creation_input_tokens.is_some()
                    || response.usage.cost_usd.is_some();
                self.usage = self.usage + response.usage;
                if let Some(turn) = self.turn {
                    let metrics = self.turn_metrics.entry(turn).or_default();
                    metrics.usage = metrics.usage + response.usage;
                    metrics.usage_reported |= response.usage.input_tokens > 0
                        || response.usage.output_tokens > 0
                        || response.usage.cached_input_tokens.is_some()
                        || response.usage.thinking_tokens.is_some()
                        || response.usage.cache_creation_input_tokens.is_some()
                        || response.usage.cost_usd.is_some();
                    if let Some(execution_turn) = self.execution_turns.get_mut(&turn) {
                        execution_turn
                            .transcript
                            .push(TranscriptItem::Assistant(response.message.clone()));
                    }
                    self.assistant_turns.push(turn);
                }
                self.committed_transcript
                    .push(TranscriptItem::Assistant(response.message.clone()));
                for call in &response.message.tool_calls {
                    if let Some(turn) = self.turn {
                        self.tool_call_turns.insert(call.id.to_string(), turn);
                    }
                    self.tools
                        .entry(call.id.to_string())
                        .or_insert_with(|| ToolState {
                            name: call.name.clone(),
                            // Exact arguments arrive in ToolStarted. This
                            // response event supplies identity and name only.
                            arguments: None,
                            status: None,
                            content_head: None,
                            started_at_millis: None,
                            completed_at_millis: None,
                        });
                }
            }
            EventKind::ToolStarted {
                name,
                call_id,
                arguments,
            } => {
                self.tool_calls_started = self.tool_calls_started.saturating_add(1);
                if let Some(turn) = self.turn {
                    self.tool_call_turns.insert(call_id.to_string(), turn);
                }
                self.tools.insert(
                    call_id.to_string(),
                    ToolState {
                        name: name.clone(),
                        arguments: Some(arguments.clone()),
                        status: None,
                        content_head: None,
                        started_at_millis: Some(timestamp_millis),
                        completed_at_millis: None,
                    },
                );
            }
            EventKind::ToolCompleted {
                name,
                call_id,
                status,
                content_head,
            } => {
                if let Some(turn) = self.turn {
                    self.tool_call_turns.insert(call_id.to_string(), turn);
                }
                let tool = self
                    .tools
                    .entry(call_id.to_string())
                    .or_insert_with(|| ToolState {
                        name: name.clone(),
                        arguments: None,
                        status: None,
                        content_head: None,
                        started_at_millis: None,
                        completed_at_millis: None,
                    });
                tool.name.clone_from(name);
                tool.status = Some(*status);
                tool.content_head = Some(content_head.clone());
                tool.completed_at_millis = Some(timestamp_millis);
            }
            EventKind::SteeringApplied { turn, messages } => {
                self.follow_up_queued = false;
                let execution_turn =
                    self.execution_turns
                        .entry(*turn)
                        .or_insert_with(|| ExecutionTurn {
                            number: *turn,
                            engine_turn: Some(*turn),
                            ..ExecutionTurn::default()
                        });
                for _ in 0..*messages {
                    if let Some(prompt) =
                        self.task.display_prompts.get(self.applied_display_prompts)
                    {
                        execution_turn.user_inputs.push(prompt.clone());
                    }
                    self.applied_display_prompts = self.applied_display_prompts.saturating_add(1);
                }
            }
            EventKind::TaskCompleted => {
                self.finish_task(event, TaskStatus::Completed, timestamp_millis, None);
            }
            EventKind::TaskFailed {
                reason,
                model_kind,
                http_status,
                stop_reason,
            } => {
                self.finish_task(
                    event,
                    TaskStatus::Failed,
                    timestamp_millis,
                    Some(&TaskFailure {
                        reason: reason.clone(),
                        model_kind: *model_kind,
                        http_status: *http_status,
                        stop_reason: stop_reason.clone(),
                    }),
                );
            }
            EventKind::TaskCancelled => {
                self.finish_task(event, TaskStatus::Cancelled, timestamp_millis, None);
            }
            EventKind::TaskPaused => {
                self.finish_task(event, TaskStatus::Paused, timestamp_millis, None);
            }
            EventKind::ContextHandoff { turn, chars } => {
                self.compactions.push((*turn, *chars));
            }
            EventKind::ContextPruned { .. }
            | EventKind::FastPathHit { .. }
            | EventKind::FastPathDeclined { .. }
            | EventKind::HarnessNotice { .. }
            | EventKind::AutofixApplied { .. }
            | EventKind::TodoUpdated { .. }
            | EventKind::TurnBudgetExtended { .. }
            | EventKind::LoopNudge { .. }
            | EventKind::AwaitingInput { .. }
            | _ => {}
        }
        if matches!(&event.kind, EventKind::ContextPruned { .. }) {
            self.cacheable_prefix_stable = Some(false);
        }
    }

    /// Advances time-dependent presentation state with a caller-supplied
    /// monotonic timestamp. No clock is read here; replay stays deterministic.
    pub fn tick(&mut self, now_millis: u64) {
        self.now_millis = self.now_millis.max(now_millis);
        if let Some(wait) = &mut self.retry_wait {
            wait.now_millis = now_millis.max(wait.started_at_millis);
        }
    }

    /// Whether a backoff wait is currently pending.
    #[must_use]
    pub fn is_retrying(&self) -> bool {
        self.retry_wait.is_some() && self.task.status == Some(TaskStatus::Running)
    }

    /// Number of attempts made so far, including a failed final attempt.
    #[must_use]
    pub fn attempts_used(&self) -> u32 {
        self.attempt
            .unwrap_or_else(|| u32::try_from(self.retries.len()).unwrap_or(u32::MAX))
            .max(u32::try_from(self.retries.len()).unwrap_or(u32::MAX))
    }

    /// Updates the externally-owned queued follow-up indicator.
    ///
    /// Queue operations are UI/control actions rather than Loom events. The
    /// runtime supplies their result explicitly; `SteeringApplied` clears it.
    pub fn set_follow_up_queued(&mut self, queued: bool) {
        self.follow_up_queued = queued;
    }

    /// Toggles display of explicitly provider-emitted reasoning traces.
    pub fn toggle_reasoning(&mut self) {
        self.expanded.reasoning = !self.expanded.reasoning;
    }

    /// Toggles expanded tool arguments and output previews.
    pub fn toggle_details(&mut self) {
        self.expanded.tools = !self.expanded.tools;
    }

    /// Rows the keyboard cursor can reach, in display order.
    #[must_use]
    pub fn selection_targets(&self) -> Vec<Selection> {
        let mut targets = Vec::new();
        for (turn_number, turn) in &self.execution_turns {
            targets.push(Selection::Turn(*turn_number));
            let mut seen = BTreeSet::new();
            for item in &turn.transcript {
                if let TranscriptItem::Assistant(message) = item {
                    for call in &message.tool_calls {
                        let id = call.id.to_string();
                        if seen.insert(id.clone()) {
                            targets.push(Selection::ToolCall {
                                turn: *turn_number,
                                call_id: id,
                            });
                        }
                    }
                }
            }
            // Calls can be observed before ModelCompleted commits the message.
            for call_id in self.tools.keys() {
                if self.tool_call_turns.get(call_id) == Some(turn_number)
                    && seen.insert(call_id.clone())
                {
                    targets.push(Selection::ToolCall {
                        turn: *turn_number,
                        call_id: call_id.clone(),
                    });
                }
            }
        }
        targets
    }

    /// Moves the cursor one row forward or backward, stopping at the ends.
    pub fn move_selection(&mut self, forward: bool) {
        let targets = self.selection_targets();
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| targets.iter().position(|row| row == selected));
        let next = match (current, forward) {
            (Some(index), true) => (index + 1).min(targets.len().saturating_sub(1)),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => targets.len().saturating_sub(1),
        };
        if let Some(row) = targets.get(next) {
            self.selected = Some(row.clone());
        }
    }

    /// Call id under the cursor, when it is a tool row.
    #[must_use]
    pub fn selected_call_id(&self) -> Option<&str> {
        match &self.selected {
            Some(Selection::ToolCall { call_id, .. }) => Some(call_id),
            Some(Selection::Turn(_)) | None => None,
        }
    }

    /// Expands or collapses the selected call's result excerpt.
    pub fn toggle_selected_result(&mut self) {
        let Some(call_id) = self.selected_call_id().map(str::to_owned) else {
            return;
        };
        if self.expanded_result.as_deref() == Some(call_id.as_str()) {
            self.expanded_result = None;
        } else {
            self.expanded_result = Some(call_id);
        }
    }

    /// Retains the engine's authoritative successful run record.
    pub fn set_summary(&mut self, summary: loom_core::TaskSummary) {
        self.summary = Some(summary);
    }

    /// Rebuilds committed display history from the source-of-truth session.
    ///
    /// This replaces event-derived transcript items (which may lack full
    /// tool result bodies) with the exact committed transcript and discards
    /// any provisional stream content.
    #[allow(clippy::wildcard_enum_match_arm)] // Item is non_exhaustive: future items are skipped on rebuild
    pub fn reconcile_session(&mut self, session: &loom_core::Session) {
        self.committed_transcript.clone_from(&session.transcript);
        // Keep the assistant messages this task actually projected. Matching
        // session assistants by equality would pull in messages from earlier
        // tasks in the same session once they share an execution turn.
        let assistants = self
            .execution_turns
            .values()
            .map(|turn| {
                turn.transcript
                    .iter()
                    .filter(|item| matches!(item, TranscriptItem::Assistant(_)))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for turn in self.execution_turns.values_mut() {
            turn.transcript.clear();
        }
        for (turn, items) in self.execution_turns.values_mut().zip(assistants) {
            turn.transcript.extend(items);
        }
        // Replace event-projected result heads with the committed bodies.
        for item in &session.transcript {
            let TranscriptItem::ToolResult(result) = item else {
                continue;
            };
            let Some(turn) = self.tool_call_turns.get(&result.call_id.to_string()) else {
                continue;
            };
            if let Some(execution_turn) = self.execution_turns.get_mut(turn) {
                execution_turn.transcript.push(item.clone());
            }
        }
        self.provisional.clear();
        self.restore_display_prompt();
    }

    /// Restores a persisted session without an event stream.
    ///
    /// Kept for callers and old stores where no events were recorded. The
    /// fallback has no turn boundaries, so turns are labelled as history
    /// rather than renumbered as if they were engine turns.
    pub fn restore_session(&mut self, session: &loom_core::Session) {
        self.restore_session_with_events(session, &[]);
    }

    /// Restores a persisted session from its recorded event stream.
    ///
    /// Events are grouped by task and replayed in store order, so every
    /// restored turn keeps its task-local engine number and every task stays
    /// a separate run. Envelope timestamps restore durations; rows written
    /// before schema v21 have none, so those runs leave durations unset.
    #[allow(clippy::too_many_lines, clippy::wildcard_enum_match_arm)] // replay table; Item is non_exhaustive
    pub fn restore_session_with_events(
        &mut self,
        session: &loom_core::Session,
        events: &[EventEnvelope],
    ) {
        *self = Self::default();
        self.restored = true;
        self.committed_transcript.clone_from(&session.transcript);
        if events.is_empty() {
            self.restore_session_from_transcript(session);
            return;
        }

        let prompts = session
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::User { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut groups: Vec<(loom_core::TaskId, Vec<&EventEnvelope>)> = Vec::new();
        for event in events {
            if let Some((_, group)) = groups
                .iter_mut()
                .find(|(task_id, _)| *task_id == event.task_id)
            {
                group.push(event);
            } else {
                groups.push((event.task_id, vec![event]));
            }
        }
        let records = session
            .tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<BTreeMap<_, _>>();
        let mut prompt_cursor = 0;
        let mut display_offset = 0u32;
        let mut last_run = None;

        for (index, (task_id, task_events)) in groups.into_iter().enumerate() {
            let run_index = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let mut run = Reducer {
                restored: true,
                ..Reducer::default()
            };
            let prompt = prompts.get(prompt_cursor).cloned();
            if let Some(prompt) = &prompt {
                run.begin_task(prompt.clone());
                // Keep later prompts available for SteeringApplied to consume;
                // truncate back to the consumed prefix after replay.
                for extra in prompts.iter().skip(prompt_cursor.saturating_add(1)) {
                    run.task.display_prompts.push(extra.clone());
                }
            }
            let timestamps_complete = task_events
                .iter()
                .all(|event| event.timestamp_millis.is_some());
            for event in &task_events {
                run.reduce(event, event.timestamp_millis.unwrap_or(0));
            }

            // Rows written before schema v21 carry no timestamp. Usage and
            // status are still authoritative; durations are not available.
            if !timestamps_complete {
                run.task.started_at_millis = None;
                run.task.completed_at_millis = None;
                for metrics in run.turn_metrics.values_mut() {
                    metrics.started_at_millis = None;
                    metrics.completed_at_millis = None;
                }
                if let Some(progress) = run.context_progress.as_mut() {
                    progress.elapsed_at_compile = None;
                }
                for tool in run.tools.values_mut() {
                    tool.started_at_millis = None;
                    tool.completed_at_millis = None;
                }
            }
            for tool in run.tools.values_mut() {
                if tool.status.is_none() {
                    tool.status = Some(ToolStatus::Cancelled {
                        started: tool.arguments.is_some(),
                    });
                }
            }
            if let Some(record) = records.get(&task_id) {
                run.task.task_id = Some(record.id);
                run.task.status = Some(record.status);
                run.task.started_at_millis = Some(record.started_at_millis);
                run.task.completed_at_millis = record.completed_at_millis;
            }

            // Event completions contain only a bounded output head. Overlay
            // the exact committed result body by call id.
            for item in &session.transcript {
                let TranscriptItem::ToolResult(result) = item else {
                    continue;
                };
                let Some(engine_turn) = run
                    .tool_call_turns
                    .get(&result.call_id.to_string())
                    .copied()
                else {
                    continue;
                };
                if let Some(turn) = run.execution_turns.get_mut(&engine_turn) {
                    turn.transcript.push(item.clone());
                }
                if let Some(tool) = run.tools.get_mut(&result.call_id.to_string()) {
                    tool.status = Some(result.status);
                    tool.content_head = Some(result.content.clone());
                }
            }

            let consumed = if prompt.is_some() {
                run.applied_display_prompts.max(1)
            } else {
                run.applied_display_prompts
            };
            prompt_cursor = prompt_cursor.saturating_add(consumed);
            run.task.display_prompts.truncate(consumed);
            run.original_instruction = None;
            if let Some(prompt) = &prompt
                && let Some(first) = run.execution_turns.values_mut().next()
                && first.user_inputs.first() != Some(prompt)
            {
                first.user_inputs.insert(0, prompt.clone());
            }

            let turns = std::mem::take(&mut run.execution_turns);
            let mut mapping = BTreeMap::new();
            if turns.is_empty() {
                if let Some(prompt) = prompt {
                    display_offset = display_offset.saturating_add(1);
                    self.execution_turns.insert(
                        display_offset,
                        ExecutionTurn {
                            number: display_offset,
                            engine_turn: None,
                            run: Some(run_index),
                            user_inputs: vec![prompt],
                            transcript: Vec::new(),
                        },
                    );
                }
            } else {
                for (engine_turn, mut turn) in turns {
                    display_offset = display_offset.saturating_add(1);
                    mapping.insert(engine_turn, display_offset);
                    turn.number = display_offset;
                    turn.engine_turn = Some(engine_turn);
                    turn.run = Some(run_index);
                    self.execution_turns.insert(display_offset, turn);
                }
            }

            for (engine_turn, metrics) in std::mem::take(&mut run.turn_metrics) {
                if let Some(display_turn) = mapping.get(&engine_turn) {
                    self.turn_metrics.insert(*display_turn, metrics);
                }
            }
            for mut retry in std::mem::take(&mut run.retries) {
                retry.turn = retry
                    .turn
                    .and_then(|engine_turn| mapping.get(&engine_turn).copied());
                self.retries.push(retry);
            }
            for (engine_turn, chars) in std::mem::take(&mut run.compactions) {
                if let Some(display_turn) = mapping.get(&engine_turn) {
                    self.compactions.push((*display_turn, chars));
                }
            }
            for (call_id, tool) in std::mem::take(&mut run.tools) {
                self.tools.insert(call_id, tool);
            }
            for (call_id, engine_turn) in std::mem::take(&mut run.tool_call_turns) {
                if let Some(display_turn) = mapping.get(&engine_turn) {
                    self.tool_call_turns.insert(call_id, *display_turn);
                }
            }
            for engine_turn in std::mem::take(&mut run.assistant_turns) {
                if let Some(display_turn) = mapping.get(&engine_turn) {
                    self.assistant_turns.push(*display_turn);
                }
            }
            self.tool_calls_started = self
                .tool_calls_started
                .saturating_add(run.tool_calls_started);
            self.event_count = self.event_count.saturating_add(run.event_count);
            self.sequence = run.sequence;
            self.observed_model = run.observed_model.take().or(self.observed_model.take());
            self.context_progress = run.context_progress.or(self.context_progress);
            self.summary = run.summary.take().or(self.summary.take());
            self.recent_events = std::mem::take(&mut run.recent_events);
            self.attempt = run.attempt;

            self.restored_runs.insert(
                run_index,
                RestoredRun {
                    task_id: run.task.task_id.or(Some(task_id)),
                    status: run.task.status,
                    turns: u32::try_from(mapping.len()).unwrap_or(u32::MAX),
                    failure_reason: run.task.failure_reason.clone(),
                    started_at_millis: run.task.started_at_millis,
                    completed_at_millis: run.task.completed_at_millis,
                },
            );
            last_run = Some(run);
        }

        if let Some(run) = last_run {
            self.task = run.task;
            self.turn = run.turn;
            self.usage = run.usage;
            self.usage_reported = run.usage_reported;
        }
        self.committed_transcript.clone_from(&session.transcript);
        self.provisional.clear();
    }

    /// Prompt-grouped fallback for sessions recorded before the event store.
    #[allow(clippy::wildcard_enum_match_arm)] // Item is non_exhaustive: future items are skipped
    fn restore_session_from_transcript(&mut self, session: &loom_core::Session) {
        self.original_instruction = None;
        let mut calls = BTreeMap::new();
        for item in &session.transcript {
            if let TranscriptItem::Assistant(message) = item {
                for call in &message.tool_calls {
                    calls.insert(
                        call.id.to_string(),
                        (call.name.clone(), call.arguments.to_string()),
                    );
                    self.tool_calls_started = self.tool_calls_started.saturating_add(1);
                }
            }
        }
        for item in &session.transcript {
            let TranscriptItem::ToolResult(result) = item else {
                continue;
            };
            let (name, arguments) = calls
                .get(&result.call_id.to_string())
                .cloned()
                .unwrap_or_else(|| ("tool".to_owned(), String::new()));
            self.tools.insert(
                result.call_id.to_string(),
                ToolState {
                    name,
                    arguments: Some(arguments),
                    status: Some(result.status),
                    content_head: Some(result.content.clone()),
                    started_at_millis: None,
                    completed_at_millis: None,
                },
            );
        }
        let mut groups: Vec<(String, Vec<TranscriptItem>)> = Vec::new();
        for item in &session.transcript {
            match item {
                TranscriptItem::User { text } => groups.push((text.clone(), Vec::new())),
                restored => {
                    if let Some((_, transcript)) = groups.last_mut() {
                        transcript.push(restored.clone());
                    }
                }
            }
        }
        for (index, (prompt, transcript)) in groups.into_iter().enumerate() {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            self.execution_turns.insert(
                number,
                ExecutionTurn {
                    number,
                    engine_turn: None,
                    run: Some(number),
                    user_inputs: vec![prompt],
                    transcript,
                },
            );
            self.restored_runs.insert(
                number,
                RestoredRun {
                    task_id: None,
                    status: None,
                    turns: 0,
                    failure_reason: None,
                    started_at_millis: None,
                    completed_at_millis: None,
                },
            );
        }
        if let Some(task) = session
            .tasks
            .iter()
            .rev()
            .find(|task| task.status != TaskStatus::Running)
        {
            self.task.task_id = Some(task.id);
            self.task.status = Some(task.status);
            self.task.started_at_millis = Some(task.started_at_millis);
            self.task.completed_at_millis = task.completed_at_millis;
            self.turn = Some(task.turns);
            self.usage = task.usage;
            self.usage_reported = task.usage.input_tokens > 0
                || task.usage.output_tokens > 0
                || task.usage.cost_usd.is_some();
        }
        self.provisional.clear();
    }

    /// Whether the reducer currently has any provisional model output.
    #[must_use]
    pub fn has_provisional_output(&self) -> bool {
        !self.provisional.is_empty()
    }

    /// Returns the event-backed parent turn for a tool call, when known.
    #[must_use]
    pub fn tool_call_turn(&self, call_id: &str) -> Option<u32> {
        self.tool_call_turns.get(call_id).copied()
    }

    fn finish_task(
        &mut self,
        event: &EventEnvelope,
        status: TaskStatus,
        timestamp_millis: u64,
        failure: Option<&TaskFailure>,
    ) {
        // A tool still waiting on approval when the task ends never sends a
        // ToolCompleted; settle it so the thread does not spin forever.
        for tool in self.tools.values_mut().filter(|tool| tool.status.is_none()) {
            tool.status = Some(ToolStatus::Cancelled {
                started: tool.started_at_millis.is_some(),
            });
            tool.completed_at_millis = Some(timestamp_millis);
        }
        self.task.task_id = Some(event.task_id);
        self.task.status = Some(status);
        self.task.completed_at_millis = Some(timestamp_millis);
        if let Some(turn) = self.turn
            && let Some(metrics) = self.turn_metrics.get_mut(&turn)
        {
            metrics.completed_at_millis = Some(timestamp_millis);
        }
        self.task.failure_reason = failure.map(|failure| failure.reason.clone());
        self.retry_wait = None;
        self.failure = failure.map(|failure| {
            let intact_turn = self.assistant_turns.iter().copied().max().unwrap_or(0);
            let elapsed_ms = self
                .task
                .started_at_millis
                .map(|started| timestamp_millis.saturating_sub(started));
            FailureState::classify(
                failure,
                &self.retries,
                self.attempt,
                &self.retry_policy,
                intact_turn,
                elapsed_ms,
            )
        });
        self.provisional.clear();
    }
}

fn event_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TaskStarted => "TaskStarted",
        EventKind::TurnStarted { .. } => "TurnStarted",
        EventKind::ModelRequestStarted { .. } => "ModelRequestStarted",
        EventKind::TextDelta { .. } => "TextDelta",
        EventKind::ReasoningDelta { .. } => "ReasoningDelta",
        EventKind::ModelRetry { .. } => "ModelRetry",
        EventKind::ModelCompleted { .. } => "ModelCompleted",
        EventKind::ContextProgress { .. } => "ContextProgress",
        EventKind::ToolStarted { .. } => "ToolStarted",
        EventKind::ToolCompleted { .. } => "ToolCompleted",
        EventKind::SteeringApplied { .. } => "SteeringApplied",
        EventKind::TaskCompleted => "TaskCompleted",
        EventKind::TaskFailed { .. } => "TaskFailed",
        EventKind::TaskCancelled => "TaskCancelled",
        EventKind::TaskPaused => "TaskPaused",
        EventKind::ContextPruned { .. } => "ContextPruned",
        EventKind::FastPathHit { .. } => "FastPathHit",
        EventKind::FastPathDeclined { .. } => "FastPathDeclined",
        EventKind::CapabilitiesResolved { .. } => "CapabilitiesResolved",
        EventKind::HarnessNotice { .. } => "HarnessNotice",
        EventKind::ContextHandoff { .. } => "ContextHandoff",
        EventKind::AutofixApplied { .. } => "AutofixApplied",
        EventKind::TodoUpdated { .. } => "TodoUpdated",
        EventKind::TurnBudgetExtended { .. } => "TurnBudgetExtended",
        EventKind::LoopNudge { .. } => "LoopNudge",
        EventKind::ApprovalRequested { .. } => "ApprovalRequested",
        EventKind::ApprovalDecided { .. } => "ApprovalDecided",
        EventKind::AwaitingInput { .. } => "AwaitingInput",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::FailureKind;
    use loom_core::kernel::event::EVENT_SCHEMA_VERSION;
    use loom_core::kernel::model::{ModelErrorKind, ModelResponse, StopReason, Usage};
    use loom_core::kernel::session::{TaskRecord, TaskStatus};
    use loom_core::kernel::transcript::{AssistantMessage, Item, ReasoningTrace};
    use loom_core::{EventEnvelope, EventKind, Session, SessionId, TaskId};

    use super::{ExecutionTurn, RECENT_EVENT_LIMIT, Reducer, RetryRecord, ToolStatus};

    fn event(sequence: u64, task_id: TaskId, kind: EventKind) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            timestamp_millis: None,
            sequence,
            task_id,
            kind,
        }
    }

    fn response(text: &str) -> ModelResponse {
        ModelResponse {
            message: AssistantMessage {
                content: text.into(),
                reasoning: Some(ReasoningTrace {
                    text: "authoritative reasoning".into(),
                    provider_data: None,
                }),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage::default(),
        }
    }

    #[test]
    fn retry_discards_deltas_and_records_attempt_history() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(0, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            10,
        );
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::TextDelta {
                    text: "partial".into(),
                },
            ),
            11,
        );
        reducer.reduce(
            &event(
                2,
                task_id,
                EventKind::ReasoningDelta {
                    text: "private partial".into(),
                },
            ),
            12,
        );
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: loom_core::kernel::model::ModelErrorKind::Transport,
                    delay_ms: 250,
                    response_started: true,
                    http_status: None,
                    policy: None,
                    idle_timeout_ms: None,
                },
            ),
            13,
        );

        assert!(!reducer.has_provisional_output());
        assert_eq!(reducer.provisional.text, "");
        assert_eq!(
            reducer.retries,
            [RetryRecord {
                attempt: 1,
                kind: "transport".into(),
                delay_ms: 250,
                response_started: true,
                http_status: None,
                turn: None,
                discarded_reasoning_tokens: 5,
                discarded_text_tokens: 2,
            }]
        );
    }

    #[test]
    fn completed_response_is_authoritative_over_streamed_deltas() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(0, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            0,
        );
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::TextDelta {
                    text: "not final".into(),
                },
            ),
            1,
        );
        reducer.reduce(
            &event(
                2,
                task_id,
                EventKind::ModelCompleted {
                    response: response("final answer"),
                },
            ),
            2,
        );

        assert!(!reducer.has_provisional_output());
        assert_eq!(reducer.committed_transcript.len(), 1);
        assert!(matches!(
            &reducer.committed_transcript[0],
            Item::Assistant(message) if message.content == "final answer"
        ));
    }

    #[test]
    fn deltas_never_touch_a_committed_turn() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("summarize the sink");
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 0);
        reducer.reduce(&event(1, task_id, EventKind::TurnStarted { turn: 1 }), 0);
        reducer.reduce(
            &event(
                2,
                task_id,
                EventKind::ModelCompleted {
                    response: response("committed"),
                },
            ),
            1,
        );
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::TextDelta {
                    text: " provisional".into(),
                },
            ),
            2,
        );
        reducer.reduce(
            &event(
                4,
                task_id,
                EventKind::ReasoningDelta {
                    text: "thinking".into(),
                },
            ),
            3,
        );

        assert_eq!(reducer.provisional.text, " provisional");
        assert_eq!(reducer.provisional.reasoning, "thinking");
        assert!(matches!(
            reducer
                .committed_transcript
                .iter()
                .find(|item| matches!(item, Item::Assistant(_))),
            Some(Item::Assistant(message)) if message.content == "committed"
        ));
        let committed = &reducer.execution_turns[&1].transcript;
        assert!(matches!(
            committed.last(),
            Some(Item::Assistant(message)) if message.content == "committed"
        ));
        assert!(
            !committed
                .iter()
                .any(|item| matches!(item, Item::Assistant(message) if message.content.contains("provisional")))
        );
    }

    #[test]
    fn tool_completion_updates_only_its_own_row() {
        use loom_core::kernel::transcript::ToolCallId;

        let task_id = TaskId::new();
        let call = |id: &str| ToolCallId::try_from(id.to_owned()).unwrap();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(
                0,
                task_id,
                EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: call("first"),
                    arguments: "{}".into(),
                },
            ),
            0,
        );
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ToolStarted {
                    name: "search".into(),
                    call_id: call("second"),
                    arguments: "{}".into(),
                },
            ),
            1,
        );
        reducer.reduce(
            &event(
                2,
                task_id,
                EventKind::ToolCompleted {
                    name: "search".into(),
                    call_id: call("second"),
                    status: ToolStatus::Success,
                    content_head: "14 matches · 9 files".into(),
                },
            ),
            2,
        );

        assert_eq!(reducer.tools["first"].status, None);
        assert_eq!(reducer.tools["second"].status, Some(ToolStatus::Success));
        assert_eq!(reducer.tools["second"].completed_at_millis, Some(2));
    }

    #[test]
    fn terminal_events_are_accepted_without_panicking() {
        let task_id = TaskId::new();
        let kinds = [
            EventKind::TaskCompleted,
            EventKind::TaskFailed {
                reason: "provider gave up".into(),
                model_kind: None,
                http_status: None,
                stop_reason: None,
            },
            EventKind::TaskCancelled,
            EventKind::TaskPaused,
        ];
        for kind in kinds {
            let mut reducer = Reducer::default();
            reducer.reduce(&event(0, task_id, kind), 1);
        }
    }

    #[test]
    fn lifecycle_uses_explicit_timestamps_and_terminal_reason() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1234);
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::TaskFailed {
                    reason: "provider failed".into(),
                    model_kind: None,
                    http_status: None,
                    stop_reason: None,
                },
            ),
            2345,
        );

        assert_eq!(reducer.task.task_id, Some(task_id));
        assert_eq!(
            reducer.task.status,
            Some(loom_core::kernel::session::TaskStatus::Failed)
        );
        assert_eq!(reducer.task.started_at_millis, Some(1234));
        assert_eq!(reducer.task.completed_at_millis, Some(2345));
        assert_eq!(
            reducer.task.failure_reason.as_deref(),
            Some("provider failed")
        );
    }

    #[test]
    fn event_history_retains_scrollable_recent_events() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        for sequence in 0..300 {
            reducer.reduce(
                &event(
                    sequence,
                    task_id,
                    EventKind::TurnStarted {
                        turn: u32::try_from(sequence).unwrap_or(u32::MAX),
                    },
                ),
                sequence,
            );
        }

        assert_eq!(reducer.event_count, 300);
        assert_eq!(reducer.recent_events.len(), RECENT_EVENT_LIMIT);
        assert_eq!(reducer.recent_events.first().unwrap(), "#299 TurnStarted");
        assert_eq!(reducer.recent_events.last().unwrap(), "#44 TurnStarted");
    }

    #[test]
    fn long_running_multi_turn_task_projects_turns_tools_and_steering() {
        use loom_core::kernel::transcript::ToolCallId;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("refactor the parser");
        reducer.push_display_prompt("also keep the tests green");

        let call = |n: u32| ToolCallId::try_from(format!("call-{n}")).unwrap();
        let mut sequence = 0;
        let mut next = |kind: EventKind| {
            let envelope = event(sequence, task_id, kind);
            sequence += 1;
            envelope
        };

        reducer.reduce(&next(EventKind::TaskStarted), 1);
        // Turn 1: model asks for a tool, the tool runs, then the model answers.
        reducer.reduce(&next(EventKind::TurnStarted { turn: 1 }), 10);
        reducer.reduce(
            &next(EventKind::ToolStarted {
                name: "read_file".into(),
                call_id: call(1),
                arguments: "{}".into(),
            }),
            11,
        );
        reducer.reduce(
            &next(EventKind::ToolCompleted {
                name: "read_file".into(),
                call_id: call(1),
                status: ToolStatus::Success,
                content_head: "fn parse()".into(),
            }),
            12,
        );
        reducer.reduce(
            &next(EventKind::ModelCompleted {
                response: response("read it"),
            }),
            13,
        );
        // Turn 2: the user's follow-up is applied at the boundary and steers it.
        reducer.reduce(&next(EventKind::TurnStarted { turn: 2 }), 20);
        reducer.reduce(
            &next(EventKind::SteeringApplied {
                turn: 2,
                messages: 1,
            }),
            21,
        );
        reducer.reduce(
            &next(EventKind::ToolStarted {
                name: "run_command".into(),
                call_id: call(2),
                arguments: "{}".into(),
            }),
            22,
        );
        reducer.reduce(
            &next(EventKind::ToolCompleted {
                name: "run_command".into(),
                call_id: call(2),
                status: ToolStatus::Success,
                content_head: "exit 0".into(),
            }),
            23,
        );
        reducer.reduce(
            &next(EventKind::ModelCompleted {
                response: response("tests pass"),
            }),
            24,
        );
        // Turn 3: the run finishes after a third model response.
        reducer.reduce(&next(EventKind::TurnStarted { turn: 3 }), 30);
        reducer.reduce(
            &next(EventKind::ModelCompleted {
                response: response("done"),
            }),
            31,
        );
        reducer.reduce(&next(EventKind::TaskCompleted), 40);

        assert_eq!(reducer.turn, Some(3));
        assert_eq!(reducer.tool_calls_started, 2);
        assert_eq!(reducer.event_count, sequence);
        assert_eq!(reducer.sequence, sequence - 1);
        assert_eq!(reducer.task.status, Some(TaskStatus::Completed));
        assert_eq!(
            reducer.original_instruction.as_deref(),
            Some("refactor the parser")
        );
        let turns: Vec<u32> = reducer.execution_turns.keys().copied().collect();
        assert_eq!(turns, vec![1, 2, 3]);
        assert_eq!(reducer.tools.len(), 2);
    }

    #[test]
    fn follow_up_input_is_associated_with_the_steered_turn() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("opening instruction");
        reducer.push_display_prompt("follow up");
        reducer.reduce(&event(0, task_id, EventKind::TurnStarted { turn: 1 }), 10);
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ModelCompleted {
                    response: response("first"),
                },
            ),
            20,
        );
        reducer.reduce(&event(2, task_id, EventKind::TurnStarted { turn: 2 }), 30);
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::SteeringApplied {
                    turn: 2,
                    messages: 1,
                },
            ),
            31,
        );
        reducer.reduce(
            &event(
                4,
                task_id,
                EventKind::ModelCompleted {
                    response: response("second"),
                },
            ),
            40,
        );

        assert_eq!(
            reducer.original_instruction.as_deref(),
            Some("opening instruction")
        );
        assert_eq!(
            reducer.execution_turns[&1].user_inputs,
            Vec::<String>::new()
        );
        assert_eq!(reducer.execution_turns[&1].transcript.len(), 1);
        assert_eq!(reducer.execution_turns[&2].user_inputs, ["follow up"]);
        assert_eq!(reducer.execution_turns[&2].transcript.len(), 1);
        assert!(!reducer.turn_metrics[&1].usage_reported);
    }

    #[test]
    fn context_progress_keeps_unbounded_distinct_from_exhausted() {
        let task_id = TaskId::new();
        let census = serde_json::from_value(serde_json::json!({
            "system_prompt": 1,
            "repo_facts": 2,
            "user_items": 3,
            "assistant_history": 4,
            "reasoning_replay": 5,
            "tool_results": 6,
            "tool_specs": 7,
            "total": 28
        }))
        .unwrap();
        let remaining_budget = serde_json::from_value(serde_json::json!({
            "input_tokens": 0,
            "output_tokens": null,
            "total_tokens": 9,
            "tool_calls": null,
            "wall_time": null
        }))
        .unwrap();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ContextProgress {
                    turn: 2,
                    context_budget: loom_core::kernel::context::ContextBudget {
                        max_input_tokens: 30,
                        reserved_output_tokens: 10,
                    },
                    census,
                    remaining_budget,
                },
            ),
            100,
        );

        let progress = reducer.context_progress.expect("context event projected");
        assert_eq!(progress.turn, 2);
        assert_eq!(progress.census.total, 28);
        assert_eq!(progress.remaining_budget.input_tokens, Some(0));
        assert_eq!(progress.remaining_budget.output_tokens, None);
        assert_eq!(progress.remaining_budget.total_tokens, Some(9));
        assert_eq!(progress.elapsed_at_compile, None);
        assert_eq!(progress.tool_calls_used, 0);
    }

    #[test]
    fn tool_status_and_follow_up_queue_are_reduced() {
        let task_id = TaskId::new();
        let call_id =
            loom_core::kernel::transcript::ToolCallId::try_from("call_1".to_owned()).unwrap();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(
                0,
                task_id,
                EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: call_id.clone(),
                    arguments: "{}".into(),
                },
            ),
            0,
        );
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ToolCompleted {
                    name: "read_file".into(),
                    call_id,
                    status: ToolStatus::Success,
                    content_head: "ok".into(),
                },
            ),
            1,
        );
        reducer.set_follow_up_queued(true);
        reducer.reduce(
            &event(
                2,
                task_id,
                EventKind::SteeringApplied {
                    turn: 1,
                    messages: 1,
                },
            ),
            2,
        );

        let tool = &reducer.tools["call_1"];
        assert_eq!(tool.status, Some(ToolStatus::Success));
        assert_eq!(tool.content_head.as_deref(), Some("ok"));
        assert!(!reducer.follow_up_queued);
    }

    #[test]
    fn reconcile_session_replaces_event_projection_with_exact_history() {
        let mut session = Session::new(SessionId::new());
        session
            .push_item(Item::User {
                text: "hello".into(),
            })
            .unwrap();
        let mut reducer = Reducer::default();
        reducer.provisional.text = "pending".into();
        reducer.reconcile_session(&session);

        assert_eq!(reducer.committed_transcript, session.transcript);
        assert!(!reducer.has_provisional_output());
    }

    #[test]
    fn restore_session_rebuilds_readable_history_and_last_task() {
        let assistant = |content: &str| AssistantMessage {
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        };
        let mut session = Session::new(SessionId::new());
        session
            .push_item(Item::User {
                text: "old prompt".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(assistant("old answer")))
            .unwrap();
        let task_id = TaskId::new();
        session.tasks.push(TaskRecord {
            id: task_id,
            status: TaskStatus::Completed,
            turns: 3,
            usage: Usage::default(),
            started_at_millis: 10,
            completed_at_millis: Some(20),
            effort: Some("high".into()),
            end_reason: Some("end_turn".into()),
        });

        let mut reducer = Reducer::default();
        reducer.restore_session(&session);

        assert!(reducer.restored);
        assert_eq!(reducer.task.task_id, Some(task_id));
        assert_eq!(reducer.task.status, Some(TaskStatus::Completed));
        assert_eq!(reducer.turn, Some(3));
        let turn = &reducer.execution_turns[&1];
        assert_eq!(turn.run, Some(1));
        assert_eq!(turn.engine_turn, None, "no events means no invented turns");
        assert_eq!(turn.user_inputs, ["old prompt"]);
        assert!(matches!(
            &turn.transcript[0],
            Item::Assistant(message) if message.content == "old answer"
        ));
    }

    #[test]
    fn restore_replays_turns_per_run_without_synthetic_numbers() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut session = Session::new(SessionId::new());
        session
            .push_item(Item::User {
                text: "first task".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(response("first answer").message))
            .unwrap();
        session
            .push_item(Item::Assistant(response("first follow-up").message))
            .unwrap();
        session
            .push_item(Item::User {
                text: "second task".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(response("second answer").message))
            .unwrap();
        for (id, turns, started) in [(first, 2, 10), (second, 1, 30)] {
            session.tasks.push(TaskRecord {
                id,
                status: TaskStatus::Completed,
                turns,
                usage: Usage::default(),
                started_at_millis: started,
                completed_at_millis: Some(started + 10),
                effort: Some("high".into()),
                end_reason: Some("end_turn".into()),
            });
        }
        let mut events = vec![
            event(0, first, EventKind::TaskStarted),
            event(1, first, EventKind::TurnStarted { turn: 1 }),
            event(
                2,
                first,
                EventKind::ModelCompleted {
                    response: response("first answer"),
                },
            ),
            event(3, first, EventKind::TurnStarted { turn: 2 }),
            event(
                4,
                first,
                EventKind::ModelCompleted {
                    response: response("first follow-up"),
                },
            ),
            event(5, first, EventKind::TaskCompleted),
            event(0, second, EventKind::TaskStarted),
            event(1, second, EventKind::TurnStarted { turn: 1 }),
            event(
                2,
                second,
                EventKind::ModelCompleted {
                    response: response("second answer"),
                },
            ),
            event(3, second, EventKind::TaskCompleted),
        ];
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.timestamp_millis =
                Some(1000 + u64::try_from(index).unwrap_or(0).saturating_mul(10));
        }

        let mut reducer = Reducer::default();
        reducer.restore_session_with_events(&session, &events);

        assert_eq!(reducer.execution_turns.len(), 3);
        let first_turn = &reducer.execution_turns[&1];
        assert_eq!(first_turn.run, Some(1));
        assert_eq!(first_turn.engine_turn, Some(1));
        assert_eq!(first_turn.user_inputs, ["first task"]);
        assert_eq!(reducer.execution_turns[&2].engine_turn, Some(2));
        let second_run = &reducer.execution_turns[&3];
        assert_eq!(second_run.run, Some(2));
        assert_eq!(second_run.engine_turn, Some(1));
        assert_eq!(second_run.user_inputs, ["second task"]);
        assert_eq!(reducer.restored_runs[&1].turns, 2);
        assert_eq!(reducer.restored_runs[&2].turns, 1);
        assert_eq!(reducer.task.task_id, Some(second));
        assert_eq!(reducer.turn_metrics[&1].started_at_millis, Some(1010));
        assert_eq!(reducer.turn_metrics[&1].completed_at_millis, Some(1030));

        for envelope in &mut events {
            envelope.timestamp_millis = None;
        }
        let mut legacy = Reducer::default();
        legacy.restore_session_with_events(&session, &events);
        assert_eq!(legacy.turn_metrics[&1].started_at_millis, None);
        assert_eq!(legacy.turn_metrics[&1].completed_at_millis, None);
    }

    #[test]
    fn restore_uses_envelope_timestamps_for_tool_durations() {
        use loom_core::kernel::transcript::ToolCallId;

        let task_id = TaskId::new();
        let call_id = ToolCallId::try_from("call-timed".to_owned()).unwrap();
        let session = Session::new(SessionId::new());
        let mut events = vec![
            event(0, task_id, EventKind::TaskStarted),
            event(1, task_id, EventKind::TurnStarted { turn: 1 }),
            event(
                2,
                task_id,
                EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: call_id.clone(),
                    arguments: "{}".into(),
                },
            ),
            event(
                3,
                task_id,
                EventKind::ToolCompleted {
                    name: "read_file".into(),
                    call_id,
                    status: ToolStatus::Success,
                    content_head: "done".into(),
                },
            ),
            event(
                4,
                task_id,
                EventKind::ModelCompleted {
                    response: response("done"),
                },
            ),
            event(5, task_id, EventKind::TaskCompleted),
        ];
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.timestamp_millis =
                Some(1000 + u64::try_from(index).unwrap_or(0).saturating_mul(10));
        }

        let mut reducer = Reducer::default();
        reducer.restore_session_with_events(&session, &events);

        let tool = &reducer.tools["call-timed"];
        assert_eq!(tool.started_at_millis, Some(1020));
        assert_eq!(tool.completed_at_millis, Some(1030));
    }

    #[test]
    fn restore_display_prompt_hides_expanded_attachment_body() {
        let mut reducer = Reducer::default();
        reducer.begin_task("Please read @README.md");
        reducer.committed_transcript[0] = Item::User {
            text: "Please read @README.md\n\n<attached file: README.md>\nlarge file body".into(),
        };

        reducer.restore_display_prompt();

        assert!(matches!(
            &reducer.committed_transcript[0],
            Item::User { text } if text == "Please read @README.md"
        ));
    }

    #[test]
    fn restore_display_prompts_keeps_multiple_attachment_prompts_compact() {
        let mut reducer = Reducer::default();
        reducer.begin_task("Read @README.md");
        reducer.push_display_prompt("Now inspect @src/lib.rs");
        reducer.reduce(
            &event(0, TaskId::new(), EventKind::TurnStarted { turn: 1 }),
            1,
        );
        reducer.reduce(
            &event(
                1,
                TaskId::new(),
                EventKind::SteeringApplied {
                    turn: 1,
                    messages: 1,
                },
            ),
            2,
        );
        reducer.committed_transcript = vec![
            Item::User {
                text: "Read @README.md\n<attached file: README.md> huge contents".into(),
            },
            Item::User {
                text: "Now inspect @src/lib.rs\n<attached file: src/lib.rs> huge contents".into(),
            },
        ];

        reducer.restore_display_prompt();

        assert!(matches!(
            &reducer.committed_transcript[0],
            Item::User { text } if text == "Read @README.md"
        ));
        assert!(matches!(
            &reducer.committed_transcript[1],
            Item::User { text } if text.contains("huge contents")
        ));
        assert_eq!(
            reducer.execution_turns[&1].user_inputs,
            ["Now inspect @src/lib.rs"]
        );
    }

    #[test]
    fn task_end_settles_a_tool_waiting_for_approval() {
        use loom_core::kernel::transcript::ToolCallId;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("write a file");
        reducer.reduce(&event(0, task_id, EventKind::TurnStarted { turn: 1 }), 1);
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ToolStarted {
                    name: "write_file".into(),
                    call_id: ToolCallId::try_from("call-wait".to_string()).unwrap(),
                    arguments: "{}".into(),
                },
            ),
            2,
        );
        assert_eq!(reducer.tools["call-wait"].status, None);

        reducer.reduce(&event(2, task_id, EventKind::TaskCancelled), 3);

        assert!(matches!(
            reducer.tools["call-wait"].status,
            Some(ToolStatus::Cancelled { started: true })
        ));
    }

    #[test]
    #[allow(clippy::wildcard_enum_match_arm)] // Item is non_exhaustive
    fn reconcile_keeps_only_the_current_tasks_assistants() {
        let assistant = |content: &str| AssistantMessage {
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        };
        let mut session = Session::new(SessionId::new());
        session
            .push_item(Item::User {
                text: "first".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(assistant("first answer")))
            .unwrap();
        session
            .push_item(Item::User {
                text: "second".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(assistant("second answer")))
            .unwrap();

        let mut reducer = Reducer::default();
        reducer.begin_task("second");
        reducer.execution_turns.insert(
            1,
            ExecutionTurn {
                number: 1,
                transcript: vec![Item::Assistant(assistant("second answer"))],
                ..ExecutionTurn::default()
            },
        );
        reducer.reconcile_session(&session);

        let shown = reducer.execution_turns[&1]
            .transcript
            .iter()
            .filter_map(|item| match item {
                Item::Assistant(message) => Some(message.content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(shown, ["second answer"]);
    }

    #[test]
    fn countdown_depends_only_on_injected_time() {
        use loom_core::kernel::model::ModelErrorKind;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1_000);
        reducer.reduce(
            &event(1, task_id, EventKind::TurnStarted { turn: 1 }),
            1_000,
        );
        reducer.reduce(
            &event(2, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            1_100,
        );
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: ModelErrorKind::Transport,
                    delay_ms: 5_000,
                    response_started: false,
                    http_status: None,
                    policy: None,
                    idle_timeout_ms: None,
                },
            ),
            2_000,
        );

        assert_eq!(
            reducer.retry_wait.as_ref().unwrap().remaining_millis(),
            5_000
        );
        reducer.tick(4_500);
        assert_eq!(
            reducer.retry_wait.as_ref().unwrap().remaining_millis(),
            2_500
        );
        reducer.tick(1_000);
        assert_eq!(
            reducer.retry_wait.as_ref().unwrap().remaining_millis(),
            5_000
        );
        reducer.tick(8_000);
        assert_eq!(reducer.retry_wait.as_ref().unwrap().remaining_millis(), 0);
        assert_eq!(reducer.retry_wait.as_ref().unwrap().progress(), 1.0);
    }

    #[test]
    fn failed_attempt_output_is_never_committed() {
        use loom_core::kernel::model::ModelErrorKind;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect");
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        reducer.reduce(&event(1, task_id, EventKind::TurnStarted { turn: 1 }), 2);
        reducer.reduce(
            &event(2, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            3,
        );
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::TextDelta {
                    text: "not committed".into(),
                },
            ),
            4,
        );
        reducer.reduce(
            &event(
                4,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: ModelErrorKind::Server,
                    delay_ms: 100,
                    response_started: true,
                    http_status: None,
                    policy: None,
                    idle_timeout_ms: None,
                },
            ),
            5,
        );

        assert!(!reducer.committed_transcript.iter().any(|item| matches!(
            item,
            Item::Assistant(message) if message.content.contains("not committed")
        )));
        assert_eq!(reducer.retries[0].discarded_text_tokens, 4);
    }

    #[test]
    fn cancelled_request_clears_the_backoff_and_is_never_retried() {
        use loom_core::kernel::model::ModelErrorKind;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        reducer.reduce(&event(1, task_id, EventKind::TurnStarted { turn: 1 }), 2);
        reducer.reduce(
            &event(2, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            3,
        );
        reducer.reduce(
            &event(
                3,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: ModelErrorKind::Transport,
                    delay_ms: 5_000,
                    response_started: false,
                    http_status: None,
                    policy: None,
                    idle_timeout_ms: None,
                },
            ),
            4,
        );
        assert!(reducer.is_retrying());

        reducer.reduce(&event(4, task_id, EventKind::TaskCancelled), 5);

        assert!(!reducer.is_retrying());
        assert_eq!(reducer.task.status, Some(TaskStatus::Cancelled));
        assert_eq!(reducer.retries.len(), 1);
    }

    #[test]
    fn unknown_failure_reason_renders_an_other_card_without_panicking() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::TaskFailed {
                    reason: "something new happened".into(),
                    model_kind: None,
                    http_status: None,
                    stop_reason: None,
                },
            ),
            2,
        );

        let failure = reducer.failure.expect("failure card");
        assert_eq!(failure.kind, FailureKind::Other);
        assert_eq!(failure.body, "something new happened");
    }

    #[test]
    fn attempt_numbers_are_clamped_to_the_policy_maximum() {
        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(
            &event(0, task_id, EventKind::ModelRequestStarted { attempt: 99 }),
            1,
        );
        assert_eq!(reducer.attempts_used(), reducer.retry_policy.max_attempts);
        assert_eq!(
            reducer.provisional.attempt,
            Some(reducer.retry_policy.max_attempts)
        );
    }

    #[test]
    fn retry_event_policy_replaces_the_mirrored_defaults() {
        use loom_core::kernel::event::RetryPolicySnapshot;

        let task_id = TaskId::new();
        let mut reducer = Reducer::default();
        reducer.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        reducer.reduce(
            &event(
                1,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: ModelErrorKind::Overloaded,
                    delay_ms: 4_000,
                    response_started: false,
                    http_status: Some(503),
                    policy: Some(RetryPolicySnapshot {
                        max_attempts: 7,
                        initial_backoff_ms: 500,
                        backoff_factor: 3,
                        max_backoff_ms: 20_000,
                        jitter_percent: 10,
                    }),
                    idle_timeout_ms: Some(300_000),
                },
            ),
            2,
        );

        assert_eq!(reducer.retry_policy.max_attempts, 7);
        assert_eq!(reducer.retry_policy.initial_backoff_ms, 500);
        assert_eq!(reducer.retry_policy.backoff_factor, 3);
        assert_eq!(reducer.retry_policy.max_backoff_ms, 20_000);
        assert_eq!(reducer.retry_policy.jitter_percent, 10);
        assert_eq!(reducer.retry_policy.idle_timeout_ms, Some(300_000));
        assert_eq!(reducer.retries[0].http_status, Some(503));
        assert_eq!(reducer.retry_wait.as_ref().unwrap().next_attempt, 2);
    }

    #[test]
    fn failure_cards_classify_the_four_reference_shapes() {
        use loom_core::kernel::model::ModelErrorKind;

        let task_id = TaskId::new();
        let mut quota = Reducer::default();
        quota.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        quota.reduce(
            &event(
                1,
                task_id,
                EventKind::TaskFailed {
                    reason: "model request failed: [quota_exhausted] HTTP 402".into(),
                    model_kind: Some(ModelErrorKind::QuotaExhausted),
                    http_status: Some(402),
                    stop_reason: None,
                },
            ),
            2,
        );
        let card = quota.failure.unwrap();
        assert_eq!(card.kind, FailureKind::QuotaExhausted);
        assert_eq!(card.http_status, Some(402));

        let mut max_tokens = Reducer::default();
        max_tokens.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        max_tokens.reduce(
            &event(
                1,
                task_id,
                EventKind::TaskFailed {
                    reason: "model stopped with `max_tokens`".into(),
                    model_kind: None,
                    http_status: None,
                    stop_reason: Some("max_tokens".into()),
                },
            ),
            2,
        );
        assert_eq!(max_tokens.failure.unwrap().kind, FailureKind::MaxTokens);

        let mut filtered = Reducer::default();
        filtered.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        filtered.reduce(
            &event(
                1,
                task_id,
                EventKind::TaskFailed {
                    reason: "model stopped with `content_filter`".into(),
                    model_kind: None,
                    http_status: None,
                    stop_reason: Some("content_filter".into()),
                },
            ),
            2,
        );
        assert_eq!(filtered.failure.unwrap().kind, FailureKind::ContentFilter);

        let mut exhausted = Reducer::default();
        exhausted.reduce(&event(0, task_id, EventKind::TaskStarted), 1);
        exhausted.reduce(&event(1, task_id, EventKind::TurnStarted { turn: 1 }), 2);
        exhausted.reduce(
            &event(2, task_id, EventKind::ModelRequestStarted { attempt: 1 }),
            3,
        );
        exhausted.reduce(
            &event(
                3,
                task_id,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: ModelErrorKind::Overloaded,
                    delay_ms: 10,
                    response_started: false,
                    http_status: None,
                    policy: None,
                    idle_timeout_ms: None,
                },
            ),
            4,
        );
        exhausted.reduce(
            &event(
                4,
                task_id,
                EventKind::TaskFailed {
                    reason: "model request failed: [overloaded] HTTP 503".into(),
                    model_kind: Some(ModelErrorKind::Overloaded),
                    http_status: Some(503),
                    stop_reason: None,
                },
            ),
            5,
        );
        let card = exhausted.failure.unwrap();
        assert_eq!(card.kind, FailureKind::RetriesExhausted);
        assert_eq!(card.subtitle.as_deref(), Some("overloaded ×4"));
    }
}
