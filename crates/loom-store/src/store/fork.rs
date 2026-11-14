//! Session forking on [`SqliteStore`] (ADR-008).

use loom_kernel::id::SessionId;
use loom_kernel::store::{ForkProvenance, StoreError};
use loom_kernel::transcript::ItemIndex;
use rusqlite::{OptionalExtension, params};

use super::{SqliteStore, now_millis, store_u64};
use crate::schema::backend;

impl SqliteStore {
    /// Forks `source` at `boundary` into the fresh session `new_id` (ADR-008).
    ///
    /// Refuses a leased source so a live writer's tip is never raced.
    pub(crate) fn fork_session(
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
}
