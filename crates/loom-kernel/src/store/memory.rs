use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::event::EventEnvelope;
use crate::id::{ArtifactId, SessionId};
use crate::session::{PlannedTurn, Session, TaskRecord, TodoList};
use crate::transcript::{Item, ItemIndex};

use super::{
    ArtifactStat, ArtifactStore, EventStore, ForkProvenance, SessionStore, StartCheckpoint,
    StoreError, StoredArtifact, verify_body_hash,
};

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

/// In-memory [`SessionStore`] for tests and embedding without a database.
///
/// Applies the same validation the in-memory [`Session`] does, so a caller
/// cannot use it to write a transcript the kernel would reject.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<SessionId, Session>>,
    forks: Mutex<HashMap<SessionId, ForkProvenance>>,
    checkpoints: Mutex<HashMap<SessionId, StartCheckpoint>>,
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
            checkpoints: Mutex::new(HashMap::new()),
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
        // `boundary` is a transcript length — far below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let boundary = boundary as u32;
        let provenance = ForkProvenance {
            forked_from: *source,
            boundary: ItemIndex::from(boundary),
        };
        self.forks
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))?
            .insert(*new_id, provenance);
        // The start checkpoint travels with the fork: it describes the
        // workspace the lineage started in, which is the fork's tier-1
        // restore point too. Bind the clone BEFORE re-locking: an `if
        // let` scrutinee guard lives for the whole body, and locking
        // the same mutex twice would deadlock.
        let checkpoint = self
            .checkpoints
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))?
            .get(source)
            .cloned();
        if let Some(checkpoint) = checkpoint {
            self.checkpoints
                .lock()
                .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))?
                .insert(*new_id, checkpoint);
        }
        Ok(provenance)
    }

    fn fork_of(&self, session: &SessionId) -> Result<Option<ForkProvenance>, StoreError> {
        self.forks
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))
            .map(|forks| forks.get(session).copied())
    }

    fn record_start_checkpoint(
        &self,
        session: &SessionId,
        checkpoint: &StartCheckpoint,
    ) -> Result<(), StoreError> {
        self.checkpoints
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))?
            .insert(*session, checkpoint.clone());
        Ok(())
    }

    fn start_checkpoint(&self, session: &SessionId) -> Result<Option<StartCheckpoint>, StoreError> {
        self.checkpoints
            .lock()
            .map_err(|_| StoreError::Backend("session store lock poisoned".to_owned()))
            .map(|checkpoints| checkpoints.get(session).cloned())
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
        let guard = self.lock()?;
        let Some((artifact, _)) = guard.get(id) else {
            return Ok(None);
        };
        verify_body_hash(id, &artifact.sha256, &artifact.body)?;
        Ok(Some(artifact.clone()))
    }

    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError> {
        let guard = self.lock()?;
        let Some((artifact, _)) = guard
            .values()
            .find(|(artifact, _)| artifact.sha256 == sha256)
        else {
            return Ok(None);
        };
        verify_body_hash(&artifact.id, &artifact.sha256, &artifact.body)?;
        Ok(Some(artifact.clone()))
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
