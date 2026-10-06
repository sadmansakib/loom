//! Artifact references: the compact handle a large tool result collapses to.
//!
//! Progressive disclosure (ADR-012 decision 3, ADR-014). When a tool result
//! is too large to travel inline, its body is stored in the [`ArtifactStore`]
//! and the transcript carries an [`ArtifactRef`] instead — `id`, `kind`,
//! `bytes`, `sha256`. The ref is the result; the artifact is its body. A
//! read-only tool refetches the body with the same windowing `read_file`
//! uses, so nothing is lost, only deferred.
//!
//! Bodies are content-addressed: the same bytes always hash to the same
//! `sha256`, so identical outputs dedupe on disk, and a ref stays valid after
//! the file it was read from changes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::id::ArtifactId;
use crate::store::{ArtifactStore, StoreError, StoredArtifact};

/// A compact, model-visible handle to a stored artifact body.
///
/// The transcript carries this in place of an oversized tool result; the
/// full body lives in the [`ArtifactStore`] under [`ArtifactRef::id`]. It is
/// `#[non_exhaustive]` so a future field (e.g. a media type) does not break
/// the persisted form.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Store key for the body.
    pub id: ArtifactId,
    /// What kind of body this is (for now always `"tool_output"`).
    pub kind: String,
    /// Body length in bytes, so a reader can size the refetch.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the body.
    pub sha256: String,
}

impl ArtifactRef {
    /// Builds a reference from its parts.
    pub fn new(
        id: ArtifactId,
        kind: impl Into<String>,
        bytes: u64,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind: kind.into(),
            bytes,
            sha256: sha256.into(),
        }
    }
}

/// Stores `body` as an artifact of `kind` and returns its reference.
///
/// The body is written before the reference is returned, so a ref in the
/// transcript is never dangling: the worst a crash between the write and the
/// referencing turn's commit can leave is an orphaned body, which is
/// harmless because bodies are immutable and content-addressed.
pub fn store_artifact(
    store: &dyn ArtifactStore,
    kind: &str,
    body: &[u8],
) -> Result<ArtifactRef, StoreError> {
    let sha256 = sha256_hex(body);
    let id = ArtifactId::new();
    let stored = StoredArtifact::new(id, kind, sha256.clone(), body.to_vec());
    store.put(&stored)?;
    Ok(ArtifactRef::new(id, kind, body.len() as u64, sha256))
}

/// Lowercase hex SHA-256 of `body`.
pub fn sha256_hex(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryArtifactStore;

    #[test]
    fn sha256_matches_the_known_digest_of_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn an_artifact_round_trips_through_the_store() {
        let store = MemoryArtifactStore::new();
        let reference = store_artifact(&store, "tool_output", b"hello artifact").expect("store");
        assert_eq!(reference.kind, "tool_output");
        assert_eq!(reference.bytes, 14);
        assert_eq!(reference.sha256, sha256_hex(b"hello artifact"));
        let body = store.get(&reference.id).expect("get").expect("present");
        assert_eq!(body.body, b"hello artifact");
        assert_eq!(body.sha256, reference.sha256);
        // Content addressing: the same bytes are findable by their hash.
        let found = store
            .find_by_sha256(&reference.sha256)
            .expect("find")
            .expect("present");
        assert_eq!(found.id, reference.id);
    }

    #[test]
    fn identical_bodies_share_one_hash() {
        assert_eq!(sha256_hex(b"same"), sha256_hex(b"same"));
        assert_ne!(sha256_hex(b"same"), sha256_hex(b"different"));
    }
}
