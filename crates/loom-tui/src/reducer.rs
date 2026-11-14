//! Pure projection of Loom's event stream into runtime-facing TUI state.
//!
//! Event data is best-effort; the session transcript remains authoritative.
//! In particular, streamed model deltas live only in [`ProvisionalResponse`]
//! until a `ModelCompleted` event commits the complete assistant message.

use std::collections::BTreeMap;

const RECENT_EVENT_LIMIT: usize = 256;

use loom_core::{EventEnvelope, EventKind};

use loom_core::kernel::agent::RemainingBudget;
use loom_core::kernel::context::{ContextBudget, ContextCensus};
use loom_core::kernel::model::Usage;
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
    /// 1-based turn ordinal emitted by the engine.
    pub number: u32,
    /// User-authored follow-ups applied at this turn boundary.
    pub user_inputs: Vec<String>,
    /// Assistant messages and reconciled tool results belonging to this turn.
    pub transcript: Vec<TranscriptItem>,
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
    /// Turn hierarchy projected from engine turn and steering events.
    pub execution_turns: BTreeMap<u32, ExecutionTurn>,
    /// Original task instruction, displayed once above execution turns.
    pub original_instruction: Option<String>,
    /// Uncommitted stream fragments for the current attempt.
    pub provisional: ProvisionalResponse,
    /// Retry history in event order.
    pub retries: Vec<RetryRecord>,
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
    /// Most recent event labels, newest first.
    pub recent_events: Vec<String>,
    /// Latest compiled-request and remaining-budget snapshot, if observed.
    pub context_progress: Option<ContextProgress>,
    /// Final successful engine summary, if the run completed successfully.
    pub summary: Option<loom_core::TaskSummary>,
    /// Per-turn measurements from the event stream.
    pub turn_metrics: BTreeMap<u32, TurnMetrics>,
    /// Turn ownership of authoritative assistant items, in transcript order.
    assistant_turns: Vec<u32>,
    /// Parent turn for each observed call ID.
    tool_call_turns: BTreeMap<String, u32>,
    /// Next visible queued prompt consumed by a `SteeringApplied` event.
    applied_display_prompts: usize,
    /// UI expansion toggles for optional transcript details.
    pub expanded: ExpandedDetails,
}

/// Independently controlled optional details in the execution timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpandedDetails {
    /// Show explicitly provider-emitted reasoning traces.
    pub reasoning: bool,
    /// Show complete tool arguments and available output previews.
    pub tools: bool,
}

impl Reducer {
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
    /// `timestamp_millis` is used only for task lifecycle timestamps; all
    /// other state comes solely from the event and current reducer state.
    #[allow(clippy::too_many_lines)] // one exhaustive event-to-state transition table
    pub fn reduce(&mut self, event: &EventEnvelope, timestamp_millis: u64) {
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
            }
            EventKind::TaskStarted => {
                self.task.task_id = Some(event.task_id);
                self.task.status = Some(TaskStatus::Running);
                self.task.started_at_millis = Some(timestamp_millis);
                self.task.completed_at_millis = None;
                self.task.failure_reason = None;
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
                        ..ExecutionTurn::default()
                    });
                self.turn_metrics
                    .entry(*turn)
                    .or_default()
                    .started_at_millis = Some(timestamp_millis);
            }
            EventKind::ModelRequestStarted { attempt } => {
                self.attempt = Some(*attempt);
                self.provisional.clear();
                self.provisional.attempt = Some(*attempt);
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
            } => {
                self.retries.push(RetryRecord {
                    attempt: *attempt,
                    kind: kind.to_string(),
                    delay_ms: *delay_ms,
                    response_started: *response_started,
                    turn: self.turn,
                });
                self.provisional.clear();
                self.attempt = Some(*attempt);
            }
            EventKind::ModelCompleted { response } => {
                // Deltas are intentionally not used here: the complete
                // response is the sole authority for committed output.
                self.provisional.clear();
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
            EventKind::TaskFailed { reason } => {
                self.finish_task(
                    event,
                    TaskStatus::Failed,
                    timestamp_millis,
                    Some(reason.clone()),
                );
            }
            EventKind::TaskCancelled => {
                self.finish_task(event, TaskStatus::Cancelled, timestamp_millis, None);
            }
            EventKind::TaskPaused => {
                self.finish_task(event, TaskStatus::Paused, timestamp_millis, None);
            }
            EventKind::ContextPruned { .. }
            | EventKind::FastPathHit { .. }
            | EventKind::FastPathDeclined { .. }
            | EventKind::HarnessNotice { .. }
            | EventKind::ContextHandoff { .. }
            | EventKind::AutofixApplied { .. }
            | EventKind::TodoUpdated { .. }
            | EventKind::TurnBudgetExtended { .. }
            | EventKind::LoopNudge { .. }
            | EventKind::AwaitingInput { .. }
            | _ => {}
        }
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
        for turn in self.execution_turns.values_mut() {
            turn.transcript.clear();
        }
        let mut unmatched_assistants = self
            .committed_transcript
            .iter()
            .filter_map(|item| {
                if let TranscriptItem::Assistant(message) = item {
                    Some(message.clone())
                } else {
                    None
                }
            })
            .zip(self.assistant_turns.iter().copied())
            .collect::<Vec<_>>();
        for item in &session.transcript {
            match item {
                TranscriptItem::Assistant(message) => {
                    if let Some(position) = unmatched_assistants
                        .iter()
                        .position(|(candidate, _)| candidate == message)
                    {
                        let (_, turn) = unmatched_assistants.remove(position);
                        if let Some(execution_turn) = self.execution_turns.get_mut(&turn) {
                            execution_turn.transcript.push(item.clone());
                        }
                    }
                }
                TranscriptItem::ToolResult(result) => {
                    if let Some(turn) = self.tool_call_turns.get(&result.call_id.to_string())
                        && let Some(execution_turn) = self.execution_turns.get_mut(turn)
                    {
                        execution_turn.transcript.push(item.clone());
                    }
                }
                _ => {}
            }
        }
        self.provisional.clear();
        self.restore_display_prompt();
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
        failure_reason: Option<String>,
    ) {
        self.task.task_id = Some(event.task_id);
        self.task.status = Some(status);
        self.task.completed_at_millis = Some(timestamp_millis);
        if let Some(turn) = self.turn
            && let Some(metrics) = self.turn_metrics.get_mut(&turn)
        {
            metrics.completed_at_millis = Some(timestamp_millis);
        }
        self.task.failure_reason = failure_reason;
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
        EventKind::AwaitingInput { .. } => "AwaitingInput",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use loom_core::kernel::event::EVENT_SCHEMA_VERSION;
    use loom_core::kernel::model::{ModelResponse, StopReason, Usage};
    use loom_core::kernel::session::TaskStatus;
    use loom_core::kernel::transcript::{AssistantMessage, Item, ReasoningTrace};
    use loom_core::{EventEnvelope, EventKind, Session, SessionId, TaskId};

    use super::{RECENT_EVENT_LIMIT, Reducer, RetryRecord, ToolStatus};

    fn event(sequence: u64, task_id: TaskId, kind: EventKind) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
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
                turn: None,
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
}
