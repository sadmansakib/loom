//! SQLite implementation of the persisted event stream.

use loom_kernel::event::EventEnvelope;
use loom_kernel::id::SessionId;
use loom_kernel::store::{EventStore, StoreError};
use rusqlite::params;

use crate::schema::backend;
use crate::store::{SqliteStore, corrupt};

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
