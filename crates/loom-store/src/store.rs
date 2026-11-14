//! The SQLite-backed implementations of the kernel store traits.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_kernel::id::SessionId;
use loom_kernel::session::{PlannedTurn, Session, TaskRecord, TodoList};
use loom_kernel::store::{ForkProvenance, SessionStore, StartCheckpoint, StoreError};
use loom_kernel::transcript::{Item, ItemIndex};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{self, backend};

mod fork;
mod lease;

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

    pub(super) fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::Backend("store lock poisoned".to_owned()))
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
        self.fork_session(source, boundary, new_id)
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

/// Unix milliseconds, saturating at the epoch (never panics on a bad clock).
pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or_default(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or_default())
}

/// Stores a `u64` column value (unix milliseconds, byte counts) in a SQLite
/// `INTEGER` column, which is i64-wide. Reads recover the value bit-exactly
/// with `i64::unsigned_abs`; clock- and file-derived inputs never approach
/// `i64::MAX`.
#[allow(clippy::cast_possible_wrap)] // SQLite INTEGER is i64; round-trips via unsigned_abs
pub(super) fn store_u64(value: u64) -> i64 {
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
pub(super) fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}
