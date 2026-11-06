//! Pre-image capture for tier-2 rewind (`docs/rewind-design.md` §4).
//!
//! Every mutating tool records, beside `files_written`/`files_deleted`,
//! the content of each path as it was immediately before the call — or
//! that the path did not exist — so a later rewind can restore the tree
//! to any transcript boundary. Capture happens at the tools seam from
//! the file the tool is about to modify, never inferred (ADR-012):
//! reverse-replaying serialized edit arguments would silently break on
//! whitespace repairs and out-of-band command effects.
//!
//! The capture helper itself is pure over its inputs (`Option` bytes =
//! the file existed, `None` = the caller established absence); the
//! filesystem reads stay where each tool already does blocking work.
//! Bodies are content-addressed into the artifact store (sha256
//! identity, dedupe for free), so identical pre-images are one body.
//!
//! Degradation is honest, never silent: with no artifact store (or if
//! the store refuses the body) no entry is recorded, and restore later
//! refuses for that path with the captured-missing reason — the
//! pre-artifact spill behavior for tool output is the same shape
//! (best-effort store, refusal downstream).

use loom_kernel::artifact::store_artifact;
use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::BeforeImage;

/// Artifact kind for pre-image bodies.
pub(crate) const PRE_IMAGE_KIND: &str = "pre_image";

/// Captures one path's pre-image: `existing` is the file's bytes when
/// it exists, `None` when the caller established the file is absent
/// (restoring then deletes it).
///
/// Returns `None` — record nothing — when the bytes exist but no store
/// is attached or the store refuses the body. A missing entry is the
/// honest "not provable" signal restore refuses on; it never stands for
/// "absent" (an absent file is a positive claim a restore may act on,
/// and it needs no body).
pub(crate) fn capture_before_image(
    store: Option<&dyn ArtifactStore>,
    path: &str,
    existing: Option<&[u8]>,
) -> Option<BeforeImage> {
    match existing {
        None => Some(BeforeImage::absent(path)),
        Some(bytes) => {
            let store = store?;
            let reference = store_artifact(store, PRE_IMAGE_KIND, bytes).ok()?;
            Some(BeforeImage::captured(path, reference))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::store::MemoryArtifactStore;

    #[test]
    fn an_absent_file_needs_no_store() {
        let image = capture_before_image(None, "new.txt", None).expect("absent entry");
        assert!(image.content.is_none());
    }

    #[test]
    fn existing_bytes_stay_stored_and_addressed() {
        let store = MemoryArtifactStore::new();
        let image = capture_before_image(Some(&store), "a.txt", Some(b"old")).expect("stored");
        let reference = image.content.expect("a ref");
        assert_eq!(reference.kind, PRE_IMAGE_KIND);
        let body = store.get(&reference.id).unwrap().expect("present");
        assert_eq!(body.body, b"old");
        assert_eq!(body.sha256, reference.sha256);
    }

    #[test]
    fn no_store_means_no_entry_for_an_existing_file() {
        // The honest "not provable" signal: absent-file entries and
        // missing entries must stay distinguishable.
        assert!(capture_before_image(None, "a.txt", Some(b"old")).is_none());
    }
}
