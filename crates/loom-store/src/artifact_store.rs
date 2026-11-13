//! SQLite implementation of content-addressed artifact storage.

use loom_kernel::id::ArtifactId;
use loom_kernel::store::{
    ArtifactStat, ArtifactStore, StoreError, StoredArtifact, verify_body_hash,
};
use rusqlite::{OptionalExtension, params};

use crate::schema::backend;
use crate::store::{SqliteStore, corrupt, now_millis, store_u64};

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
