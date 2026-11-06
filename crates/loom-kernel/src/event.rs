//! The task event stream: best-effort observability protocol.
//!
//! Events are serialisable protocol objects for clients, NDJSON traces,
//! replay of *what was observed*, and future evaluation. They are **not**
//! the source of truth for session state — the transcript is (ADR-004);
//! rebuilding a session never requires events.
//!
//! Delivery is best-effort and non-fatal: observability must not silently
//! alter task semantics. [`EventSink::emit`] returns `()`, so a broken
//! consumer (closed channel, failed NDJSON writer, full disk) can never
//! turn a successful task into a failed one or vice versa.
//! [`EventEnvelope::sequence`] numbers are unique and strictly increasing
//! per sink, and the sink holds its send-side lock across numbering and
//! send — so **arrival order equals sequence order** and streaming
//! consumers can write events as they arrive. Loss is surfaced by the
//! sink's drop count ([`EventSink::dropped`], reported at task end):
//! with the unbounded channel, a dropped event means the receiver is
//! closed, so every later event drops too and a trace simply ends early
//! rather than showing a mid-stream gap.
//!
//! Streaming deltas appear here as [`EventKind::TextDelta`] /
//! [`EventKind::ReasoningDelta`], mirroring the model stream. The same
//! provisional-until-completed rule applies: consumers must discard every
//! delta received since the preceding `ModelRequestStarted` / `ModelRetry`
//! when a [`EventKind::ModelRetry`] fires; only
//! [`EventKind::ModelCompleted`] is authoritative.

use crate::context::PruneReport;
use crate::id::{SessionId, TaskId};
use crate::model::{ModelErrorKind, ModelResponse};
use crate::session::TodoItem;
use crate::store::SharedEventStore;
use crate::transcript::{ToolCallId, ToolStatus};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
pub const EVENT_SCHEMA_VERSION: u16 = 14;

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

/// Head-bounds a string to `budget` bytes on a char boundary, marking
/// the cut. Used by the tool events so the stream stays forensic
/// without reproducing whole file bodies or tool outputs.
pub fn text_head(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &text[..end])
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
    /// iteration-1 [`ChannelEventSink`] only drops once the receiver is
    /// closed, so losses are trailing — a trace that ends early — and
    /// are surfaced by [`EventSink::dropped`] instead.
    pub sequence: u64,
    /// The task this event belongs to.
    pub task_id: TaskId,
    /// What happened.
    pub kind: EventKind,
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
        /// Coarse decline reason: `"no-intent-match"` or
        /// `"intent-{kind}-unsolved"` (matched, but a guard or the
        /// offline verifier said no).
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
    /// The engine finished one tool call (success or failure — the
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

/// Best-effort sink for task events.
///
/// Implementations assign [`EventEnvelope::sequence`] (unique, strictly
/// increasing) and must never block or fail the task on delivery
/// problems. The canonical iteration-1 implementation is
/// [`ChannelEventSink`]; the CLI's NDJSON writer lives on the consumer
/// side of the channel. Several sinks compose into one with
/// [`FanoutSink`] — the shape multi-consumer frontends (GUI, TUI) fan
/// out on.
///
/// Loss visibility is part of the contract: [`dropped`](EventSink::dropped)
/// feeds the task-end `events_dropped` report (§11). The count must be
/// read through this method — it can never travel on the event channel
/// itself, because a sink that drops events would drop the count too.
pub trait EventSink: Send + Sync {
    /// Records one event. Returns nothing: delivery is best-effort.
    fn emit(&self, task_id: TaskId, kind: EventKind);

    /// Events dropped so far, for the task-end `events_dropped` report.
    ///
    /// Default `0` fits sinks that cannot drop; sinks that can must
    /// override so the caller (which may only hold `&dyn EventSink`)
    /// can report loss.
    fn dropped(&self) -> u64 {
        0
    }
}

/// An [`EventSink`] that forwards envelopes over an unbounded tokio MPSC
/// channel.
///
/// Unbounded is deliberate (ADR-004): deltas arrive at token rate, and a
/// merely slow consumer (piped NDJSON, paused terminal) must queue rather
/// than lose events — a lost `ModelRetry` would leave consumers unable to
/// tell which deltas to discard. The memory risk of a stalled consumer is
/// accepted for the CLI; bounded delivery with backpressure is a
/// server-mode revisit. The only drop path is a closed receiver: the
/// event is counted, and the task keeps running. The first drop logs a
/// `tracing::warn!`; the total is available via
/// [`dropped`](ChannelEventSink::dropped) for the task-end
/// `events_dropped` report.
///
/// The sequence counter and drop counter live in an inner `Arc`, so
/// cloning the sink (e.g. per turn) keeps one ordering and one count.
#[derive(Clone)]
pub struct ChannelEventSink {
    inner: Arc<Inner>,
}

struct Inner {
    sender: mpsc::UnboundedSender<EventEnvelope>,
    sequence: AtomicU64,
    dropped: AtomicU64,
    warned: AtomicBool,
    /// Held across numbering *and* send so arrival order always equals
    /// sequence order; lets streaming consumers write events as they
    /// arrive instead of buffering to sort. Uncontended in practice —
    /// one lock per event is cheap next to token-rate traffic.
    send_lock: std::sync::Mutex<()>,
}

impl ChannelEventSink {
    /// Creates a sink and returns it together with its receiving end.
    ///
    /// The channel is unbounded (ADR-004): a slow consumer delays events
    /// in memory instead of dropping them; only a closed receiver drops.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<EventEnvelope>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(Inner {
                    sender,
                    sequence: AtomicU64::new(0),
                    dropped: AtomicU64::new(0),
                    warned: AtomicBool::new(false),
                    send_lock: std::sync::Mutex::new(()),
                }),
            },
            receiver,
        )
    }
}

impl EventSink for ChannelEventSink {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        // The lock spans numbering and send, so arrival order always
        // equals sequence order — consumers can stream events as they
        // arrive. The critical section is tiny and never held across an
        // await.
        let _guard = self.inner.send_lock.lock().unwrap_or_else(|poisoned| {
            // A panic in another emitter poisoned the lock; the state it
            // protects (counters, channel) is still consistent, so keep
            // going rather than failing the task.
            poisoned.into_inner()
        });
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence,
            task_id,
            kind,
        };
        if self.inner.sender.send(envelope).is_err() {
            // Only reachable with a closed receiver: every later send
            // fails too, so loss is trailing and shows up as a trace
            // that ends early — counted here, not as a mid-stream gap.
            let dropped = self.inner.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Warn once, then stay quiet: the count carries the ongoing
            // signal; per-event warnings would spam an already-struggling
            // consumer's stderr.
            if !self.inner.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(dropped, "event delivery failed; further drops counted only");
            }
        }
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

/// An [`EventSink`] that fans every event out to several child sinks.
///
/// Multi-consumer frontends — a GUI streaming deltas while a tracer
/// writes NDJSON, say — wrap their sinks in one `FanoutSink` and hand
/// that single sink to the driving API. Each child numbers its own
/// stream ([`EventEnvelope::sequence`] is per sink), so every consumer
/// sees a contiguous 0-based view; no ordering holds *across*
/// consumers. Delivery stays best-effort: a child that drops (a closed
/// channel) costs its event and nothing else — siblings still receive,
/// and the task keeps running.
///
/// [`dropped`](EventSink::dropped) sums across children, so the
/// task-end `events_dropped` report counts a loss on any consumer. An
/// empty fan-out is a valid no-op sink.
#[derive(Clone, Default)]
pub struct FanoutSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl FanoutSink {
    /// Creates an empty fan-out, which observes nothing.
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    /// Adds an owned child sink, chainable:
    /// `FanoutSink::new().with(renderer_sink).with(trace_sink)`.
    pub fn with(mut self, sink: impl EventSink + 'static) -> Self {
        self.sinks.push(Arc::new(sink));
        self
    }

    /// Adds a shared child sink, for callers that keep their own
    /// handle (e.g. to poll one consumer's drop count while the task
    /// runs).
    pub fn with_shared(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sinks.push(sink);
        self
    }
}

impl EventSink for FanoutSink {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        for sink in &self.sinks {
            // One clone per child keeps every consumer's stream
            // independent; the payload copy is cheap next to
            // token-rate serialization.
            sink.emit(task_id, kind.clone());
        }
    }

    fn dropped(&self) -> u64 {
        self.sinks.iter().map(|sink| sink.dropped()).sum()
    }
}

/// Coarse task phase for [`StatusSnapshot`], folded from terminal events.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// No task observed yet, or the engine went idle awaiting input.
    #[default]
    Idle,
    /// Between `TaskStarted` and a terminal event.
    Running,
    /// Terminal success (`TaskCompleted`).
    Completed,
    /// Terminal failure (`TaskFailed`).
    Failed,
    /// Cancelled mid-flight (`TaskCancelled`).
    Cancelled,
    /// Paused between turns (`TaskPaused`); the session is resumable.
    Paused,
}

/// A pollable progress picture of one task, folded from the event stream.
///
/// Derived state, never the source of truth (ADR-004 — the transcript
/// is): rebuild it at any time by replaying events. Maintained by
/// [`StatusRecorder`]; frontends poll
/// [`snapshot`](StatusRecorder::snapshot) on their own cadence (a TUI
/// status line, a GUI progress pane). The fields answer "where is this
/// task right now": which turn, which tools are executing, which
/// request attempt, and what phase the task ended in.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// The task this snapshot describes, from the latest envelope seen.
    pub task_id: Option<TaskId>,
    /// The idle session after [`EventKind::AwaitingInput`], once
    /// observed — the id a resume loop sends the next input to.
    pub session_id: Option<SessionId>,
    /// 1-based turn now running (the latest `TurnStarted`).
    pub turn: Option<u32>,
    /// Names of tool calls started but not yet completed, in start
    /// order — usually one; more when a batch runs concurrently.
    pub in_flight_tools: Vec<String>,
    /// The running model-request attempt (the latest
    /// `ModelRequestStarted`), cleared on `ModelCompleted`.
    pub attempt: Option<u32>,
    /// Coarse task phase.
    pub phase: TaskPhase,
}

/// An [`EventSink`] that folds the stream into a pollable
/// [`StatusSnapshot`] — the progress surface frontends poll while a
/// task runs (current turn, in-flight tools, running attempt, phase).
///
/// Clone it and hand one clone to the driving API — directly, or as a
/// [`FanoutSink`] child via `with(recorder.clone())` — and keep the
/// other for polling: clones share one snapshot, so the retained handle
/// observes everything emitted. It is a pure in-memory tally: it cannot
/// drop events, so it is a transparent inner choice for [`StrictSink`].
///
/// One recorder tracks one task's stream. Frontends driving concurrent
/// tasks give each run its own recorder (its own fan-out branch);
/// envelopes from interleaved tasks would otherwise overwrite each
/// other's fields.
#[derive(Clone, Default)]
pub struct StatusRecorder {
    inner: Arc<StatusInner>,
}

/// Shared mutable fold state; the lock is held only across one `emit`
/// and never across an await.
#[derive(Default)]
struct StatusInner {
    state: std::sync::Mutex<RecorderState>,
}

/// What [`StatusRecorder`] actually tracks; projected into
/// [`StatusSnapshot`] on poll.
#[derive(Default)]
struct RecorderState {
    task_id: Option<TaskId>,
    session_id: Option<SessionId>,
    turn: Option<u32>,
    attempt: Option<u32>,
    phase: TaskPhase,
    /// Correlation id + name per started-but-not-completed tool call,
    /// so concurrent same-name calls stay distinct.
    in_flight: Vec<(ToolCallId, String)>,
}

impl StatusRecorder {
    /// Creates a recorder whose snapshot starts `Idle` with no fields
    /// set — the state before any event is observed.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current progress picture (a cheap clone of small state).
    pub fn snapshot(&self) -> StatusSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        StatusSnapshot {
            task_id: state.task_id,
            session_id: state.session_id,
            turn: state.turn,
            in_flight_tools: state
                .in_flight
                .iter()
                .map(|(_, name)| name.clone())
                .collect(),
            attempt: state.attempt,
            phase: state.phase,
        }
    }
}

impl EventSink for StatusRecorder {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.task_id = Some(task_id);
        match kind {
            EventKind::TaskStarted => state.phase = TaskPhase::Running,
            EventKind::TurnStarted { turn } => state.turn = Some(turn),
            EventKind::ModelRequestStarted { attempt } => state.attempt = Some(attempt),
            EventKind::ModelCompleted { .. } => state.attempt = None,
            EventKind::ToolStarted {
                name,
                call_id,
                arguments: _,
            } => {
                state.in_flight.push((call_id, name));
            }
            EventKind::ToolCompleted { call_id, .. } => {
                state.in_flight.retain(|(id, _)| *id != call_id);
            }
            EventKind::TaskCompleted => state.phase = TaskPhase::Completed,
            EventKind::TaskFailed { .. } => state.phase = TaskPhase::Failed,
            EventKind::TaskCancelled => state.phase = TaskPhase::Cancelled,
            EventKind::TaskPaused => state.phase = TaskPhase::Paused,
            EventKind::AwaitingInput { session_id } => {
                state.session_id = Some(session_id);
                state.phase = TaskPhase::Idle;
            }
            // Deltas and prune reports do not move the coarse picture.
            _ => {}
        }
    }
}

/// An [`EventSink`] that cancels the task when an event is lost.
///
/// Evaluation runs must fail on event loss rather than silently accepting a
/// truncated trace (ADR-004 addendum, item 3). This sink wraps another sink
/// and, whenever a call to [`emit`](EventSink::emit) increases the inner
/// sink's drop count, triggers the supplied [`CancellationToken`]. The loop
/// observes that token at its next cancellation checkpoint and ends the task
/// as [`TaskError::Cancelled`](crate::agent::TaskError::Cancelled), so a lossy
/// run is reported as a failure instead of a clean success.
///
/// Wrapping a sink that cannot drop — whose [`dropped`](EventSink::dropped)
/// stays `0`, such as an in-process tally — is a transparent pass-through: the
/// token is never triggered. The strict mode only bites when the inner sink can
/// actually lose events.
pub struct StrictSink<S: EventSink> {
    inner: S,
    cancellation: CancellationToken,
}

impl<S: EventSink> StrictSink<S> {
    /// Wraps `inner`; `cancellation` must be the same token the task runs
    /// under, so loss trips the cancellation the loop already observes.
    pub fn new(inner: S, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }

    /// Borrows the wrapped sink (its own counters, e.g. an event tally).
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: EventSink> EventSink for StrictSink<S> {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        let before = self.inner.dropped();
        self.inner.emit(task_id, kind);
        if self.inner.dropped() > before {
            self.cancellation.cancel();
        }
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
}

/// An [`EventSink`] that persists every event it observes to an
/// [`EventStore`] before forwarding it to a wrapped sink (roadmap item
/// 1's open tail: persist the event stream alongside the transcript).
///
/// Composition: the engine wraps the frontend's sink in one of these
/// when session persistence is on, so the durable stream sees exactly
/// what the frontend saw — same events, same order, including the
/// engine-level markers the driving layer emits after a task. Write
/// cadence matches the transcript seams: **append-on-emit**, each
/// envelope durable before it reaches the inner consumer
/// (persist-then-forward, mirroring ADR-012's persist-then-commit).
///
/// The sink numbers its **own** stream (unique, strictly increasing
/// from 0) exactly like any other sink: `sequence` is per sink, so a
/// resumed session's second run numbers from zero again and the store
/// orders rows by insertion, not by sequence. The send-side lock spans
/// numbering, persist, and forward, so both the stored rows and the
/// inner consumer see events in sequence order.
///
/// Persistence is best-effort (ADR-004): a store failure is counted —
/// surfaced through [`dropped`](EventSink::dropped) alongside the inner
/// sink's own drops — and warned once, never propagated. Observability
/// must not fail a task, not even when its durability hiccups.
pub struct PersistingEventSink<'a> {
    store: SharedEventStore,
    session: SessionId,
    inner: &'a dyn EventSink,
    state: Arc<PersistState>,
}

/// Shared numbering and failure state; the lock is held across one
/// `emit` and never across an await.
struct PersistState {
    sequence: AtomicU64,
    persist_failures: AtomicU64,
    warned: AtomicBool,
    send_lock: std::sync::Mutex<()>,
}

impl<'a> PersistingEventSink<'a> {
    /// Wraps `inner` so every event is appended to `store` under
    /// `session` before `inner` sees it.
    pub fn new(store: SharedEventStore, session: SessionId, inner: &'a dyn EventSink) -> Self {
        Self {
            store,
            session,
            inner,
            state: Arc::new(PersistState {
                sequence: AtomicU64::new(0),
                persist_failures: AtomicU64::new(0),
                warned: AtomicBool::new(false),
                send_lock: std::sync::Mutex::new(()),
            }),
        }
    }

    /// Events whose persistence failed so far.
    pub fn persist_failures(&self) -> u64 {
        self.state.persist_failures.load(Ordering::Relaxed)
    }
}

impl EventSink for PersistingEventSink<'_> {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        // The lock spans numbering, persist, and forward: both the
        // stored rows and the inner consumer observe sequence order.
        // The critical section is tiny and never held across an await.
        let _guard = self.state.send_lock.lock().unwrap_or_else(|poisoned| {
            // A panic in another emitter poisoned the lock; the state
            // it protects (counters, store) is still consistent, so
            // keep going rather than failing the task.
            poisoned.into_inner()
        });
        let sequence = self.state.sequence.fetch_add(1, Ordering::Relaxed);
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence,
            task_id,
            kind,
        };
        if let Err(error) = self.store.append_event(&self.session, &envelope) {
            // Counted, not fatal: the event still forwards, and the
            // task keeps running (ADR-004). Warn once, then stay quiet
            // — the count carries the ongoing signal.
            let failures = self.state.persist_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.state.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(failures, %error, "event persistence failed; further failures counted only");
            }
        }
        self.inner.emit(envelope.task_id, envelope.kind);
    }

    fn dropped(&self) -> u64 {
        self.persist_failures() + self.inner.dropped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PruneTrigger, PrunedCall, StubReason};
    use crate::model::{StopReason, Usage};
    use crate::transcript::AssistantMessage;

    #[test]
    fn sequences_are_contiguous_from_zero() {
        let (sink, mut receiver) = ChannelEventSink::new();
        let task = TaskId::new();
        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TurnStarted { turn: 1 });
        sink.emit(task, EventKind::ModelRequestStarted { attempt: 1 });
        sink.emit(task, EventKind::TextDelta { text: "hi".into() });
        sink.emit(task, EventKind::TaskCompleted);

        let mut expected = 0u64;
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.sequence, expected);
            assert_eq!(event.schema_version, EVENT_SCHEMA_VERSION);
            assert_eq!(event.task_id, task);
            expected += 1;
        }
        assert_eq!(expected, 5);
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn closed_receiver_drops_are_counted() {
        let (sink, receiver) = ChannelEventSink::new();
        drop(receiver); // consumer gone: every emit must drop, not fail

        let task = TaskId::new();
        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TaskCompleted);

        assert_eq!(sink.dropped(), 3);
    }

    /// Schema v3: `ContextPruned` serializes with the snake_case tag and
    /// a fully-nested report, so an NDJSON consumer can attribute
    /// savings per mechanism without kernel types.
    #[test]
    fn context_pruned_serializes_the_nested_report() {
        let report = PruneReport {
            trigger: PruneTrigger::AgeMask,
            calls: vec![PrunedCall {
                call_id: "c9".to_owned(),
                reason: StubReason::TurnAge,
                reclaimed_tokens: 1_234,
            }],
            reclaimed_tokens: 1_234,
        };
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 7,
            task_id: TaskId::new(),
            kind: EventKind::ContextPruned { turn: 3, report },
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert_eq!(value["kind"]["type"], "context_pruned");
        assert_eq!(value["kind"]["turn"], 3);
        assert_eq!(value["kind"]["report"]["trigger"], "age_mask");
        assert_eq!(value["kind"]["report"]["reclaimed_tokens"], 1_234);
        assert_eq!(value["kind"]["report"]["calls"][0]["call_id"], "c9");
        assert_eq!(value["kind"]["report"]["calls"][0]["reason"], "TurnAge");
        assert_eq!(
            value["kind"]["report"]["calls"][0]["reclaimed_tokens"],
            1_234
        );
        let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, envelope);
    }

    /// Schema v4: `AwaitingInput` serializes with the snake_case tag and
    /// the idle session's id, so a stream-only consumer can key its resume
    /// loop on the marker alone.
    #[test]
    fn awaiting_input_serializes_the_session_id() {
        let session_id = SessionId::new();
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 11,
            task_id: TaskId::new(),
            kind: EventKind::AwaitingInput { session_id },
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert_eq!(value["kind"]["type"], "awaiting_input");
        assert_eq!(
            value["kind"]["session_id"],
            serde_json::to_value(session_id).expect("id serializes")
        );
        let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, envelope);
    }

    /// Schema v5: `TaskPaused` serializes under the snake_case tag so a
    /// paused (resumable) ending is distinguishable from a mid-flight
    /// cancellation on the event stream alone.
    #[test]
    fn task_paused_serializes_with_its_tag() {
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 7,
            task_id: TaskId::new(),
            kind: EventKind::TaskPaused,
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert_eq!(value["kind"]["type"], "task_paused");
        let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, envelope);
    }

    /// Schema v8: the surfaced-decision kinds serialize under their
    /// snake_case tags and round-trip, so NDJSON consumers can read
    /// harness decisions straight off the event stream.
    #[test]
    fn surfaced_decisions_serialize_and_round_trip() {
        let cases = vec![
            (
                EventKind::FastPathDeclined {
                    reason: "intent-rename-unsolved".to_owned(),
                },
                "fast_path_declined",
                serde_json::json!({ "reason": "intent-rename-unsolved" }),
            ),
            (
                EventKind::CapabilitiesResolved {
                    model: "z-ai/glm-5.3-flash".to_owned(),
                    source: "catalog".to_owned(),
                },
                "capabilities_resolved",
                serde_json::json!({
                    "model": "z-ai/glm-5.3-flash",
                    "source": "catalog"
                }),
            ),
            (
                EventKind::HarnessNotice {
                    category: "sandbox.passthrough".to_owned(),
                    detail: "\"echo\" ran unconfined".to_owned(),
                },
                "harness_notice",
                serde_json::json!({
                    "category": "sandbox.passthrough",
                    "detail": "\"echo\" ran unconfined"
                }),
            ),
        ];
        for (kind, tag, fields) in cases {
            let envelope = EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: 7,
                task_id: TaskId::new(),
                kind: kind.clone(),
            };
            let value = serde_json::to_value(&envelope).expect("serializes");
            assert_eq!(value["kind"]["type"], tag);
            for (field, expected) in fields.as_object().expect("object") {
                assert_eq!(&value["kind"][field], expected, "{tag}.{field}");
            }
            let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
            assert_eq!(round, envelope);
        }
    }

    #[test]
    fn dyn_sink_reports_real_drop_count() {
        // The agent loop will hold `&dyn EventSink`; the default
        // `dropped() == 0` must not mask a dropping sink's count.
        let (sink, receiver) = ChannelEventSink::new();
        drop(receiver);
        let dyn_sink: &dyn EventSink = &sink;
        dyn_sink.emit(TaskId::new(), EventKind::TaskStarted);
        assert_eq!(dyn_sink.dropped(), 1);
    }

    #[test]
    fn slow_consumer_queues_instead_of_dropping() {
        // Unbounded by design (ADR-004): a consumer that never receives
        // while a token-rate burst is emitted must lose nothing — the
        // whole burst queues and drains in order afterwards.
        let (sink, mut receiver) = ChannelEventSink::new();
        let task = TaskId::new();
        for i in 0..1000 {
            sink.emit(
                task,
                EventKind::TextDelta {
                    text: i.to_string(),
                },
            );
        }
        assert_eq!(sink.dropped(), 0);

        let mut expected = 0u64;
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.sequence, expected);
            expected += 1;
        }
        assert_eq!(expected, 1000);
    }

    #[test]
    fn clones_share_sequence_and_drop_count() {
        let (sink, mut receiver) = ChannelEventSink::new();
        let cloned = sink.clone();
        let task = TaskId::new();
        sink.emit(task, EventKind::TaskStarted);
        cloned.emit(task, EventKind::TaskStarted);

        let a = receiver.try_recv().unwrap();
        let b = receiver.try_recv().unwrap();
        assert_eq!((a.sequence, b.sequence), (0, 1));
        drop(receiver);
        cloned.emit(task, EventKind::TaskCancelled);
        assert_eq!(sink.dropped(), 1);
        assert_eq!(cloned.dropped(), 1);
    }

    #[test]
    fn concurrent_emitters_arrive_in_sequence_order() {
        // The send-side lock spans numbering and send, so even with
        // several threads emitting, the receiver sees sequences in
        // strictly increasing order — no buffering-to-sort needed.
        let (sink, mut receiver) = ChannelEventSink::new();
        let threads = 4;
        let per_thread = 250;

        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let sink = sink.clone();
                std::thread::spawn(move || {
                    let task = TaskId::new();
                    for _ in 0..per_thread {
                        sink.emit(
                            task,
                            EventKind::TextDelta {
                                text: String::new(),
                            },
                        );
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(sink.dropped(), 0);

        let mut last = None;
        let mut count = 0u64;
        while let Ok(event) = receiver.try_recv() {
            if let Some(last) = last {
                assert_eq!(event.sequence, last + 1, "arrival order broke");
            } else {
                assert_eq!(event.sequence, 0);
            }
            last = Some(event.sequence);
            count += 1;
        }
        assert_eq!(count, threads * per_thread);
        assert_eq!(last, Some(count - 1));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let response = ModelResponse {
            message: AssistantMessage {
                content: "done".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: Some(3),
                thinking_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
        };
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 7,
            task_id: TaskId::new(),
            kind: EventKind::ModelCompleted { response },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, back);
    }

    #[test]
    fn event_kinds_use_snake_case_tags() {
        let task = TaskId::new();
        let retry = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 0,
            task_id: task,
            kind: EventKind::ModelRetry {
                attempt: 2,
                kind: ModelErrorKind::RateLimited,
                delay_ms: 4000,
                response_started: true,
            },
        };
        let json = serde_json::to_value(&retry).unwrap();
        assert_eq!(json["kind"]["type"], "model_retry");
        assert_eq!(json["kind"]["kind"], "rate_limited");
        assert_eq!(json["kind"]["attempt"], 2);
        assert_eq!(json["kind"]["delay_ms"], 4000);
        assert_eq!(json["kind"]["response_started"], true);
    }

    #[test]
    fn strict_sink_cancels_the_task_on_the_first_drop() {
        // The motivating consumer (ADR-004 addendum, item 3): evaluation
        // must not silently accept a truncated trace.
        let (inner, receiver) = ChannelEventSink::new();
        drop(receiver); // consumer gone: the next emit drops
        let token = CancellationToken::new();
        let sink = StrictSink::new(inner, token.clone());

        assert!(!token.is_cancelled());
        sink.emit(TaskId::new(), EventKind::TaskStarted);
        assert!(token.is_cancelled(), "loss must trip the task's token");
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn strict_sink_is_a_transparent_passthrough_when_nothing_is_dropped() {
        let (inner, mut receiver) = ChannelEventSink::new();
        let token = CancellationToken::new();
        let sink = StrictSink::new(inner, token.clone());
        let task = TaskId::new();

        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TaskCompleted);

        assert!(!token.is_cancelled());
        assert_eq!(sink.dropped(), 0);
        assert_eq!(receiver.try_recv().unwrap().sequence, 0);
        assert_eq!(receiver.try_recv().unwrap().sequence, 1);
    }

    #[test]
    fn strict_sink_never_cancels_for_a_sink_that_cannot_drop() {
        // Wrapping an in-process tally (whose `dropped()` is the default 0)
        // must not spuriously cancel.
        struct Null;
        impl EventSink for Null {
            fn emit(&self, _task_id: TaskId, _kind: EventKind) {}
        }

        let token = CancellationToken::new();
        let sink = StrictSink::new(Null, token.clone());
        for _ in 0..10 {
            sink.emit(TaskId::new(), EventKind::TurnStarted { turn: 1 });
        }
        assert!(!token.is_cancelled());
        assert_eq!(sink.dropped(), 0);
    }

    /// An event store that always fails, to pin the best-effort
    /// contract: persistence failures are counted, never propagated.
    struct FailingStore;

    impl crate::store::EventStore for FailingStore {
        fn append_event(
            &self,
            _session: &SessionId,
            _envelope: &EventEnvelope,
        ) -> Result<(), crate::store::StoreError> {
            Err(crate::store::StoreError::Backend("disk gone".to_owned()))
        }

        fn load_events(
            &self,
            _session: &SessionId,
        ) -> Result<Vec<EventEnvelope>, crate::store::StoreError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn persisting_sink_round_trips_through_the_store() {
        use crate::store::{EventStore, MemoryEventStore};

        let session = SessionId::new();
        let store = std::sync::Arc::new(MemoryEventStore::with_session(session));
        let (inner, mut receiver) = ChannelEventSink::new();

        let sink = PersistingEventSink::new(store.clone(), session, &inner);
        let task = TaskId::new();
        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TurnStarted { turn: 1 });
        sink.emit(
            task,
            EventKind::ModelRetry {
                attempt: 1,
                kind: ModelErrorKind::RateLimited,
                delay_ms: 500,
                response_started: false,
            },
        );

        // The inner consumer still receives everything, in order.
        let mut seen = 0u64;
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.sequence, seen);
            seen += 1;
        }
        assert_eq!(seen, 3);

        // The stored stream round-trips intact: same envelopes, same
        // order, sequence contiguous from zero.
        let stored = store.load_events(&session).unwrap();
        assert_eq!(stored.len(), 3);
        for (index, event) in stored.iter().enumerate() {
            assert_eq!(event.sequence, index as u64);
            assert_eq!(event.task_id, task);
            assert_eq!(event.schema_version, EVENT_SCHEMA_VERSION);
        }
        assert_eq!(
            stored[2].kind,
            EventKind::ModelRetry {
                attempt: 1,
                kind: ModelErrorKind::RateLimited,
                delay_ms: 500,
                response_started: false,
            }
        );
        assert_eq!(sink.persist_failures(), 0);
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn persisting_failures_are_counted_and_never_fatal() {
        let (inner, receiver) = ChannelEventSink::new();
        drop(receiver); // the inner consumer is gone too: both loss paths fire

        let sink =
            PersistingEventSink::new(std::sync::Arc::new(FailingStore), SessionId::new(), &inner);
        let task = TaskId::new();
        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TaskCompleted);

        assert_eq!(sink.persist_failures(), 2);
        assert_eq!(sink.dropped(), 4, "two persist failures + two inner drops");
    }

    #[test]
    fn persisting_sink_delegates_drops_when_the_store_is_healthy() {
        use crate::store::MemoryEventStore;

        // A memory store cannot fail, but the session is unknown: that
        // is a Corrupt append failure — the exact partial-outage shape
        // the counter must catch without failing the emit.
        let (inner, mut receiver) = ChannelEventSink::new();
        let sink = PersistingEventSink::new(
            std::sync::Arc::new(MemoryEventStore::new()),
            SessionId::new(),
            &inner,
        );
        sink.emit(TaskId::new(), EventKind::TaskStarted);
        assert_eq!(sink.persist_failures(), 1, "unknown session is a failure");
        assert!(receiver.try_recv().is_ok(), "the event still forwarded");
    }

    #[test]
    fn fanout_delivers_every_event_to_every_consumer() {
        let (a, mut rx_a) = ChannelEventSink::new();
        let (b, mut rx_b) = ChannelEventSink::new();
        let sink = FanoutSink::new().with(a).with(b);
        let task = TaskId::new();

        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TextDelta { text: "x".into() });
        sink.emit(task, EventKind::TaskCompleted);

        // Each consumer numbers its own stream from zero.
        for rx in [&mut rx_a, &mut rx_b] {
            let mut expected = 0u64;
            while let Ok(event) = rx.try_recv() {
                assert_eq!(event.sequence, expected);
                assert_eq!(event.task_id, task);
                expected += 1;
            }
            assert_eq!(expected, 3);
        }
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn fanout_counts_drops_but_keeps_healthy_consumers_fed() {
        let (dead, receiver) = ChannelEventSink::new();
        drop(receiver); // this consumer is gone: its events must drop
        let (live, mut rx_live) = ChannelEventSink::new();
        let sink = FanoutSink::new().with(dead).with(live);
        let task = TaskId::new();

        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TaskCompleted);

        assert_eq!(sink.dropped(), 2, "one drop per event on the closed child");
        assert_eq!(
            rx_live.try_recv().unwrap().kind,
            EventKind::TaskStarted,
            "a dead sibling must not starve healthy consumers"
        );
        assert_eq!(rx_live.try_recv().unwrap().kind, EventKind::TaskCompleted);
    }

    #[test]
    fn empty_fanout_observes_nothing() {
        let sink = FanoutSink::new();
        sink.emit(TaskId::new(), EventKind::TaskStarted);
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn fanout_shares_children_through_arc_handles() {
        // `with_shared` is for callers that keep a handle to one child
        // (status polls); the child must stay shared, not owned.
        let (child, mut rx) = ChannelEventSink::new();
        let shared: Arc<dyn EventSink> = Arc::new(child);
        let sink = FanoutSink::new().with_shared(shared.clone());

        sink.emit(TaskId::new(), EventKind::TaskStarted);
        assert!(rx.try_recv().is_ok(), "shared child receives");
        assert_eq!(shared.dropped(), 0, "handle observes the child state");

        drop(sink);
        assert_eq!(
            Arc::strong_count(&shared),
            1,
            "caller keeps the child alive"
        );
    }

    #[test]
    fn status_recorder_tracks_a_task_lifecycle() {
        let recorder = StatusRecorder::new();
        let task = TaskId::new();
        let call = ToolCallId::try_from("call_1".to_owned()).unwrap();

        // Before anything: idle with no fields set.
        let idle = recorder.snapshot();
        assert_eq!(idle.phase, TaskPhase::Idle);
        assert_eq!(idle.task_id, None);

        recorder.emit(task, EventKind::TaskStarted);
        assert_eq!(recorder.snapshot().phase, TaskPhase::Running);

        recorder.emit(task, EventKind::TurnStarted { turn: 1 });
        assert_eq!(recorder.snapshot().turn, Some(1));

        recorder.emit(task, EventKind::ModelRequestStarted { attempt: 1 });
        assert_eq!(recorder.snapshot().attempt, Some(1));

        recorder.emit(task, EventKind::TextDelta { text: "…".into() });
        assert_eq!(
            recorder.snapshot().attempt,
            Some(1),
            "deltas keep the attempt"
        );

        let response = ModelResponse {
            message: AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: None,
                thinking_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
        };
        recorder.emit(task, EventKind::ModelCompleted { response });
        assert_eq!(
            recorder.snapshot().attempt,
            None,
            "completed clears the attempt"
        );

        recorder.emit(
            task,
            EventKind::ToolStarted {
                name: "read_file".into(),
                call_id: call.clone(),
                arguments: "{}".into(),
            },
        );
        assert_eq!(
            recorder.snapshot().in_flight_tools,
            vec!["read_file".to_owned()]
        );

        recorder.emit(
            task,
            EventKind::ToolCompleted {
                name: "read_file".into(),
                call_id: call,
                status: ToolStatus::Success,
                content_head: "ok".into(),
            },
        );
        assert_eq!(recorder.snapshot().in_flight_tools, Vec::<String>::new());

        recorder.emit(task, EventKind::TaskCompleted);
        let done = recorder.snapshot();
        assert_eq!(done.phase, TaskPhase::Completed);
        assert_eq!(done.task_id, Some(task));
        assert_eq!(done.turn, Some(1), "fields persist past the terminal event");

        let session = SessionId::new();
        recorder.emit(
            task,
            EventKind::AwaitingInput {
                session_id: session,
            },
        );
        let idle = recorder.snapshot();
        assert_eq!(idle.phase, TaskPhase::Idle);
        assert_eq!(idle.session_id, Some(session));
    }

    #[test]
    fn status_recorder_shares_one_snapshot_across_clones() {
        // The driving pattern: fan a clone out to the task, keep the
        // original for polling — both must see the same fold state.
        let recorder = StatusRecorder::new();
        let (channel, mut receiver) = ChannelEventSink::new();
        let sink = FanoutSink::new().with(recorder.clone()).with(channel);
        let task = TaskId::new();

        sink.emit(task, EventKind::TaskStarted);
        sink.emit(task, EventKind::TurnStarted { turn: 2 });

        assert_eq!(recorder.snapshot().phase, TaskPhase::Running);
        assert_eq!(recorder.snapshot().turn, Some(2));
        assert_eq!(sink.dropped(), 0, "the tally cannot drop");
        assert_eq!(
            receiver.try_recv().unwrap().kind,
            EventKind::TaskStarted,
            "siblings still receive"
        );
    }

    #[test]
    fn status_recorder_keeps_concurrent_tool_calls_distinct() {
        let recorder = StatusRecorder::new();
        let task = TaskId::new();
        let a = ToolCallId::try_from("call_a".to_owned()).unwrap();
        let b = ToolCallId::try_from("call_b".to_owned()).unwrap();

        recorder.emit(
            task,
            EventKind::ToolStarted {
                name: "shell".into(),
                call_id: a.clone(),
                arguments: "{}".into(),
            },
        );
        recorder.emit(
            task,
            EventKind::ToolStarted {
                name: "shell".into(),
                call_id: b.clone(),
                arguments: "{}".into(),
            },
        );
        assert_eq!(
            recorder.snapshot().in_flight_tools,
            vec!["shell".to_owned(), "shell".to_owned()]
        );

        recorder.emit(
            task,
            EventKind::ToolCompleted {
                name: "shell".into(),
                call_id: a,
                status: ToolStatus::Success,
                content_head: "ok".into(),
            },
        );
        assert_eq!(
            recorder.snapshot().in_flight_tools,
            vec!["shell".to_owned()],
            "completion correlates by call id, not name"
        );
        recorder.emit(
            task,
            EventKind::ToolCompleted {
                name: "shell".into(),
                call_id: b,
                status: ToolStatus::Success,
                content_head: "ok".into(),
            },
        );
        assert_eq!(recorder.snapshot().in_flight_tools, Vec::<String>::new());
    }

    #[test]
    fn status_snapshot_survives_a_json_round_trip() {
        // IPC-ready: a GUI may shuttle snapshots across a process
        // boundary; the shape must round-trip like the events it
        // derives from.
        let snapshot = StatusSnapshot {
            task_id: Some(TaskId::new()),
            session_id: Some(SessionId::new()),
            turn: Some(3),
            in_flight_tools: vec!["edit_file".to_owned()],
            attempt: Some(1),
            phase: TaskPhase::Running,
        };
        let value = serde_json::to_value(&snapshot).expect("serializes");
        assert_eq!(value["phase"], "running");
        assert_eq!(value["in_flight_tools"][0], "edit_file");
        let round: StatusSnapshot = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, snapshot);
    }

    /// Schema v9: `TodoUpdated` serializes with the snake_case tag and
    /// the complete item list, so an NDJSON consumer can reconstruct the
    /// session's todo state without kernel types beyond serde.
    #[test]
    fn todo_updated_serializes_the_full_list() {
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 7,
            task_id: TaskId::new(),
            kind: EventKind::TodoUpdated {
                revision: 3,
                items: vec![TodoItem {
                    id: "1".to_owned(),
                    text: "write the test".to_owned(),
                    status: crate::session::TodoStatus::Done,
                }],
            },
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert_eq!(value["kind"]["type"], "todo_updated");
        assert_eq!(value["kind"]["revision"], 3);
        assert_eq!(value["kind"]["items"][0]["id"], "1");
        assert_eq!(value["kind"]["items"][0]["status"], "done");
        let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, envelope);
    }

    /// Roadmap item 8 phase B, replay: folding a task's `TodoUpdated`
    /// Schema v12: `TurnBudgetExtended` serializes with the snake_case
    /// tag and a snake_case reason, so an NDJSON consumer can count
    /// extensions and their evidence without kernel types.
    #[test]
    fn turn_budget_extended_serializes_snake_case() {
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 3,
            task_id: TaskId::new(),
            kind: EventKind::TurnBudgetExtended {
                turn: 4,
                reason: TurnBudgetReason::UnverifiedWork,
                new_limit: 12,
            },
        };
        let value = serde_json::to_value(&envelope).expect("json");
        assert_eq!(value["kind"]["type"], "turn_budget_extended");
        assert_eq!(value["kind"]["turn"], 4);
        assert_eq!(value["kind"]["reason"], "unverified_work");
        assert_eq!(value["kind"]["new_limit"], 12);
    }

    /// events in sequence order reconstructs the list — and each event
    /// corresponds to exactly one tail re-append, so a replayer renders
    /// the same byte-stable blocks the transcript carries.
    #[test]
    fn todo_updated_events_replay_into_the_list_and_its_blocks() {
        let mut live = crate::session::TodoList::default();
        let mut emitted: Vec<EventKind> = Vec::new();

        // The loop's emission shape: after each mutating batch, the
        // event carries the whole list.
        live.add("write the test");
        live.add("fix the bug");
        emitted.push(EventKind::TodoUpdated {
            revision: live.revision,
            items: live.items.clone(),
        });
        live.mark("1", crate::session::TodoStatus::Done);
        emitted.push(EventKind::TodoUpdated {
            revision: live.revision,
            items: live.items.clone(),
        });

        // Replay: the last event's payload is the current list.
        let EventKind::TodoUpdated { revision: _, items } = emitted.last().unwrap() else {
            panic!("expected a TodoUpdated event");
        };
        let replayed = crate::session::TodoList {
            items: items.clone(),
            ..Default::default()
        };
        assert_eq!(replayed.items, live.items);
        assert_eq!(
            replayed.render(),
            "## todo (kernel)\n- [x] 1: write the test\n- [ ] 2: fix the bug",
            "replay renders the same byte-stable block the loop appended"
        );
        assert_eq!(
            emitted.len(),
            2,
            "one event per change, none per quiet turn"
        );
    }

    #[test]
    fn text_head_passes_short_text_through_verbatim() {
        assert_eq!(text_head("hello", 2_000), "hello");
        assert_eq!(text_head("", 1_000), "");
    }

    #[test]
    fn text_head_truncates_on_a_char_boundary_with_a_marker() {
        let long = "ab".repeat(3_000);
        let head = text_head(&long, 1_000);
        assert!(head.len() <= 1_000 + "[truncated]".len());
        assert!(head.ends_with("[truncated]"));
        assert!(head.starts_with("abab"));
    }

    #[test]
    fn text_head_never_splits_a_multibyte_char() {
        // Each 'あ' is three UTF-8 bytes; a byte budget that lands
        // mid-character must back off to the boundary.
        let wide = "あ".repeat(600);
        let head = text_head(&wide, 1_000);
        assert!(head.ends_with("[truncated]"));
        let kept = head.trim_end_matches("[truncated]");
        assert_eq!(kept.len() % 3, 0, "no partial character survives");
        assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
    }

    #[test]
    fn enriched_tool_events_and_loop_nudge_round_trip() {
        // Schema 11: tool events carry argument heads and result heads,
        // and loop-owned nudges serialize — the eval forensics the old
        // shape was blind to (2026-10-23 misdiagnosis).
        let call = ToolCallId::try_from("call_1".to_owned()).unwrap();
        let started = EventKind::ToolStarted {
            name: "run_command".into(),
            call_id: call.clone(),
            arguments: "{\"command\":[\"pytest\",\"-q\"]}".into(),
        };
        let completed = EventKind::ToolCompleted {
            name: "run_command".into(),
            call_id: call,
            status: ToolStatus::Success,
            content_head: "$ pytest -q\nexit status: 0".into(),
        };
        let nudge = EventKind::LoopNudge {
            text: "Hold on — these files changed".into(),
        };
        for kind in [started, completed, nudge] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: EventKind = serde_json::from_str(&json).unwrap();
            let round = serde_json::to_string(&back).unwrap();
            assert_eq!(json, round, "event {json} must round-trip");
        }
        let nudge_json = serde_json::to_string(&EventKind::LoopNudge { text: "x".into() }).unwrap();
        assert!(
            nudge_json.contains("\"loop_nudge\""),
            "kind tag is snake_case: {nudge_json}"
        );
    }
}
