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

use crate::id::TaskId;
use crate::model::{ModelErrorKind, ModelResponse};
use crate::transcript::ToolCallId;
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
pub const EVENT_SCHEMA_VERSION: u16 = 2;

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
    },
    /// The engine finished one tool call (success or failure — the
    /// structured outcome lands in the transcript's `ToolResult`; the
    /// next `ModelCompleted` shows what the model did with it).
    ToolCompleted {
        /// Name of the tool that was executed.
        name: String,
        /// Correlation id of the finished call.
        call_id: ToolCallId,
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
}

/// Best-effort sink for task events.
///
/// Implementations assign [`EventEnvelope::sequence`] (unique, strictly
/// increasing) and must never block or fail the task on delivery
/// problems. The canonical iteration-1 implementation is
/// [`ChannelEventSink`]; the CLI's NDJSON writer lives on the consumer
/// side of the channel.
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
