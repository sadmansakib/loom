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
use std::sync::{Mutex, MutexGuard};

use crate::id::{ArtifactId, SessionId};
use crate::session::{PlannedTurn, Session, TaskRecord};
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
}

/// In-memory [`SessionStore`] for tests and embedding without a database.
///
/// Applies the same validation the in-memory [`Session`] does, so a caller
/// cannot use it to write a transcript the kernel would reject.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<SessionId, Session>>,
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
}

/// In-memory [`ArtifactStore`] for tests.
#[derive(Default)]
pub struct MemoryArtifactStore {
    artifacts: Mutex<HashMap<ArtifactId, StoredArtifact>>,
}

impl MemoryArtifactStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<ArtifactId, StoredArtifact>>, StoreError> {
        self.artifacts
            .lock()
            .map_err(|_| StoreError::Backend("artifact store lock poisoned".to_owned()))
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn put(&self, artifact: &StoredArtifact) -> Result<(), StoreError> {
        self.lock()?.insert(artifact.id, artifact.clone());
        Ok(())
    }

    fn get(&self, id: &ArtifactId) -> Result<Option<StoredArtifact>, StoreError> {
        Ok(self.lock()?.get(id).cloned())
    }

    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError> {
        Ok(self
            .lock()?
            .values()
            .find(|artifact| artifact.sha256 == sha256)
            .cloned())
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
