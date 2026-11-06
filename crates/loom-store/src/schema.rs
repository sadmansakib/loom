//! Schema definition and forward-only migrations (ADR-008).
//!
//! A database records the schema version it was created with in `meta`; a
//! build refuses to open a file written by a newer build rather than
//! mis-reading it. Version 1 is the first schema, so migration is just
//! "create the tables" — later versions append steps here.

use loom_kernel::store::StoreError;
use rusqlite::{Connection, OptionalExtension, params};

/// The schema version this build writes and understands.
pub(crate) const SCHEMA_VERSION: u32 = 4;

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

/// Version 2: the persisted event stream (roadmap item 1's open tail).
///
/// One row per emitted [`EventEnvelope`](loom_kernel::event::EventEnvelope):
/// `body_json` is the envelope's authoritative serde form — the same
/// shape `events.ndjson` traces carry — so a stored stream and an
/// exported trace deserialize identically. Rows order by the
/// autoincremented `position` (insertion order), *not* by the
/// envelope's `sequence`: sequence numbers are per sink and a resumed
/// session numbers its second run from zero again. `task_id` is
/// denormalised the way `items.kind` is, for future per-task filters.
/// Additive: a v1 database migrates up in place, and a session without
/// event rows loads exactly as before.
const V2: &str = "\
CREATE TABLE IF NOT EXISTS events (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position   INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL,
    body_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS events_session ON events (session_id, position);
";

/// Version 3: fork provenance for the rewind surface
/// (`docs/rewind-design.md`).
///
/// One row per session that was created by
/// [`SessionStore::fork`](loom_kernel::store::SessionStore::fork): the
/// source session and the transcript boundary the fork kept. The fork
/// copies item rows directly (same `idx` values, so indices stay stable
/// across the fork), which is why there is nothing else to record —
/// this table is lineage, not content. A session without a row is an
/// original, and loads exactly as before; additive, like every step.
const V3: &str = "\
CREATE TABLE IF NOT EXISTS forks (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    forked_from TEXT NOT NULL,
    boundary   INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
";

/// Version 4: workspace start checkpoints for the rewind surface
/// (`docs/rewind-design.md` §4, tier 1).
///
/// One row per persisted session, written by the frontend at `loom
/// run` start. `head` is the tier-1 restore point when the workspace
/// was a clean git checkout; a NULL `head` keeps the refusal reason
/// (dirty start, not a repo) so a later `--restore-workspace` can
/// explain itself instead of guessing. Sessions without a row predate
/// the feature and load exactly as before — additive, like every step.
const V4: &str = "\
CREATE TABLE IF NOT EXISTS start_checkpoints (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    workspace  TEXT NOT NULL,
    head       TEXT,
    reason     TEXT,
    created_at INTEGER NOT NULL
);
";

/// The ordered migration chain, oldest step first. Each step is
/// idempotent (`IF NOT EXISTS`) so a fresh file and an upgrade run the
/// same statements.
const STEPS: &[&str] = &[V1, V2, V3, V4];

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
        // Fresh file, or a version older than this build: walk every
        // step (idempotent) and record the version this build writes.
        _ => {
            for step in STEPS {
                connection.execute_batch(step).map_err(backend)?;
            }
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
