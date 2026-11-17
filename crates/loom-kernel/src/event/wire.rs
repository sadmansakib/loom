//! The event wire model: the protocol version, serialization budgets,
//! the envelope, and the event kinds that cross the wire.

use crate::agent::RemainingBudget;
use crate::context::{ContextBudget, ContextCensus, PruneReport};
use crate::id::{SessionId, TaskId};
use crate::model::{ModelErrorKind, ModelResponse};
use crate::session::TodoItem;
use crate::transcript::{ToolCallId, ToolStatus};
use serde::{Deserialize, Serialize};
/// Current event protocol version.
///
/// Bumped on any wire-visible change — including adding an
/// [`EventKind`] variant: there is deliberately no `Unknown` catch-all
/// variant, so an older consumer meeting a newer tag fails to parse the
/// whole envelope (and never reaches `schema_version`). The event stream
/// is forward-only, exactly like stored transcripts.
///
/// The wire format also **nests types owned by other modules**, so
/// changing any of these is an event wire change and bumps this version:
/// via `ModelCompleted` — [`ModelResponse`] with
/// [`Usage`](crate::model::Usage),
/// [`AssistantMessage`](crate::transcript::AssistantMessage) (which nests
/// [`ToolCall`](crate::transcript::ToolCall)),
/// [`ReasoningTrace`](crate::transcript::ReasoningTrace) (which nests
/// [`ProviderData`](crate::transcript::ProviderData)), and
/// [`StopReason`](crate::model::StopReason); via `ModelRetry` —
/// [`ModelErrorKind`]; plus [`ToolCallId`] and [`TaskId`].
///
/// v2 adds `Usage.cost_usd` (provider-reported charge; `serde(default)`,
/// so v1 payloads still deserialize).
/// v3 adds `ContextPruned` (prune attribution: trigger arm plus
/// per-call mechanism and estimated reclaim).
/// v4 adds `AwaitingInput` (engine idle marker after a task ends);
/// v5 adds `TaskPaused` (graceful between-turns stop).
/// v6 adds `ContextHandoff` (LLM handoff compaction marker: turn and
/// summary length).
/// v7 adds `FastPathHit` (deterministic solve with zero model calls).
/// v8 adds the surfaced-decision events: `FastPathDeclined` (the
/// attempted fall-through side of `FastPathHit`),
/// `CapabilitiesResolved` (which authority — catalog, provider table,
/// or recording — fixed the model's capabilities), and
/// `HarnessNotice` (per-command deviations such as an unconfined
/// sandbox or a truncated stream, carried out of the tools layer).
/// v9 adds `TodoUpdated` (roadmap item 8 phase B: the kernel-owned todo
/// list changed during a turn's tool batch; replaying the events
/// reconstructs the list, and each event marks one tail re-append).
/// v10 adds `SteeringApplied` (item 25: queued user messages joined
/// v10 adds `SteeringApplied` (item 25: queued user messages joined
/// the transcript at a turn boundary).
/// v11 adds `LoopNudge` plus the serialized-argument and content head
/// budgets for the tool events.
/// v12 adds `TurnBudgetExtended` (item 26: a staged turn budget was
/// extended at a turn boundary on deterministic progress evidence).
/// v13 adds the `WritingPhase` stub reason (item 10: phase-aware
/// retention masked exploration reads during the writing phase).
/// v14 adds `AutofixApplied` (item 33: the self-optimization loop's
/// autofix mechanism touched a file set during a task).
/// v15 widens `FastPathDeclined`'s rename reason additively (item 34
/// precondition): the coarse `intent-rename-unsolved` splits into
/// `intent-rename-collision`, `intent-rename-zero-occurrences`,
/// `intent-rename-too-many-occurrences`, `intent-rename-verify-failed`,
/// and `intent-rename-write-failed`. The field stays a free string, so
/// older payloads and readers are unaffected.
/// v16 adds `ContextProgress`: the compiler's per-request context budget and
/// census plus the agent's remaining task-budget headroom at compile time.
/// v17 adds `StopReason::PauseTurn` so provider-paused responses are distinct
/// from terminal `EndTurn` responses in persisted `ModelCompleted` events.
/// v18 adds `ApprovalRequested` and `ApprovalDecided`: the operator gate for
/// file writes, so a pending write and the answer to it are on the record.
/// v19 makes `ApprovalDecided.allowed` exact: a refused write reports
/// `false` from the operator's decision, not inferred from the tool status.
/// v20 adds `PruneTrigger::Operator`: an operator drop (`/context` `x`)
/// stubbed a result below the window trigger.
/// v21 adds [`EventEnvelope::timestamp_millis`]: observation time on the
/// envelope so persisted history can restore real durations.
/// v22 adds structured failure metadata: [`ModelError`](crate::model::ModelError)'s
/// HTTP status reaches `ModelRetry`, `ModelRetry` carries the effective
/// [`RetryPolicySnapshot`], and `TaskFailed` names the model kind, HTTP
/// status, and stop reason instead of leaving consumers to parse `reason`.
/// v23 adds `ModelRetry.idle_timeout_ms`: the provider's own stream
/// liveness window, so a frontend labels the real number. It is not part
/// of [`RetryPolicySnapshot`] — the policy decides *whether* to retry,
/// this decides when a silent stream counts as a failure.
pub const EVENT_SCHEMA_VERSION: u16 = 23;

/// Current Unix epoch milliseconds, or `0` before the epoch.
#[must_use]
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The evidence that justified a [`EventKind::TurnBudgetExtended`] grant
/// (item 26): state the loop already tracks, never a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnBudgetReason {
    /// The turn that exhausted the grant changed a file — active work.
    MutationAtLimit,
    /// Mutations were still unverified against the full suite — the run
    /// was truncated inside the completion gate's verify loop.
    UnverifiedWork,
}

/// Byte budget for a `ToolStarted` event's serialized arguments (schema
/// 11): enough for the typical command or edit summary, never a whole
/// file body.
pub const ARGUMENTS_EVENT_HEAD: usize = 2_000;

/// Byte budget for a `ToolCompleted` event's content head (schema 11):
/// enough for the echoed command line and its exit status.
pub const CONTENT_EVENT_HEAD: usize = 1_000;

/// Head-bounds a string to at most `budget` bytes on a UTF-8 char
/// boundary, without any truncation marker. Returns the input
/// unchanged when it already fits.
pub(crate) fn head_bytes(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Head-bounds a string to `budget` bytes on a char boundary, marking
/// the cut. Used by the tool events so the stream stays forensic
/// without reproducing whole file bodies or tool outputs.
#[must_use]
pub fn text_head(text: &str, budget: usize) -> String {
    let head = head_bytes(text, budget);
    if head.len() < text.len() {
        format!("{head}[truncated]")
    } else {
        head.to_owned()
    }
}

/// One observed occurrence during a task, wrapped for the wire.
///
/// Every event belongs to a task. `sequence` numbers are unique and
/// strictly increasing per sink, starting at 0 (child runs sharing a
/// sink share one numbering), and the sink guarantees arrival order
/// equals sequence order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Wire protocol version of this event ([`EVENT_SCHEMA_VERSION`]).
    pub schema_version: u16,
    /// Unique, strictly increasing per-sink position, starting at 0.
    ///
    /// Ordering key for consumers (and child runs sharing the sink);
    /// arrival order matches it by construction. In principle a missing
    /// number amid higher ones is a lost event; in practice the
    /// iteration-1 [`ChannelEventSink`][crate::event::ChannelEventSink]
    /// only drops once the receiver is closed, so losses are trailing —
    /// a trace that ends early — and are surfaced by
    /// [`EventSink::dropped`][crate::event::EventSink::dropped] instead.
    pub sequence: u64,
    /// When the sink observed the event: Unix epoch milliseconds, UTC.
    ///
    /// `None` for rows persisted before schema v21; replay must not
    /// invent durations from a missing timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_millis: Option<u64>,
    /// The task this event belongs to.
    pub task_id: TaskId,
    /// What happened.
    pub kind: EventKind,
}

/// The effective model-request retry policy for one task, as values the
/// frontend can label (§6.3).
///
/// This is a wire projection of the kernel's private
/// [`RetryPolicy`](crate::agent::RetryPolicy), not a separate policy: the
/// kernel remains the only authority that decides retries. Carried on
/// `ModelRetry` so a frontend never mirrors backoff defaults that an
/// embedder may have overridden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicySnapshot {
    /// Total attempts allowed per request.
    pub max_attempts: u32,
    /// First retry backoff, in milliseconds.
    pub initial_backoff_ms: u64,
    /// Exponential growth between retries.
    pub backoff_factor: u32,
    /// Cap on the computed backoff, in milliseconds.
    pub max_backoff_ms: u64,
    /// Jitter around the computed backoff, in whole percent.
    pub jitter_percent: u32,
}

/// What happened during a task.
///
/// Variants follow the agent loop (§5, §13) in emission order.
/// `#[non_exhaustive]` keeps the **Rust API** evolvable — downstream
/// `match`es keep compiling when variants are added. It does nothing for
/// the wire: an older consumer cannot parse a newer tag (there is no
/// catch-all), so adding a variant is a wire change that bumps
/// [`EVENT_SCHEMA_VERSION`].
///
/// Serialized with an internal `type` tag (not `kind`, which collides
/// with [`EventKind::ModelRetry`]'s own `kind` field naming the error
/// classification).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A task started; its input was appended to the transcript.
    TaskStarted,
    /// Turn `turn` (1-based) of the agent loop started.
    TurnStarted {
        /// 1-based turn number within the task.
        turn: u32,
    },
    /// A pruning pass froze a **new** checkpoint during this turn's
    /// compile: from here on, the listed tool results project as stubs.
    ///
    /// Emitted once per freeze — turns that merely re-apply the frozen
    /// stubs stay silent, so consumers can attribute savings per
    /// mechanism (sum `reclaimed_tokens` across these events,
    /// deduplicating by `call_id`; roadmap item 20.1).
    ContextPruned {
        /// 1-based turn whose compile froze the checkpoint.
        turn: u32,
        /// Trigger arm plus per-call attribution.
        report: PruneReport,
    },
    /// Successful compilation of the request about to be sent to the model.
    /// Emitted after context pruning/handoff and before streaming, once per
    /// compile (not once per retry attempt). `remaining_budget` is sampled at
    /// this compile boundary; `None` fields mean the task has no configured
    /// ceiling for that resource.
    ContextProgress {
        /// 1-based task turn for this compiled request.
        turn: u32,
        /// Context window split between input allowance and output reserve.
        context_budget: ContextBudget,
        /// Compiler's estimates for the request projection.
        census: ContextCensus,
        /// Remaining task-level resource headroom at this point.
        remaining_budget: RemainingBudget,
    },
    /// A deterministic fast path solved the task with zero model calls
    /// (roadmap item 3): the engine analyzed the instruction, applied
    /// mechanical edits, and verified them green before any provider
    /// traffic. Emitted between `TaskStarted` and `TaskCompleted`;
    /// the task summary reports `model_requests == 0`.
    FastPathHit {
        /// The analyzer's classification (`"rename"`, `"constant"`,
        /// `"format"`), for filtering and attribution.
        kind: String,
        /// Files the fast path rewrote before verification.
        files_changed: u32,
    },
    /// The engine attempted a fast path and fell through to the agent
    /// loop: the analyzer matched no intent, or the solver declined
    /// its guards (never-worse outranks coverage). Emitted after the
    /// task's terminal events — the attempt happens before a task
    /// record exists, so this reads as a post-hoc annotation on the
    /// model-driven task that resulted. The workspace was untouched.
    FastPathDeclined {
        /// Decline reason: `"no-intent-match"`,
        /// `"intent-{kind}-unsolved"` (matched, but a guard or the
        /// offline verifier said no), or — for renames since schema
        /// v15 — the specific guard (`intent-rename-collision`,
        /// `intent-rename-zero-occurrences`, …; see
        /// [`EVENT_SCHEMA_VERSION`]'s history).
        reason: String,
    },
    /// The authority that fixed the configured model's capabilities,
    /// emitted once per engine instance on its first task: catalog row,
    /// provider table, or — under replay — the recording itself.
    /// Surfaced because a silent table fallback after a failed catalog
    /// fetch is exactly the kind of under-the-hood box this event
    /// exists to unbox.
    CapabilitiesResolved {
        /// The model id the resolution fixed.
        model: String,
        /// `"catalog"`, `"provider-table"`, or `"recording"`.
        source: String,
    },
    /// A harness-level observation that never had a first-class event:
    /// per-command deviations reported out of the tools layer (sandbox
    /// passthrough, a stream truncated by a pipe-holding descendant)
    /// plus one-shot baselines (the sandbox mode the policy resolved
    /// to). Categories are stable strings; unknown categories must be
    /// tolerated by consumers.
    HarnessNotice {
        /// Dotted category, e.g. `"sandbox.passthrough"`,
        /// `"stream.truncated"`.
        category: String,
        /// One-line human-readable detail.
        detail: String,
    },
    /// The loop spent one model call on an LLM handoff (roadmap item
    /// 12's last resort): the projection from here on is brief +
    /// handoff summary + tail, and the next compile retries with that
    /// view. Emitted once per handoff — a task never hands off twice.
    ContextHandoff {
        /// 1-based turn whose compile blew the budget and triggered the
        /// handoff.
        turn: u32,
        /// Character length of the handoff summary frozen into the
        /// checkpoint.
        chars: usize,
    },
    /// A model request attempt started. `attempt` is 1-based; retries
    /// emit this again with the next attempt number.
    ModelRequestStarted {
        /// 1-based attempt number for this request.
        attempt: u32,
    },
    /// Provisional text delta from the streaming response. Discard on
    /// [`EventKind::ModelRetry`]; only `ModelCompleted` is authoritative.
    TextDelta {
        /// The delta text, exactly as received.
        text: String,
    },
    /// Provisional reasoning delta from the streaming response. Same
    /// provisional rule as [`EventKind::TextDelta`].
    ReasoningDelta {
        /// The delta text, exactly as received.
        text: String,
    },
    /// The current attempt failed and will be retried after `delay_ms`.
    ///
    /// Ends the attempt: consumers must discard every delta received
    /// since the preceding `ModelRequestStarted` / `ModelRetry`.
    /// `response_started` records whether any stream bytes arrived before
    /// the failure (useful for diagnosing connect vs mid-stream drops).
    ModelRetry {
        /// 1-based attempt number that just failed.
        attempt: u32,
        /// Stable classification of the failure; retryable kinds are the
        /// only reason this event exists.
        kind: ModelErrorKind,
        /// Backoff before the next attempt, in milliseconds.
        delay_ms: u64,
        /// Whether the failed attempt had begun receiving a response.
        response_started: bool,
        /// HTTP status that produced the failure, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
        /// The effective policy that decided this retry. Absent in traces
        /// written before schema v22.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<RetryPolicySnapshot>,
        /// The provider's stream-liveness window in milliseconds: the
        /// silent gap after which this attempt would fail as a transport
        /// error. Not a retry-policy value. Absent in traces written
        /// before schema v23.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle_timeout_ms: Option<u64>,
    },
    /// The model finished; carries the authoritative
    /// [`ModelResponse`] (complete message + stop reason + usage). This is
    /// the only source of committed assistant output. The message is
    /// serialized in full — including reasoning `provider_data` — so a
    /// `--json` trace has the same fidelity as the saved session; treat
    /// shared traces with the same care as session files.
    ModelCompleted {
        /// The authoritative response for the completed attempt.
        response: ModelResponse,
    },
    /// The engine started executing one tool call. `call_id` correlates
    /// with the assistant's `ToolCall` and the eventual
    /// `ToolCompleted`.
    ToolStarted {
        /// Name of the tool being executed.
        name: String,
        /// Correlation id of the call being executed.
        call_id: ToolCallId,
        /// Serialized call arguments, head-bounded by
        /// [`ARGUMENTS_EVENT_HEAD`] bytes — eval forensics reads what the
        /// model asked for without the event stream reproducing whole
        /// file bodies (schema 11).
        arguments: String,
    },
    /// The operator was asked to approve a file write before it ran.
    /// `call_id` correlates with the `ToolStarted` for the same write.
    ApprovalRequested {
        /// Name of the write tool asking (`write_file`).
        name: String,
        /// Correlation id of the write call awaiting approval.
        call_id: ToolCallId,
        /// Workspace-relative path the write would touch.
        path: String,
    },
    /// The operator answered an `ApprovalRequested`. `allowed` is false when
    /// the write was declined.
    ApprovalDecided {
        /// Correlation id of the write call that was asked about.
        call_id: ToolCallId,
        /// Whether the write was allowed to proceed.
        allowed: bool,
    },
    /// structured outcome lands in the transcript's `ToolResult`; the
    /// next `ModelCompleted` shows what the model did with it).
    ToolCompleted {
        /// Name of the tool that was executed.
        name: String,
        /// Correlation id of the finished call.
        call_id: ToolCallId,
        /// The structured outcome (schema 11): `success`, an `error`
        /// payload, or `cancelled`.
        status: ToolStatus,
        /// First [`CONTENT_EVENT_HEAD`] bytes of the result content —
        /// enough to see the command line and its exit status without
        /// reproducing whole tool outputs (schema 11).
        content_head: String,
    },
    /// A verify-call autofix finished (roadmap item 33): after a failing
    /// verify, the tools layer applied rustc `MachineApplicable`
    /// suggestions and re-ran the same command under a never-worse gate.
    /// Emitted after `ToolCompleted` for the call that carried the
    /// effect; `kept: false` means the gate reverted every edit (schema
    /// 14, additive).
    AutofixApplied {
        /// Machine-applicable edits applied to disk before the gate.
        edits: u32,
        /// Distinct workspace files those edits touched.
        files: u32,
        /// Error+warning diagnostic count of the failing run.
        diagnostics_before: u32,
        /// Diagnostic count after the re-run (`<= before` when kept).
        diagnostics_after: u32,
        /// Whether the edits survived the never-worse gate.
        kept: bool,
    },
    /// The kernel-owned todo list changed during the turn's tool batch
    /// (roadmap item 8 phase B): a `todo` create/add/mark bumped its
    /// revision. Emitted once per turn whose batch mutated the list,
    /// after the turn commits and after the new block is durable at the
    /// transcript tail. The payload carries the complete list after the
    /// change, so replaying the `TodoUpdated` events reconstructs both
    /// the list and every tail re-append byte-stably — each event is
    /// exactly one re-append of [`TodoList::render`]'s output.
    TodoUpdated {
        /// The list's revision after the change; bumped on every
        /// mutation, never reset.
        revision: u64,
        /// The complete item list after the change, in list order.
        items: Vec<TodoItem>,
    },
    /// Queued user steering joined the transcript at this turn's
    /// boundary (item 25): every message drained from
    /// [`TaskControl::steer`](crate::agent::TaskControl::steer) was
    /// appended as a user item — persist-then-commit — before the
    /// turn's compile, so the model sees it this turn. Emitted once
    /// per boundary with a non-empty drain, after `TurnStarted`.
    SteeringApplied {
        /// 1-based turn whose compile will see the messages.
        turn: u32,
        /// How many queued messages were applied.
        messages: u32,
    },
    /// The staged turn budget was extended at this turn's boundary
    /// (item 26): the initial grant ran out and the boundary showed
    /// deterministic progress evidence, so the loop continues with
    /// `new_limit` turns. Pure marker — replay needs no state from it.
    TurnBudgetExtended {
        /// 1-based turn whose compile runs under the extended budget.
        turn: u32,
        /// The evidence that justified the grant.
        reason: TurnBudgetReason,
        /// The effective turn limit after this grant (never past the
        /// hard `max_turns` ceiling).
        new_limit: u32,
    },
    /// The loop injected a shaping message into the transcript (schema
    /// 11): one of the one-shot completion refusals, the zero-progress
    /// nudge, or the late-budget nudge. The item also persists in the
    /// session, but the event stream carried no trace of it before —
    /// eval forensics over run.json was blind to which runs had been
    /// nudged, or with what text.
    LoopNudge {
        /// The full injected text.
        text: String,
    },
    /// The task reached terminal success.
    TaskCompleted,
    /// The task reached terminal failure. `reason` is a human-readable
    /// cause; structured state lives in the task record.
    TaskFailed {
        /// Human-readable failure cause.
        reason: String,
        /// Stable model-error classification, when the task failed in a
        /// model request. Absent in traces written before schema v22.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_kind: Option<ModelErrorKind>,
        /// HTTP status behind a model failure, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
        /// Normalized stop reason that ended the task, for the fatal
        /// `max_tokens` / `content_filter` / `context_limit` stops.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },
    /// The task was cancelled (cooperative cancellation won).
    TaskCancelled,
    /// The task was paused between turns: the in-flight turn committed
    /// fully and the session is resumable. Distinct from
    /// [`EventKind::TaskCancelled`], which aborted mid-flight.
    TaskPaused,
    /// The driving engine is idle: the task ended, and its session now
    /// awaits the next user input.
    ///
    /// Emitted by the engine's driving layer right after the terminal
    /// task event ([`EventKind::TaskCompleted`],
    /// [`EventKind::TaskFailed`], [`EventKind::TaskCancelled`], or
    /// [`EventKind::TaskPaused`]), so consumers that render only the event stream learn when more
    /// input can be sent — the signal a GUI or TUI resume loop keys on.
    /// The envelope's `task_id` is the task that just ended; the payload
    /// names the idle session. Being engine-level (not task loop)
    /// observability, it sits outside the task's `events_dropped`
    /// accounting: it is the last event of the burst, and its loss
    /// costs nothing downstream.
    ///
    /// Kernel-level drivers ([`Agent::run_task`](crate::agent::Agent))
    /// do not emit it — only engine wrappers do, so hand-rolled loops
    /// (the eval harness) see no new events.
    AwaitingInput {
        /// The idle session, ready for more input.
        session_id: SessionId,
    },
}
