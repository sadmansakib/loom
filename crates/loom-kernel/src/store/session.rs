use crate::id::SessionId;
use crate::session::{PlannedTurn, Session, TaskRecord};
use crate::transcript::{Item, ItemIndex};

use super::StoreError;

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

/// Where a session's workspace stood when the run started — the tier-1
/// restore point of the rewind surface (`docs/rewind-design.md` §4).
///
/// Recorded by the frontend at `loom run` start, before the task can
/// touch the tree. `head` is the exact restore target when the workspace
/// was a clean git checkout; otherwise it is `None` and `reason` says
/// why no tree restore is provable (dirty start, not a git repo) — the
/// tier-0 refusal the rewind surface reports rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCheckpoint {
    /// The workspace the run started in (canonicalized by the caller).
    pub workspace: std::path::PathBuf,
    /// HEAD at run start when the tree was clean; the commit
    /// `--restore-workspace --to-start` resets to.
    pub head: Option<String>,
    /// Why [`StartCheckpoint::head`] is absent, for the refusal message.
    pub reason: Option<String>,
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
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the storage backend fails to record
    /// the session row.
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError>;

    /// Appends one non-turn item (the task's user input) at `index`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot persist the item.
    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError>;

    /// Atomically appends a committed turn: the assistant item and one result
    /// per tool call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot persist the turn
    /// atomically.
    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError>;

    /// Inserts or updates a task record (start, progress, and terminal state).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot persist the record.
    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError>;

    /// Loads a session by rebuilding it from stored items and task records.
    ///
    /// Returns `None` when the session is unknown.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend fails or the stored
    /// history cannot be rebuilt (a corrupt transcript).
    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError>;

    /// Lists known session ids, ordered by id (v7, i.e. creation order).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot enumerate the
    /// sessions.
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
    ///
    /// The source's [`StartCheckpoint`], if any, travels with the fork:
    /// it describes the workspace the session lineage started in, which
    /// is the tier-1 restore point for the fork as well.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] when `source` is unknown,
    /// `boundary` is past the transcript tip, or `new_id` already
    /// exists; [`StoreError::Leased`] when a live writer holds the
    /// source; [`StoreError::Backend`] on persistence failure.
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
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot read the fork
    /// provenance.
    fn fork_of(&self, session: &SessionId) -> Result<Option<ForkProvenance>, StoreError>;

    /// Records the workspace state at the start of `session`'s first
    /// run (`docs/rewind-design.md` §4, tier 1).
    ///
    /// Called by the frontend before the task runs, so the checkpoint
    /// describes a tree the task has not touched yet. Idempotent: a
    /// re-record replaces the row (the honest baseline is whichever run
    /// actually started the session).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot persist the
    /// checkpoint.
    fn record_start_checkpoint(
        &self,
        session: &SessionId,
        checkpoint: &StartCheckpoint,
    ) -> Result<(), StoreError>;

    /// Returns the recorded start checkpoint for `session`, if any.
    ///
    /// `None` means the session predates the checkpoint feature or was
    /// never run through a frontend that records one — a tier-0
    /// refusal for `--restore-workspace`, never a guess.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot read the
    /// checkpoint.
    fn start_checkpoint(&self, session: &SessionId) -> Result<Option<StartCheckpoint>, StoreError>;

    /// Acquires the single-writer lease over `session` (ADR-008),
    /// before the caller persists anything to it.
    ///
    /// Succeeds when the session is unleased or the previous lease
    /// expired, returning the held [`Lease`]. Returns `Ok(None)` when
    /// another live writer holds it; the caller must then leave the
    /// session alone rather than race it. `ttl_millis` bounds how long
    /// the lease survives a crashed writer.
    ///
    /// The default grants trivially: stores without cross-process
    /// visibility (in-memory) cannot be raced by a second process and
    /// need no lock.
    ///
    /// # Errors
    ///
    /// Never returns `Err` in the default implementation: it grants
    /// unconditionally.
    fn acquire_lease(
        &self,
        session: &SessionId,
        owner: &str,
        ttl_millis: u64,
    ) -> Result<Option<Lease>, StoreError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| {
                // Millis since the epoch fits `u64` for any real clock
                // (the limit is ~year 584 million); truncation is
                // unreachable.
                #[allow(clippy::cast_possible_truncation)]
                (elapsed.as_millis() as u64)
            })
            .unwrap_or_default();
        let expires_at_millis = now.saturating_add(ttl_millis);
        Ok(Some(Lease::new(
            *session,
            owner.to_owned(),
            expires_at_millis,
        )))
    }

    /// Extends a held lease by `ttl_millis`. Returns `false` when
    /// `owner` no longer holds it (expired and stolen, or released).
    ///
    /// The default reports success: nothing can steal a trivially
    /// granted lease.
    ///
    /// # Errors
    ///
    /// Never returns `Err` in the default implementation.
    fn renew_lease(
        &self,
        _session: &SessionId,
        _owner: &str,
        _ttl_millis: u64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    /// Releases a held lease. Returns `false` when `owner` did not
    /// hold it.
    ///
    /// The default reports success, matching the trivial grant.
    ///
    /// # Errors
    ///
    /// Never returns `Err` in the default implementation.
    fn release_lease(&self, _session: &SessionId, _owner: &str) -> Result<bool, StoreError> {
        Ok(true)
    }

    /// The live lease holder for `session`, if any:
    /// `(owner, expires_at_millis)`.
    ///
    /// Diagnostic only — the truth is whatever [`SessionStore::acquire_lease`]
    /// returns; this just fills the refusal message. The default cannot
    /// say (`None`).
    ///
    /// # Errors
    ///
    /// Never returns `Err` in the default implementation.
    fn lease_holder(&self, _session: &SessionId) -> Result<Option<(String, u64)>, StoreError> {
        Ok(None)
    }
}

/// A held single-writer lease over one session (ADR-008).
///
/// The task loop acquires one before its first persist of a run and
/// drops it when the run ends, so a second process driving the same
/// session fails fast instead of interleaving writes. The TTL bounds
/// how long a crashed writer's lease blocks resumption; renewal keeps
/// long tasks covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    session: SessionId,
    owner: String,
    expires_at_millis: u64,
}

impl Lease {
    /// A lease held by `owner` over `session` until `expires_at_millis`.
    #[must_use]
    pub fn new(session: SessionId, owner: String, expires_at_millis: u64) -> Self {
        Self {
            session,
            owner,
            expires_at_millis,
        }
    }

    /// The session this lease guards.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The owner that acquired it.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// When the lease lapses if not renewed (Unix millis).
    #[must_use]
    pub fn expires_at_millis(&self) -> u64 {
        self.expires_at_millis
    }
}
