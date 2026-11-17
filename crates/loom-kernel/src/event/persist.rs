//! A sink wrapper that persists each event to the shared event store
//! before forwarding it.

use super::sink::EventSink;
use super::wire::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, now_millis};
use crate::id::{SessionId, TaskId};
use crate::store::SharedEventStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// Two kinds are exempt (review S4): [`EventKind::TextDelta`] and
/// [`EventKind::ReasoningDelta`] forward live but are never written —
/// the durable record holds the same content through
/// [`EventKind::ModelCompleted`] and the transcript, and one INSERT
/// per provider chunk dominated store write volume.
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
    #[must_use]
    pub fn persist_failures(&self) -> u64 {
        self.state.persist_failures.load(Ordering::Relaxed)
    }
}

impl EventSink for PersistingEventSink<'_> {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        // Per-token deltas stream live but are never persisted
        // (review S4): they were ~65% of all stored rows at one
        // synchronous INSERT per provider chunk, and the durable
        // record already holds the same content authoritatively —
        // `ModelCompleted` carries the final message and the
        // transcript stores it. Forward without numbering or storing
        // so the persisted stream stays contiguous from zero.
        if matches!(
            kind,
            EventKind::TextDelta { .. } | EventKind::ReasoningDelta { .. }
        ) {
            self.inner.emit(task_id, kind);
            return;
        }
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
            timestamp_millis: Some(now_millis()),
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
