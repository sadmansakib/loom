//! Workspace text plumbing: byte-exact snapshots with restore-on-failure,
//! Rust-file discovery, and word-bounded replace/count primitives.

use std::path::{Path, PathBuf};

/// One captured original file, for byte-exact restore on failure.
pub(super) struct Snapshot {
    pub(super) path: PathBuf,
    pub(super) bytes: Vec<u8>,
}

/// Restores every captured file to its original bytes.
pub(super) fn restore(snapshots: &[Snapshot]) {
    for snapshot in snapshots {
        if let Err(error) = std::fs::write(&snapshot.path, &snapshot.bytes) {
            tracing::warn!(
                path = %snapshot.path.display(),
                error = %error,
                "fast path revert failed"
            );
        }
    }
}

/// Collects the workspace's Rust source files: gitignore-aware walk,
/// with `target/` and dot-directories always skipped.
pub(super) fn rust_files(workspace: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(workspace);
    builder.hidden(true).git_ignore(true).parents(false);
    let walker =
        builder.filter_entry(|entry| !(entry.depth() > 0 && entry.file_name() == "target"));
    let mut files = Vec::new();
    for entry in walker.build().flatten() {
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.path().extension().is_some_and(|ext| ext == "rs")
        {
            files.push(entry.into_path());
        }
    }
    files
}

/// Whether `needle` at `at` in `hay` stands alone (word boundaries both
/// sides). Identifiers are ASCII, so byte checks are sound.
pub(super) fn word_bounded(hay: &str, needle: &str, at: usize) -> bool {
    let bytes = hay.as_bytes();
    let head = at
        .checked_sub(1)
        .is_none_or(|prev| !is_word_byte(bytes[prev]));
    let tail = bytes
        .get(at + needle.len())
        .is_none_or(|next| !is_word_byte(*next));
    head && tail
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Replaces whole-word occurrences of `from` with `to`, returning the
/// new text and the replacement count.
pub(super) fn replace_word(text: &str, from: &str, to: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(from) {
        let at = cursor + found;
        if word_bounded(text, from, at) {
            out.push_str(&text[cursor..at]);
            out.push_str(to);
            cursor = at + from.len();
            count += 1;
        } else {
            let boundary = at + from.len();
            out.push_str(&text[cursor..boundary]);
            cursor = boundary;
        }
    }
    out.push_str(&text[cursor..]);
    (out, count)
}

/// Counts whole-word occurrences of `needle` in `text`.
pub(super) fn count_word(text: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(needle) {
        let at = cursor + found;
        if word_bounded(text, needle, at) {
            count += 1;
        }
        cursor = at + needle.len();
    }
    count
}
