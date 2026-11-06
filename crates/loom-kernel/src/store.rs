//! Durable state seams: `SessionStore` and `ArtifactStore` (ADR-008, ADR-012).
//!
//! The agent loop commits a turn in memory, but ADR-012 requires the turn to
//! be **durable before it is in memory** (persist-then-commit): a crash after
//! a mutating tool ran must never lose the record of that side effect. The
//! loop therefore plans a turn ([`Session::plan_turn`]), hands it to a
//! [`SessionStore`], and only then applies it in memory.
//!
//! These traits live in the neutral kernel; the concrete SQLite implementation
//! lives in `loom-store` so the loop keeps no database dependency. Errors are
//! surfaced as [`StoreError`] and mapped to [`TaskError::Persistence`](crate::agent::TaskError::Persistence)
//! by the loop — a store failure fails the task; an event-sink failure never
//! does (ADR-004, unchanged).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::event::EventEnvelope;
use crate::id::{ArtifactId, SessionId};
use crate::session::{PlannedTurn, Session, TaskRecord, TodoList};
use crate::transcript::{Item, ItemIndex};

/// A durable-storage failure.
///
/// Transport-thin: the kernel does not care *how* storage failed, only that
/// it did. The message is for the operator, not for control flow.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The storage backend failed (I/O, SQL, a poisoned lock).
    #[error("storage backend error: {0}")]
    Backend(String),
    /// A record read back did not match the shape the kernel writes
    /// (deserialisation failure, an index gap, an unknown session).
    #[error("corrupt stored state: {0}")]
    Corrupt(String),
    /// A live single-writer lease (ADR-008) still covers the session, so
    /// the caller must not race the writer that holds it.
    #[error("live lease on {session} (owner {owner}); refusing to race it")]
    Leased {
        /// The session the live lease covers.
        session: SessionId,
        /// The writer that holds the lease and must finish first.
        owner: String,
    },
}

/// Where a forked session came from: the source session and the first
/// transcript index the fork did **not** copy.
///
/// Recorded by [`SessionStore::fork`] and read back with
/// [`SessionStore::fork_of`], so fork lineage is store-backed rather
/// than inferred from ids or timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkProvenance {
    /// The session the fork copied from (still intact — a fork never
    /// truncates its source).
    pub forked_from: SessionId,
    /// The fork kept items `[0, boundary)`; `boundary` is the first
    /// dropped index.
    pub boundary: ItemIndex,
}

/// Persists a [`Session`]'s transcript and task records.
///
/// Implementations must make [`SessionStore::append_turn`] atomic: the
/// assistant item and all of its tool results become durable together, or not
/// at all. The kernel calls the methods here at exactly the seams the
/// in-memory [`Session`] mutates, so a store mirrors the session item for
/// item.
///
/// `Send + Sync` so a single store can sit behind `Box<dyn SessionStore>` on
/// an [`Agent`](crate::agent::Agent) shared across tasks.
pub trait SessionStore: Send + Sync {
    /// Records that a session exists, before its first item is appended.
    ///
    /// Idempotent: calling it again for a known session succeeds.
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError>;

    /// Appends one non-turn item (the task's user input) at `index`.
    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError>;

    /// Atomically appends a committed turn: the assistant item and one result
    /// per tool call.
    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError>;

    /// Inserts or updates a task record (start, progress, and terminal state).
    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError>;

    /// Loads a session by rebuilding it from stored items and task records.
    ///
    /// Returns `None` when the session is unknown.
    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError>;

    /// Lists known session ids, ordered by id (v7, i.e. creation order).
    fn session_ids(&self) -> Result<Vec<SessionId>, StoreError>;

    /// Forks `source` into `new_id`, copying transcript items
    /// `[0, boundary)`.
    ///
    /// The rewind surface's store half (`docs/rewind-design.md`): a fork
    /// never truncates the source — the transcript is append-only
    /// (ADR-004) and the original stays queryable. Copied items keep
    /// their indices, so references held against the source remain valid
    /// in the fork. Task records, todos, and events belong to the run
    /// that produced them and are not copied.
    ///
    /// Refuses as [`StoreError::Corrupt`] when `source` is unknown,
    /// `boundary` is past the transcript tip, or `new_id` already
    /// exists. Implementations with leases (ADR-008) refuse as
    /// [`StoreError::Leased`] while a live writer holds the source, so a
    /// fork can never race a moving tip.
    fn fork(
        &self,
        source: &SessionId,
        boundary: ItemIndex,
        new_id: &SessionId,
    ) -> Result<ForkProvenance, StoreError>;

    /// Returns the fork provenance recorded for `session`, if any.
    ///
    /// Listings use this to show lineage (`loom sessions`) without
    /// guessing from ids.
    fn fork_of(&self, session: &SessionId) -> Result<Option<ForkProvenance>, StoreError>;
}

/// A stored artifact body with its metadata (§ 2D consumes this; ADR-012).
///
/// The transcript carries a compact reference; the body lives here.
/// Content identity is [`StoredArtifact::sha256`]; [`StoredArtifact::id`] is
/// the stable row key a reference carries.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    /// The row key a transcript reference carries.
    pub id: ArtifactId,
    /// Artifact kind (e.g. a tool name), for routing and display.
    pub kind: String,
    /// Hex sha256 of `body`; the content identity.
    pub sha256: String,
    /// Length of `body` in bytes.
    pub bytes: u64,
    /// The full body.
    pub body: Vec<u8>,
}

impl StoredArtifact {
    /// Builds an artifact record, deriving `bytes` from the body length.
    pub fn new(
        id: ArtifactId,
        kind: impl Into<String>,
        sha256: impl Into<String>,
        body: Vec<u8>,
    ) -> Self {
        Self {
            id,
            kind: kind.into(),
            sha256: sha256.into(),
            bytes: body.len() as u64,
            body,
        }
    }
}

/// Metadata for one stored artifact body, without the body itself.
///
/// The shape [`ArtifactStore::list`] returns; the GC (2D lifecycle) and the
/// stats surface consume it, neither of which needs the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactStat {
    /// The row key a transcript reference carries.
    pub id: ArtifactId,
    /// Artifact kind (e.g. a tool name), for routing and display.
    pub kind: String,
    /// Length of the body in bytes.
    pub bytes: u64,
    /// First-write time, milliseconds since the Unix epoch. Storing the
    /// same id again replaces the body but never resets this.
    pub created_at: u64,
}

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
    /// The session must already exist ([`SessionStore::ensure_session`]);
    /// durable implementations reject an unknown session the same way
    /// [`SessionStore::append_item`] does.
    fn append_event(&self, session: &SessionId, envelope: &EventEnvelope)
    -> Result<(), StoreError>;

    /// Loads a session's event stream in write order; empty when the
    /// session never persisted events (every session predating this
    /// seam, and any run whose persistence was off).
    fn load_events(&self, session: &SessionId) -> Result<Vec<EventEnvelope>, StoreError>;
}

/// In-memory [`EventStore`] for tests and embedding without a database.
///
/// Mirrors the SQLite implementation's contract: rows keep insertion
/// order, and appending to an unknown session is [`StoreError::Corrupt`]
/// (the durable store enforces the same rule with a foreign key).
#[derive(Default)]
pub struct MemoryEventStore {
    events: Mutex<HashMap<SessionId, Vec<EventEnvelope>>>,
}

impl MemoryEventStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a session exists, before its first event is
    /// appended (the mirror of [`SessionStore::ensure_session`]).
    pub fn ensure_session(&self, session: &SessionId) {
        let Ok(mut guard) = self.events.lock() else {
            return; // poisoned: a silent skip, like the tool cache
        };
        guard.entry(*session).or_default();
    }

    /// Creates a store already accepting events for `session`.
    #[must_use]
    pub fn with_session(session: SessionId) -> Self {
        let store = Self::new();
        store.ensure_session(&session);
        store
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<SessionId, Vec<EventEnvelope>>>, StoreError> {
        self.events
            .lock()
            .map_err(|_| StoreError::Backend("event store lock poisoned".to_owned()))
    }
}

impl EventStore for MemoryEventStore {
    fn append_event(
        &self,
        session: &SessionId,
        envelope: &EventEnvelope,
    ) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        let rows = guard
            .get_mut(session)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown session {session}")))?;
        rows.push(envelope.clone());
        Ok(())
    }

    fn load_events(&self, session: &SessionId) -> Result<Vec<EventEnvelope>, StoreError> {
        Ok(self.lock()?.get(session).cloned().unwrap_or_default())
    }
}

/// A shared, clone-safe handle to an [`EventStore`], the shape the
/// persisting event sink keeps.
pub type SharedEventStore = Arc<dyn EventStore>;

/// Stores artifact bodies addressed by [`ArtifactId`] (ADR-012).
///
/// Kept separate from [`SessionStore`] because the two are written in the same
/// transaction by the SQLite implementation but read independently: the loop
/// only needs session append during a run, while `artifact_read` (2D) only
/// needs lookup.
pub trait ArtifactStore: Send + Sync {
    /// Stores a body. Storing the same id twice replaces the row.
    fn put(&self, artifact: &StoredArtifact) -> Result<(), StoreError>;

    /// Fetches a body by id.
    fn get(&self, id: &ArtifactId) -> Result<Option<StoredArtifact>, StoreError>;

    /// Finds a body by content hash, for deduplication.
    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError>;

    /// Lists every artifact's metadata, oldest first. Bodies are not
    /// included; this is the GC's and the stats surface's read.
    fn list(&self) -> Result<Vec<ArtifactStat>, StoreError>;

    /// Deletes a body. Only the GC calls this, and only for ids the
    /// reference scan proved unreferenced.
    fn delete(&self, id: &ArtifactId) -> Result<(), StoreError>;
}

/// In-memory [`SessionStore`] for tests and embedding without a database.
///
/// Applies the same validation the in-memory [`Session`] does, so a caller
/// cannot use it to write a transcript the kernel would reject.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<SessionId, Session>>,
    forks: Mutex<HashMap<SessionId, ForkProvenance>>,
}

impl MemorySessionStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a store already holding `session`.
    #[must_use]
    pub fn with_session(session: Session) -> Self {
        let mut sessions = HashMap::new();
        sessions.insert(session.id, session);
        Self {
            sessions: Mutex::new(sessions),
            forks: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<SessionId, Session>>, StoreError> {
        self.sessions
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))
    }

    fn session_mut<'a>(
        guard: &'a mut MutexGuard<'_, HashMap<SessionId, Session>>,
        session: &SessionId,
    ) -> Result<&'a mut Session, StoreError> {
        guard
            .get_mut(session)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown session {session}")))
    }
}

impl SessionStore for MemorySessionStore {
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError> {
        self.lock()?
            .entry(*session)
            .or_insert_with(|| Session::new(*session));
        Ok(())
    }

    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        let session = Self::session_mut(&mut guard, session)?;
        if session.next_index() != index {
            return Err(StoreError::Corrupt(format!(
                "item index {index} does not follow transcript tip {}",
                session.next_index()
            )));
        }
        session
            .push_item(item.clone())
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        Ok(())
    }

    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        let session = Self::session_mut(&mut guard, session)?;
        if session.next_index() != turn.assistant_at() {
            return Err(StoreError::Corrupt(format!(
                "turn planned for {} but the transcript tip is {}",
                turn.assistant_at(),
                session.next_index()
            )));
        }
        session
            .append_turn(turn.assistant().clone(), turn.results().to_vec())
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        Ok(())
    }

    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        let session = Self::session_mut(&mut guard, session)?;
        match session.tasks.iter_mut().find(|task| task.id == record.id) {
            Some(slot) => *slot = record.clone(),
            None => session.push_task(record.clone()),
        }
        Ok(())
    }

    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError> {
        Ok(self.lock()?.get(session).cloned())
    }

    fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut ids: Vec<SessionId> = self.lock()?.keys().copied().collect();
        ids.sort();
        Ok(ids)
    }

    fn fork(
        &self,
        source: &SessionId,
        boundary: ItemIndex,
        new_id: &SessionId,
    ) -> Result<ForkProvenance, StoreError> {
        let mut guard = self.lock()?;
        let session = guard
            .get(source)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown session {source}")))?;
        let boundary = boundary.as_u32() as usize;
        if boundary > session.transcript.len() {
            return Err(StoreError::Corrupt(format!(
                "boundary {boundary} is past the transcript tip {}",
                session.transcript.len()
            )));
        }
        if guard.contains_key(new_id) {
            return Err(StoreError::Corrupt(format!(
                "session {new_id} already exists"
            )));
        }
        // Indices are positions, so a prefix copy preserves them; task
        // records and todos are run state, not conversation state, and
        // stay with the source (the fork starts a fresh task on
        // continue, exactly like a resumed session rebuild).
        let forked = Session {
            id: *new_id,
            transcript: session.transcript[..boundary].to_vec(),
            tasks: Vec::new(),
            todos: TodoList::default(),
        };
        guard.insert(*new_id, forked);
        let provenance = ForkProvenance {
            forked_from: *source,
            boundary: ItemIndex::from(boundary as u32),
        };
        self.forks
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))?
            .insert(*new_id, provenance);
        Ok(provenance)
    }

    fn fork_of(&self, session: &SessionId) -> Result<Option<ForkProvenance>, StoreError> {
        self.forks
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))
            .map(|forks| forks.get(session).copied())
    }
}

/// In-memory [`ArtifactStore`] for tests.
#[derive(Default)]
pub struct MemoryArtifactStore {
    artifacts: Mutex<MemoryArtifactMap>,
}

/// The memory store's rows: body plus first-write time.
type MemoryArtifactMap = HashMap<ArtifactId, (StoredArtifact, u64)>;

impl MemoryArtifactStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, MemoryArtifactMap>, StoreError> {
        self.artifacts
            .lock()
            .map_err(|_| StoreError::Backend("artifact store lock poisoned".to_owned()))
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn put(&self, artifact: &StoredArtifact) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        // An id-storing put replaces the body but keeps the first-write
        // time, mirroring the SQLite upsert's created_at behavior.
        let created_at = guard
            .get(&artifact.id)
            .map_or_else(crate::artifact_gc::now_millis, |(_, at)| *at);
        guard.insert(artifact.id, (artifact.clone(), created_at));
        Ok(())
    }

    fn get(&self, id: &ArtifactId) -> Result<Option<StoredArtifact>, StoreError> {
        Ok(self.lock()?.get(id).map(|(artifact, _)| artifact.clone()))
    }

    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError> {
        Ok(self
            .lock()?
            .values()
            .find(|(artifact, _)| artifact.sha256 == sha256)
            .map(|(artifact, _)| artifact.clone()))
    }

    fn list(&self) -> Result<Vec<ArtifactStat>, StoreError> {
        let mut stats: Vec<ArtifactStat> = self
            .lock()?
            .iter()
            .map(|(id, (artifact, created_at))| ArtifactStat {
                id: *id,
                kind: artifact.kind.clone(),
                bytes: artifact.bytes,
                created_at: *created_at,
            })
            .collect();
        stats.sort_by_key(|stat| (stat.created_at, stat.id));
        Ok(stats)
    }

    fn delete(&self, id: &ArtifactId) -> Result<(), StoreError> {
        self.lock()?.remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::TaskId;
    use crate::model::StopReason;
    use crate::session::{TaskStatus, TerminalTaskStatus};
    use crate::transcript::{
        AssistantMessage, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
    };

    fn call_id(id: &str) -> ToolCallId {
        ToolCallId::try_from(id.to_owned()).unwrap()
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: call_id(id),
            name: name.to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({})).unwrap(),
        }
    }

    fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
        AssistantMessage {
            content: text.to_owned(),
            reasoning: None,
            stop_reason: if calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolCalls
            },
            tool_calls: calls,
        }
    }

    fn result(id: &str) -> ToolResult {
        ToolResult::new(call_id(id), ToolStatus::Success, "contents", false)
    }

    #[test]
    fn append_then_load_round_trips_a_session() {
        let id = SessionId::new();
        let store = MemorySessionStore::new();
        store.ensure_session(&id).unwrap();

        // The in-memory session is the reference; the store must mirror it
        // item for item at the same seams the loop uses.
        let mut session = Session::new(id);

        let user = Item::User { text: "hi".into() };
        let index = session.push_item(user.clone()).unwrap();
        store.append_item(&id, index, &user).unwrap();

        let planned = session
            .plan_turn(
                assistant("working", vec![call("call_1", "read_file")]),
                vec![result("call_1")],
            )
            .unwrap();
        store.append_turn(&id, &planned).unwrap();
        session.apply_planned_turn(planned);

        let record = TaskRecord::new(TaskId::new(), 1_000);
        store.upsert_task(&id, &record).unwrap();
        let mut record = record;
        record.finish(TerminalTaskStatus::Completed, 2_000, "end_turn".to_owned());
        store.upsert_task(&id, &record).unwrap();

        let loaded = store.load(&id).unwrap().expect("session");
        assert_eq!(loaded.transcript, session.transcript);
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].status, TaskStatus::Completed);
        assert_eq!(loaded.tasks[0].end_reason.as_deref(), Some("end_turn"));
        assert_eq!(loaded.tasks[0].completed_at_millis, Some(2_000));
        assert_eq!(store.session_ids().unwrap(), vec![id]);
    }

    #[test]
    fn append_item_rejects_a_gapped_index() {
        let store = MemorySessionStore::new();
        let id = SessionId::new();
        store.ensure_session(&id).unwrap();
        let error = store
            .append_item(&id, ItemIndex::from(5), &Item::User { text: "x".into() })
            .unwrap_err();
        assert!(matches!(error, StoreError::Corrupt(_)));
    }

    #[test]
    fn appending_to_an_unknown_session_is_corrupt() {
        let store = MemorySessionStore::new();
        let id = SessionId::new();
        let error = store
            .append_item(&id, ItemIndex::from(0), &Item::User { text: "x".into() })
            .unwrap_err();
        assert!(matches!(error, StoreError::Corrupt(_)));
    }

    #[test]
    fn loading_an_unknown_session_is_none() {
        let store = MemorySessionStore::new();
        assert!(store.load(&SessionId::new()).unwrap().is_none());
    }

    /// A seeded three-item session: user input then one committed turn.
    fn seeded_session() -> (SessionId, Session) {
        let id = SessionId::new();
        let mut session = Session::new(id);
        let user = Item::User { text: "fix".into() };
        let index = session.push_item(user.clone()).unwrap();
        debug_assert_eq!(index, ItemIndex::from(0));

        let planned = session
            .plan_turn(
                assistant("working", vec![call("call_1", "read_file")]),
                vec![result("call_1")],
            )
            .unwrap();
        session.apply_planned_turn(planned);
        (id, session)
    }

    #[test]
    fn fork_copies_a_prefix_preserves_indices_and_records_lineage() {
        let (id, session) = seeded_session();
        let store = MemorySessionStore::with_session(session);
        let fork_id = SessionId::new();

        // Cut before the assistant turn: keep the user item only.
        let provenance = store.fork(&id, ItemIndex::from(1), &fork_id).unwrap();
        assert_eq!(provenance.forked_from, id);
        assert_eq!(provenance.boundary, ItemIndex::from(1));

        let forked = store.load(&fork_id).unwrap().expect("forked session");
        assert_eq!(forked.transcript.len(), 1);
        // Indices are positions, so the copy keeps them: item 0 is the
        // user item, not a renumbered one.
        assert_eq!(forked.transcript[0], Item::User { text: "fix".into() });
        // Run state stays with the source; the fork is conversation only.
        assert!(forked.tasks.is_empty());
        assert!(forked.todos.is_empty());

        // The source is intact — a fork never truncates.
        let source = store.load(&id).unwrap().expect("source session");
        assert_eq!(source.transcript.len(), 3);
        assert_eq!(store.session_ids().unwrap().len(), 2);

        // Lineage is store-backed and queryable both ways.
        assert_eq!(store.fork_of(&fork_id).unwrap(), Some(provenance));
        assert_eq!(store.fork_of(&id).unwrap(), None);
    }

    #[test]
    fn fork_refuses_unknown_source_past_tip_and_duplicate_id() {
        let (id, session) = seeded_session();
        let store = MemorySessionStore::with_session(session);

        let stranger = SessionId::new();
        assert!(matches!(
            store.fork(&stranger, ItemIndex::from(0), &SessionId::new()),
            Err(StoreError::Corrupt(_))
        ));

        // The tip is 3; boundary 4 is past it.
        let past_tip = SessionId::new();
        let error = store.fork(&id, ItemIndex::from(4), &past_tip).unwrap_err();
        assert!(
            error.to_string().contains("past the transcript tip"),
            "wrong error: {error}"
        );

        // A fork id that already exists would collide with live data.
        let duplicate = SessionId::new();
        store.ensure_session(&duplicate).unwrap();
        assert!(matches!(
            store.fork(&id, ItemIndex::from(0), &duplicate),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn memory_event_store_round_trips_in_write_order() {
        use crate::event::{EventEnvelope, EventKind};
        use crate::id::TaskId;

        let id = SessionId::new();
        let store = MemoryEventStore::with_session(id);
        assert!(
            store.load_events(&id).unwrap().is_empty(),
            "a session without events is empty, not an error"
        );

        let task = TaskId::new();
        let envelope = |sequence, kind| EventEnvelope {
            schema_version: crate::event::EVENT_SCHEMA_VERSION,
            sequence,
            task_id: task,
            kind,
        };
        // Two runs share the session: sequences restart, write order wins.
        store
            .append_event(&id, &envelope(0, EventKind::TaskStarted))
            .unwrap();
        store
            .append_event(&id, &envelope(0, EventKind::TaskStarted))
            .unwrap();
        store
            .append_event(&id, &envelope(1, EventKind::TaskCompleted))
            .unwrap();

        let loaded = store.load_events(&id).unwrap();
        assert_eq!(loaded.len(), 3);
        let sequences: Vec<u64> = loaded.iter().map(|event| event.sequence).collect();
        assert_eq!(
            sequences,
            vec![0, 0, 1],
            "insertion order, not sequence order"
        );
        assert_eq!(loaded[0].task_id, task);
        assert_eq!(loaded[2].kind, EventKind::TaskCompleted);
    }

    #[test]
    fn memory_event_store_rejects_an_unknown_session() {
        use crate::event::{EventEnvelope, EventKind};
        use crate::id::TaskId;

        let store = MemoryEventStore::new();
        let error = store
            .append_event(
                &SessionId::new(),
                &EventEnvelope {
                    schema_version: crate::event::EVENT_SCHEMA_VERSION,
                    sequence: 0,
                    task_id: TaskId::new(),
                    kind: EventKind::TaskStarted,
                },
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Corrupt(_)));
    }

    #[test]
    fn artifact_store_round_trips_and_finds_by_hash() {
        let store = MemoryArtifactStore::new();
        let id = ArtifactId::new();
        let artifact = StoredArtifact::new(id, "read_file", "abc123", b"hello".to_vec());
        assert_eq!(artifact.bytes, 5);
        store.put(&artifact).unwrap();

        assert_eq!(store.get(&id).unwrap(), Some(artifact.clone()));
        assert_eq!(store.find_by_sha256("abc123").unwrap(), Some(artifact));
        assert_eq!(store.find_by_sha256("missing").unwrap(), None);
        assert_eq!(store.get(&ArtifactId::new()).unwrap(), None);
    }
}
