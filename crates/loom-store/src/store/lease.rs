//! Single-writer lease operations on [`SqliteStore`] (ADR-008).

use loom_kernel::id::SessionId;
use loom_kernel::store::StoreError;
use rusqlite::{OptionalExtension, params};

use super::{Lease, SqliteStore, now_millis, store_u64};
use crate::schema::backend;

impl SqliteStore {
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
