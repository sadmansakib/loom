//! Schema definition and forward-only migrations (ADR-008).
//!
//! A database records the schema version it was created with in `meta`; a
//! build refuses to open a file written by a newer build rather than
//! mis-reading it. Version 1 is the first schema, so migration is just
//! "create the tables" — later versions append steps here.

use loom_kernel::store::StoreError;
use rusqlite::{Connection, OptionalExtension, params};

/// The schema version this build writes and understands.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// `meta` holds bookkeeping, not application data (`schema_version`).
const META: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Version 1: sessions, their items and tasks, and artifact bodies.
///
/// `items.body_json` is the authoritative serde form of an `Item` (ADR-004);
/// `items.kind` is denormalised for the future search index (Iteration 3,
/// ADR-010) — schema v1 deliberately has no FTS. `artifacts.sha256` is the
/// content identity (ADR-012 decision 3).
const V1: &str = "\
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER
);

CREATE TABLE IF NOT EXISTS items (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    idx        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    body_json  TEXT NOT NULL,
    PRIMARY KEY (session_id, idx)
);

CREATE TABLE IF NOT EXISTS tasks (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id         TEXT NOT NULL,
    status     TEXT NOT NULL,
    body_json  TEXT NOT NULL,
    PRIMARY KEY (session_id, id)
);

CREATE TABLE IF NOT EXISTS artifacts (
    id         TEXT PRIMARY KEY,
    sha256     TEXT NOT NULL,
    kind       TEXT NOT NULL,
    bytes      INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    body       BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS artifacts_sha256 ON artifacts (sha256);
";

/// Brings `connection` to [`SCHEMA_VERSION`], refusing a newer file.
pub(crate) fn migrate(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(META).map_err(backend)?;

    let stored = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(backend)?;

    let stored = match stored {
        None => None,
        Some(raw) => Some(raw.parse::<u32>().map_err(|_| {
            StoreError::Corrupt(format!("meta.schema_version is not a number: {raw:?}"))
        })?),
    };

    match stored {
        Some(version) if version == SCHEMA_VERSION => Ok(()),
        Some(version) if version > SCHEMA_VERSION => Err(StoreError::Corrupt(format!(
            "database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
        ))),
        // Fresh file, or a version older than this build: apply v1 (idempotent
        // `IF NOT EXISTS`) and record the version. A future migration chain
        // inserts steps between here and the version write.
        _ => {
            connection.execute_batch(V1).map_err(backend)?;
            connection
                .execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![SCHEMA_VERSION.to_string()],
                )
                .map_err(backend)?;
            Ok(())
        }
    }
}

/// Formats any underlying error as a backend failure.
pub(crate) fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}
