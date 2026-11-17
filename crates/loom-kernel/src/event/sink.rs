//! Event sinks: the best-effort delivery contract, the channel sink
//! that numbers and forwards envelopes, and the fan-out combinator.

use super::wire::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, now_millis};
use crate::id::TaskId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;
/// Best-effort sink for task events.
///
/// Implementations assign [`EventEnvelope::sequence`] (unique, strictly
/// increasing) and [`EventEnvelope::timestamp_millis`] (observation time),
/// and must never block or fail the task on delivery problems. The canonical iteration-1 implementation is
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
    #[must_use]
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
            timestamp_millis: Some(now_millis()),
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
    #[must_use]
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    /// Adds an owned child sink, chainable:
    /// `FanoutSink::new().with(renderer_sink).with(trace_sink)`.
    #[must_use]
    pub fn with(mut self, sink: impl EventSink + 'static) -> Self {
        self.sinks.push(Arc::new(sink));
        self
    }

    /// Adds a shared child sink, for callers that keep their own
    /// handle (e.g. to poll one consumer's drop count while the task
    /// runs).
    #[must_use]
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
