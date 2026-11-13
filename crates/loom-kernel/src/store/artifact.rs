use serde::{Deserialize, Serialize};

use crate::id::ArtifactId;

use super::StoreError;

/// A stored artifact body with its metadata (§ 2D consumes this; ADR-012).
///
/// The transcript carries a compact reference; the body lives here.
/// Content identity is [`StoredArtifact::sha256`]; [`StoredArtifact::id`] is
/// the stable row key a reference carries.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    /// The row key a transcript reference carries.
    pub id: ArtifactId,
    /// Artifact kind (e.g. a tool name), for routing and display.
    pub kind: String,
    /// Hex sha256 of `body`; the content identity.
    pub sha256: String,
    /// Length of `body` in bytes.
    pub bytes: u64,
    /// The full body.
    pub body: Vec<u8>,
}

impl StoredArtifact {
    /// Builds an artifact record, deriving `bytes` from the body length.
    pub fn new(
        id: ArtifactId,
        kind: impl Into<String>,
        sha256: impl Into<String>,
        body: Vec<u8>,
    ) -> Self {
        Self {
            id,
            kind: kind.into(),
            sha256: sha256.into(),
            bytes: body.len() as u64,
            body,
        }
    }
}

/// Metadata for one stored artifact body, without the body itself.
///
/// The shape [`ArtifactStore::list`] returns; the GC (2D lifecycle) and the
/// stats surface consume it, neither of which needs the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactStat {
    /// The row key a transcript reference carries.
    pub id: ArtifactId,
    /// Artifact kind (e.g. a tool name), for routing and display.
    pub kind: String,
    /// Length of the body in bytes.
    pub bytes: u64,
    /// First-write time, milliseconds since the Unix epoch. Storing the
    /// same id again replaces the body but never resets this.
    pub created_at: u64,
}

/// The read-side integrity check shared by every [`ArtifactStore`]
/// implementation (review S2): a body must still hash to its recorded
/// `sha256` before it is returned. Refusal beats best-effort — a body
/// that drifted (bit rot, a hand-edited blob, a foreign store) never
/// reaches a restore.
///
/// # Errors
///
/// Returns [`StoreError::Corrupt`] when `body` no longer hashes to
/// `recorded`.
pub fn verify_body_hash(id: &ArtifactId, recorded: &str, body: &[u8]) -> Result<(), StoreError> {
    let computed = crate::artifact::sha256_hex(body);
    if computed == recorded {
        return Ok(());
    }
    Err(StoreError::Corrupt(format!(
        "artifact {id} body hashes to {computed} but its record says {recorded}; \
         refusing to return a body that drifted from its content address"
    )))
}

/// Stores artifact bodies addressed by [`ArtifactId`] (ADR-012).
///
/// Kept separate from [`SessionStore`](super::SessionStore) because the two are written in the same
/// transaction by the SQLite implementation but read independently: the loop
/// only needs session append during a run, while `artifact_read` (2D) only
/// needs lookup.
///
/// Read integrity is part of the contract (review S2): `get` and
/// `find_by_sha256` verify the body still hashes to the recorded
/// `sha256` before returning it — a body that drifted (bit rot, a
/// hand-edited blob, a foreign store) surfaces as
/// [`StoreError::Corrupt`] instead of flowing into restores.
pub trait ArtifactStore: Send + Sync {
    /// Stores a body. Storing the same id twice replaces the row.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot persist the row.
    fn put(&self, artifact: &StoredArtifact) -> Result<(), StoreError>;

    /// Fetches a body by id, verifying its content hash first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] when the body no longer hashes
    /// to its recorded `sha256`, and [`StoreError`] on backend
    /// failure.
    fn get(&self, id: &ArtifactId) -> Result<Option<StoredArtifact>, StoreError>;

    /// Finds a body by content hash, for deduplication; the body
    /// returned is verified to still hash to `sha256`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Corrupt`] when the body no longer hashes
    /// to its recorded `sha256`, and [`StoreError`] on backend
    /// failure.
    fn find_by_sha256(&self, sha256: &str) -> Result<Option<StoredArtifact>, StoreError>;

    /// Lists every artifact's metadata, oldest first. Bodies are not
    /// included; this is the GC's and the stats surface's read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot enumerate the
    /// rows.
    fn list(&self) -> Result<Vec<ArtifactStat>, StoreError>;

    /// Deletes a body. Only the GC calls this, and only for ids the
    /// reference scan proved unreferenced.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backend cannot delete the row.
    fn delete(&self, id: &ArtifactId) -> Result<(), StoreError>;
}
