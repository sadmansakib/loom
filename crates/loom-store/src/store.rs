//! The SQLite-backed implementations of the kernel store traits.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_kernel::id::{ArtifactId, SessionId};
use loom_kernel::session::{PlannedTurn, Session, TaskRecord};
use loom_kernel::store::{ArtifactStat, ArtifactStore, SessionStore, StoreError, StoredArtifact};
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
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory().map_err(backend)?)
    }

    /// The default database path: `$LOOM_DB` when set, else
    /// `$XDG_DATA_HOME/loom/loom.db`, else `$HOME/.local/share/loom/loom.db`.
    ///
    /// Never inside the workspace (ADR-008): a database in the repo would be
    /// committed, cleaned, or shadowed by a worktree.
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
    pub fn open_default() -> Result<Self, StoreError> {
        Self::open(Self::default_path()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
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
        schema::migrate(&connection)?;
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
                params![owner, expires as i64, session.to_string(), now as i64],
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
        Ok(Some(Lease {
            session: *session,
            owner,
            expires_at_millis: expires,
        }))
    }

    /// Extends a held lease. Returns `false` if `owner` no longer holds it.
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
                params![expires as i64, session.to_string(), owner],
            )
            .map_err(backend)?;
        Ok(changed > 0)
    }

    /// Releases a held lease. Returns `false` if `owner` did not hold it.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    session: SessionId,
    owner: String,
    expires_at_millis: u64,
}

impl Lease {
    /// The session this lease guards.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The owner that acquired it.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// When the lease lapses if not renewed (Unix millis).
    pub fn expires_at_millis(&self) -> u64 {
        self.expires_at_millis
    }
}

impl SessionStore for SqliteStore {
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError> {
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR IGNORE INTO sessions (id, created_at, lease_owner, lease_until) \
                 VALUES (?1, ?2, NULL, NULL)",
                params![session.to_string(), now_millis() as i64],
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
        let value = serde_json::to_value(item).map_err(backend)?;
        let kind = value
            .get("kind")
            .and_then(|kind| kind.as_str())
            .unwrap_or_default()
            .to_owned();
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT INTO items (session_id, idx, kind, body_json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    session.to_string(),
                    i64::from(index.as_u32()),
                    kind,
                    value.to_string()
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(backend)?;
        for (index, item) in turn.items() {
            let value = serde_json::to_value(&item).map_err(backend)?;
            let kind = value
                .get("kind")
                .and_then(|kind| kind.as_str())
                .unwrap_or_default()
                .to_owned();
            transaction
                .execute(
                    "INSERT INTO items (session_id, idx, kind, body_json) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        session.to_string(),
                        i64::from(index.as_u32()),
                        kind,
                        value.to_string()
                    ],
                )
                .map_err(backend)?;
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

        let mut transcript = Vec::new();
        {
            let mut query = connection
                .prepare("SELECT body_json FROM items WHERE session_id = ?1 ORDER BY idx ASC")
                .map_err(backend)?;
            let rows = query
                .query_map([session.to_string()], |row| row.get::<_, String>(0))
                .map_err(backend)?;
            for row in rows {
                let body = row.map_err(backend)?;
                transcript.push(serde_json::from_str::<Item>(&body).map_err(corrupt)?);
            }
        }

        let mut tasks = Vec::new();
        {
            let mut query = connection
                .prepare("SELECT body_json FROM tasks WHERE session_id = ?1 ORDER BY id ASC")
                .map_err(backend)?;
            let rows = query
                .query_map([session.to_string()], |row| row.get::<_, String>(0))
                .map_err(backend)?;
            for row in rows {
                let body = row.map_err(backend)?;
                tasks.push(serde_json::from_str::<TaskRecord>(&body).map_err(corrupt)?);
            }
        }

        Ok(Some(Session {
            id: *session,
            transcript,
            tasks,
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
        let mut ids = Vec::new();
        for row in rows {
            let raw = row.map_err(backend)?;
            ids.push(raw.parse::<SessionId>().map_err(corrupt)?);
        }
        Ok(ids)
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
                    artifact.bytes as i64,
                    now_millis() as i64,
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
        Ok(row.map(|(sha256, kind, body)| StoredArtifact::new(*id, kind, sha256, body)))
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
        Ok(Some(StoredArtifact::new(id, kind, sha256, body)))
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
        let mut stats = Vec::new();
        for row in rows {
            let (raw_id, kind, bytes, created_at) = row.map_err(backend)?;
            let id = raw_id.parse::<ArtifactId>().map_err(corrupt)?;
            stats.push(ArtifactStat {
                id,
                kind,
                bytes: bytes.unsigned_abs(),
                created_at: created_at.unsigned_abs(),
            });
        }
        Ok(stats)
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
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Wraps a deserialisation failure as corrupt stored state.
fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}
