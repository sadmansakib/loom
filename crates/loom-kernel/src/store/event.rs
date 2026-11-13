use std::sync::Arc;

use crate::event::EventEnvelope;
use crate::id::SessionId;

use super::StoreError;

/// Persists a session's observed event stream alongside its transcript.
///
/// The transcript stays the source of truth for session state (ADR-004);
/// the event stream is the observability record — prune reports, fast
/// paths, harness decisions, and the model/turn/tool timeline — that
/// `loom stats` attributes per mechanism (roadmap item 1's open tail:
/// "persist events alongside transcripts, so loom stats can attribute
/// savings per mechanism").
///
/// Same write cadence as the transcript seams: **append-on-emit**. The
/// transcript loop writes each item *before* the in-memory session
/// mirrors it; the persisting event sink
/// ([`crate::event::PersistingEventSink`]) writes each envelope as it
/// is emitted, so a crash loses at most the tail of a burst. Unlike the
/// transcript seams, delivery is best-effort (ADR-004): an event
/// persistence failure is counted and surfaced through
/// [`EventSink::dropped`](crate::event::EventSink::dropped), never
/// propagated as a task failure — observability must not alter task
/// semantics.
///
/// Rows are append-only and ordered by insertion (a monotonic
/// position), *not* by [`EventEnvelope::sequence`]: sequence numbers
/// are per sink, and a resumed session numbers its second run from
/// zero again. [`EventStore::load_events`] returns envelopes in write
/// order so consumers replay exactly what was persisted.
pub trait EventStore: Send + Sync {
    /// Appends one envelope to `session`'s event stream.
    ///
    /// The session must already exist ([`SessionStore::ensure_session`](super::SessionStore::ensure_session));
    /// durable implementations reject an unknown session the same way
    /// [`SessionStore::append_item`](super::SessionStore::append_item) does.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot append the
    /// envelope (including an unknown session).
    fn append_event(&self, session: &SessionId, envelope: &EventEnvelope)
    -> Result<(), StoreError>;

    /// Loads a session's event stream in write order; empty when the
    /// session never persisted events (every session predating this
    /// seam, and any run whose persistence was off).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot read the event
    /// stream.
    fn load_events(&self, session: &SessionId) -> Result<Vec<EventEnvelope>, StoreError>;
}

/// A shared, clone-safe handle to an [`EventStore`], the shape the
/// persisting event sink keeps.
pub type SharedEventStore = Arc<dyn EventStore>;
