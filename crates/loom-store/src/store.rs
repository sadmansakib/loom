//! The SQLite-backed implementations of the kernel store traits.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_kernel::event::EventEnvelope;
use loom_kernel::id::{ArtifactId, SessionId};
use loom_kernel::session::{PlannedTurn, Session, TaskRecord, TodoList};
use loom_kernel::store::{
    ArtifactStat, ArtifactStore, EventStore, ForkProvenance, SessionStore, StartCheckpoint,
    StoreError, StoredArtifact, verify_body_hash,
};
use loom_kernel::transcript::{Item, ItemIndex};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{self, backend};

/// How long SQLite waits on a locked database before failing.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// A durable store over one SQLite database file.
///
/// Holds a single connection behind a mutex: writes serialize in-process, and
/// WAL lets other processes (the credential store) read and write the same
/// file concurrently. Implements both [`SessionStore`] and [`ArtifactStore`]
/// over that one connection, since a turn and the artifacts it references are
/// written together (ADR-012).
pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path`, applying pragmas and
    /// migrations.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the parent directory cannot be
    /// created, SQLite cannot open the file, a `PRAGMA` is rejected, or a
    /// schema migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(backend)?;
        }
        let connection = Connection::open(path).map_err(backend)?;
        Self::from_connection(connection)
    }

    /// Opens an anonymous in-memory database (tests).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when SQLite cannot create the
    /// in-memory connection or a schema migration fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory().map_err(backend)?)
    }

    /// The default database path: `$LOOM_DB` when set, else
    /// `$XDG_DATA_HOME/loom/loom.db`, else `$HOME/.local/share/loom/loom.db`.
    ///
    /// Never inside the workspace (ADR-008): a database in the repo would be
    /// committed, cleaned, or shadowed by a worktree.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when neither `XDG_DATA_HOME` nor
    /// `HOME` is set, so no default location can be derived.
    pub fn default_path() -> Result<PathBuf, StoreError> {
        if let Some(explicit) = std::env::var_os("LOOM_DB")
            && !explicit.is_empty()
        {
            return Ok(PathBuf::from(explicit));
        }
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let home = std::env::var_os("HOME").ok_or_else(|| {
                    StoreError::Backend(
                        "neither XDG_DATA_HOME nor HOME is set; set LOOM_DB".to_owned(),
                    )
                })?;
                PathBuf::from(home).join(".local").join("share")
            }
        };
        Ok(base.join("loom").join("loom.db"))
    }

    /// Opens the database at [`SqliteStore::default_path`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`SqliteStore::default_path`] and
    /// [`SqliteStore::open`] (missing environment, unusable file,
    /// rejected `PRAGMA`, failed migration).
    pub fn open_default() -> Result<Self, StoreError> {
        Self::open(Self::default_path()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(BUSY_TIMEOUT).map_err(backend)?;
        // `journal_mode` reports the mode it settled on as a row; WAL is a
        // no-op that reports "memory" for an in-memory database.
        let _: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(backend)?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(backend)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(backend)?;
        schema::migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::Backend("store lock poisoned".to_owned()))
    }

    /// Acquires the single-writer lease for `session` (ADR-008).
    ///
    /// Succeeds when the session is unleased, already leased by `owner`, or
    /// the previous lease expired. Returns `None` when another owner holds a
    /// live lease; the caller must then leave that session alone rather than
    /// race it. `ttl_millis` bounds how long the lease survives a crash.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] when `session` names no stored
    /// session, [`StoreError::Backend`] when the store lock is poisoned or
    /// SQLite rejects the write/read, and surfaces any lease-row read
    /// failure while distinguishing those cases.
    pub fn acquire_lease(
        &self,
        session: &SessionId,
        owner: impl Into<String>,
        ttl_millis: u64,
    ) -> Result<Option<Lease>, StoreError> {
        let owner = owner.into();
        let now = now_millis();
        let expires = now.saturating_add(ttl_millis);
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET lease_owner = ?1, lease_until = ?2 \
                 WHERE id = ?3 \
                   AND (lease_owner IS NULL OR lease_owner = ?1 OR lease_until <= ?4)",
                params![
                    owner,
                    store_u64(expires),
                    session.to_string(),
                    store_u64(now)
                ],
            )
            .map_err(backend)?;
        if changed == 0 {
            // Either another owner holds the lease, or the session row does
            // not exist. Distinguish so the caller is not misled.
            let exists: Option<i64> = connection
                .query_row(
                    "SELECT 1 FROM sessions WHERE id = ?1",
                    [session.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)?;
            if exists.is_none() {
                return Err(StoreError::Corrupt(format!("unknown session {session}")));
            }
            return Ok(None);
        }
        Ok(Some(Lease::new(*session, owner, expires)))
    }

    /// Extends a held lease. Returns `false` if `owner` no longer holds it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the store lock is poisoned or
    /// SQLite rejects the update.
    pub fn renew_lease(
        &self,
        session: &SessionId,
        owner: &str,
        ttl_millis: u64,
    ) -> Result<bool, StoreError> {
        let expires = now_millis().saturating_add(ttl_millis);
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET lease_until = ?1 WHERE id = ?2 AND lease_owner = ?3",
                params![store_u64(expires), session.to_string(), owner],
            )
            .map_err(backend)?;
        Ok(changed > 0)
    }

    /// Releases a held lease. Returns `false` if `owner` did not hold it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the store lock is poisoned or
    /// SQLite rejects the update.
    pub fn release_lease(&self, session: &SessionId, owner: &str) -> Result<bool, StoreError> {
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET lease_owner = NULL, lease_until = NULL \
                 WHERE id = ?1 AND lease_owner = ?2",
                params![session.to_string(), owner],
            )
            .map_err(backend)?;
        Ok(changed > 0)
    }
}

/// A held single-writer lease over one session (ADR-008).
///
/// Re-exported from the kernel: the trait's default implementations
/// and the SQLite implementation share one shape.
pub use loom_kernel::store::Lease;

impl SessionStore for SqliteStore {
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR IGNORE INTO sessions (id, created_at, lease_owner, lease_until) \
                 VALUES (?1, ?2, NULL, NULL)",
                params![session.to_string(), store_u64(now_millis())],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        insert_item(&connection, session, index, item)
    }

    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(backend)?;
        for (index, item) in turn.items() {
            insert_item(&transaction, session, index, &item)?;
        }
        transaction.commit().map_err(backend)?;
        Ok(())
    }

    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError> {
        let status = serde_json::to_value(record.status).map_err(backend)?;
        let status = status.as_str().unwrap_or_default().to_owned();
        let body = serde_json::to_string(record).map_err(backend)?;
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO tasks (session_id, id, status, body_json) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(session_id, id) DO UPDATE SET \
                     status = excluded.status, body_json = excluded.body_json",
                params![session.to_string(), record.id.to_string(), status, body],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn fork(
        &self,
        source: &SessionId,
        boundary: ItemIndex,
        new_id: &SessionId,
    ) -> Result<ForkProvenance, StoreError> {
        let now = now_millis();
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(backend)?;

        // One read for the source row: existence and lease state. A live
        // lease means a writer may still be moving the transcript tip, so
        // the fork refuses rather than race it (ADR-008's discipline).
        let lease = transaction
            .query_row(
                "SELECT lease_owner, lease_until FROM sessions WHERE id = ?1",
                [source.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        let Some((lease_owner, lease_until)) = lease else {
            return Err(StoreError::Corrupt(format!("unknown session {source}")));
        };
        if lease_until.is_some_and(|until| until > store_u64(now)) {
            return Err(StoreError::Leased {
                session: *source,
                owner: lease_owner.unwrap_or_else(|| "?".to_owned()),
            });
        }

        // Indices are contiguous from zero, so the tip is one past the
        // highest stored index (0 for an empty transcript).
        let tip: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(idx) + 1, 0) FROM items WHERE session_id = ?1",
                [source.to_string()],
                |row| row.get(0),
            )
            .map_err(backend)?;
        let boundary_index = i64::from(boundary.as_u32());
        if boundary_index > tip {
            return Err(StoreError::Corrupt(format!(
                "boundary {boundary_index} is past the transcript tip {tip}"
            )));
        }
        let exists: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                [new_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        if exists.is_some() {
            return Err(StoreError::Corrupt(format!(
                "session {new_id} already exists"
            )));
        }

        transaction
            .execute(
                "INSERT INTO sessions (id, created_at, lease_owner, lease_until) \
                 VALUES (?1, ?2, NULL, NULL)",
                params![new_id.to_string(), store_u64(now)],
            )
            .map_err(backend)?;
        // Copies keep their `idx` values: references made against the
        // source stay valid in the fork (ADR-004's stability rule).
        transaction
            .execute(
                "INSERT INTO items (session_id, idx, kind, body_json) \
                 SELECT ?1, idx, kind, body_json FROM items \
                  WHERE session_id = ?2 AND idx < ?3",
                params![new_id.to_string(), source.to_string(), boundary_index],
            )
            .map_err(backend)?;
        transaction
            .execute(
                "INSERT INTO forks (session_id, forked_from, boundary, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    new_id.to_string(),
                    source.to_string(),
                    boundary_index,
                    store_u64(now)
                ],
            )
            .map_err(backend)?;
        // The start checkpoint travels with the fork (tier-1 restore
        // point of the lineage); same transaction, so a fork either
        // carries its baseline or nothing at all.
        transaction
            .execute(
                "INSERT INTO start_checkpoints \
                    (session_id, workspace, head, reason, created_at) \
                 SELECT ?1, workspace, head, reason, created_at \
                   FROM start_checkpoints WHERE session_id = ?2",
                params![new_id.to_string(), source.to_string()],
            )
            .map_err(backend)?;
        transaction.commit().map_err(backend)?;

        Ok(ForkProvenance {
            forked_from: *source,
            boundary,
        })
    }

    fn fork_of(&self, session: &SessionId) -> Result<Option<ForkProvenance>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT forked_from, boundary FROM forks WHERE session_id = ?1",
                [session.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(backend)?;
        let Some((from, boundary)) = row else {
            return Ok(None);
        };
        let forked_from: SessionId = from.parse().map_err(|error| {
            StoreError::Corrupt(format!(
                "fork row for {session} has a bad source id {from:?}: {error}"
            ))
        })?;
        let boundary = u32::try_from(boundary).map_err(|_| {
            StoreError::Corrupt(format!(
                "fork row for {session} has a bad boundary {boundary}"
            ))
        })?;
        Ok(Some(ForkProvenance {
            forked_from,
            boundary: ItemIndex::from(boundary),
        }))
    }

    fn record_start_checkpoint(
        &self,
        session: &SessionId,
        checkpoint: &StartCheckpoint,
    ) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO start_checkpoints (session_id, workspace, head, reason, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(session_id) DO UPDATE SET \
                    workspace = excluded.workspace, head = excluded.head, \
                    reason = excluded.reason, created_at = excluded.created_at",
                params![
                    session.to_string(),
                    checkpoint.workspace.to_string_lossy(),
                    checkpoint.head,
                    checkpoint.reason,
                    store_u64(now_millis())
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn start_checkpoint(&self, session: &SessionId) -> Result<Option<StartCheckpoint>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT workspace, head, reason FROM start_checkpoints WHERE session_id = ?1",
                [session.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        Ok(row.map(|(workspace, head, reason)| StartCheckpoint {
            workspace: workspace.into(),
            head,
            reason,
        }))
    }

    fn acquire_lease(
        &self,
        session: &SessionId,
        owner: &str,
        ttl_millis: u64,
    ) -> Result<Option<Lease>, StoreError> {
        // The inherent method takes `impl Into<String>`; delegate.
        SqliteStore::acquire_lease(self, session, owner, ttl_millis)
    }

    fn renew_lease(
        &self,
        session: &SessionId,
        owner: &str,
        ttl_millis: u64,
    ) -> Result<bool, StoreError> {
        SqliteStore::renew_lease(self, session, owner, ttl_millis)
    }

    fn release_lease(&self, session: &SessionId, owner: &str) -> Result<bool, StoreError> {
        SqliteStore::release_lease(self, session, owner)
    }

    fn lease_holder(&self, session: &SessionId) -> Result<Option<(String, u64)>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT lease_owner, lease_until FROM sessions WHERE id = ?1",
                [session.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(backend)?;
        Ok(row.and_then(|(owner, until)| {
            (until > store_u64(now_millis())).then_some((owner, until.max(0).unsigned_abs()))
        }))
    }

    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError> {
        let connection = self.lock()?;
        let known: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                [session.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        if known.is_none() {
            return Ok(None);
        }

        let transcript = load_json_rows(
            &connection,
            "SELECT body_json FROM items WHERE session_id = ?1 ORDER BY idx ASC",
            session,
            |body| serde_json::from_str::<Item>(body),
        )?;
        let tasks = load_json_rows(
            &connection,
            "SELECT body_json FROM tasks WHERE session_id = ?1 ORDER BY id ASC",
            session,
            |body| serde_json::from_str::<TaskRecord>(body),
        )?;

        // The kernel-owned todo list is not item-keyed: a session
        // rebuilt from items starts with an empty list. The transcript's
        // appended todo blocks keep the model's view intact, and the
        // `TodoUpdated` event stream reconstructs the list for replay.
        Ok(Some(Session {
            id: *session,
            transcript,
            tasks,
            todos: TodoList::default(),
        }))
    }

    fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let connection = self.lock()?;
        let mut query = connection
            .prepare("SELECT id FROM sessions ORDER BY id ASC")
            .map_err(backend)?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?;
        rows.map(|row| {
            let raw = row.map_err(backend)?;
            raw.parse::<SessionId>().map_err(corrupt)
        })
        .collect()
    }
}

impl EventStore for SqliteStore {
    fn append_event(
        &self,
        session: &SessionId,
        envelope: &EventEnvelope,
    ) -> Result<(), StoreError> {
        let body = serde_json::to_string(envelope).map_err(backend)?;
        let connection = self.lock()?;
        // `position` is assigned by SQLite (AUTOINCREMENT), so the row
        // order is insertion order even when a resumed session's
        // envelope sequences restart from zero.
        connection
            .execute(
                "INSERT INTO events (session_id, task_id, body_json) VALUES (?1, ?2, ?3)",
                params![session.to_string(), envelope.task_id.to_string(), body],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn load_events(&self, session: &SessionId) -> Result<Vec<EventEnvelope>, StoreError> {
        let connection = self.lock()?;
        let mut query = connection
            .prepare("SELECT body_json FROM events WHERE session_id = ?1 ORDER BY position ASC")
            .map_err(backend)?;
        let rows = query
            .query_map([session.to_string()], |row| row.get::<_, String>(0))
            .map_err(backend)?;
        let mut events = Vec::new();
        for row in rows {
            let body = row.map_err(backend)?;
            match serde_json::from_str::<EventEnvelope>(&body) {
                Ok(envelope) => events.push(envelope),
                Err(parse) => {
                    // Review S7: an envelope this build cannot parse
                    // used to fail the whole session with an opaque
                    // unknown-variant error. If its recorded schema
                    // version is newer than ours, say that instead —
                    // the session is readable by a newer loom, not
                    // corrupt. Any other parse failure stays corrupt.
                    let newer = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|value| value["schema_version"].as_u64())
                        .filter(|version| {
                            *version > u64::from(loom_kernel::event::EVENT_SCHEMA_VERSION)
                        });
                    return Err(match newer {
                        Some(version) => StoreError::Corrupt(format!(
                            "session {session} was written by a newer loom \
                             (event schema v{version}; this build understands \
                             v{})",
                            loom_kernel::event::EVENT_SCHEMA_VERSION
                        )),
                        None => corrupt(parse),
                    });
                }
            }
        }
        Ok(events)
    }
}

impl ArtifactStore for SqliteStore {
    fn put(&self, artifact: &StoredArtifact) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO artifacts (id, sha256, kind, bytes, created_at, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(id) DO UPDATE SET \
                     sha256 = excluded.sha256, kind = excluded.kind, \
                     bytes = excluded.bytes, body = excluded.body",
                params![
                    artifact.id.to_string(),
                    &artifact.sha256,
                    &artifact.kind,
                    store_u64(artifact.bytes),
                    store_u64(now_millis()),
                    &artifact.body,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn get(&self, id: &ArtifactId) -> Result<Option<StoredArtifact>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT sha256, kind, body FROM artifacts WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        let Some((sha256, kind, body)) = row else {
            return Ok(None);
        };
        // The recorded hash is metadata; the body is what gets written
        // into files. Verify one against the other on every read
        // (review S2) so a drifted blob refuses instead of restoring.
        verify_body_hash(id, &sha256, &body)?;
        Ok(Some(StoredArtifact::new(*id, kind, sha256, body)))
    }

    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT id, kind, body FROM artifacts WHERE sha256 = ?1 ORDER BY id ASC LIMIT 1",
                [sha256],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        let Some((raw_id, kind, body)) = row else {
            return Ok(None);
        };
        let id = raw_id.parse::<ArtifactId>().map_err(corrupt)?;
        verify_body_hash(&id, sha256, &body)?;
        Ok(Some(StoredArtifact::new(id, kind, sha256.to_owned(), body)))
    }

    fn list(&self) -> Result<Vec<ArtifactStat>, StoreError> {
        let connection = self.lock()?;
        let mut query = connection
            .prepare(
                "SELECT id, kind, bytes, created_at FROM artifacts \
                 ORDER BY created_at ASC, id ASC",
            )
            .map_err(backend)?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(backend)?;
        rows.map(|row| {
            let (raw_id, kind, bytes, created_at) = row.map_err(backend)?;
            let id = raw_id.parse::<ArtifactId>().map_err(corrupt)?;
            Ok(ArtifactStat {
                id,
                kind,
                bytes: bytes.unsigned_abs(),
                created_at: created_at.unsigned_abs(),
            })
        })
        .collect()
    }

    fn delete(&self, id: &ArtifactId) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection
            .execute("DELETE FROM artifacts WHERE id = ?1", [id.to_string()])
            .map_err(backend)?;
        Ok(())
    }
}

/// Unix milliseconds, saturating at the epoch (never panics on a bad clock).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or_default())
        .unwrap_or_default()
}

/// Stores a `u64` column value (unix milliseconds, byte counts) in a SQLite
/// `INTEGER` column, which is i64-wide. Reads recover the value bit-exactly
/// with `i64::unsigned_abs`; clock- and file-derived inputs never approach
/// `i64::MAX`.
#[allow(clippy::cast_possible_wrap)] // SQLite INTEGER is i64; round-trips via unsigned_abs
fn store_u64(value: u64) -> i64 {
    value as i64
}

/// Serializes an item once for both its denormalized kind and authoritative
/// body columns.
fn serialize_item(item: &Item) -> Result<(String, String), StoreError> {
    let value = serde_json::to_value(item).map_err(backend)?;
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((kind, value.to_string()))
}

/// Inserts one transcript item, keeping its indexed kind and JSON body in sync.
fn insert_item(
    connection: &Connection,
    session: &SessionId,
    index: ItemIndex,
    item: &Item,
) -> Result<(), StoreError> {
    let (kind, body) = serialize_item(item)?;
    connection
        .execute(
            "INSERT INTO items (session_id, idx, kind, body_json) VALUES (?1, ?2, ?3, ?4)",
            params![session.to_string(), i64::from(index.as_u32()), kind, body],
        )
        .map_err(backend)?;
    Ok(())
}

/// Loads and decodes one JSON body column in the requested deterministic
/// order. Both transcript items and task records use this storage shape.
fn load_json_rows<T>(
    connection: &Connection,
    sql: &str,
    session: &SessionId,
    decode: impl for<'a> Fn(&'a str) -> Result<T, serde_json::Error>,
) -> Result<Vec<T>, StoreError> {
    let mut query = connection.prepare(sql).map_err(backend)?;
    let rows = query
        .query_map([session.to_string()], |row| row.get::<_, String>(0))
        .map_err(backend)?;
    rows.map(|row| {
        let body = row.map_err(backend)?;
        decode(&body).map_err(corrupt)
    })
    .collect()
}

/// Wraps a deserialisation failure as corrupt stored state.
fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}
