//! A sink wrapper that turns event loss into task cancellation.

use super::sink::EventSink;
use super::wire::EventKind;
use crate::id::TaskId;
use tokio_util::sync::CancellationToken;
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
