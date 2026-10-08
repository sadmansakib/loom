//! Content-hash result cache for read-only tool calls (2G, ADR-018, V14).
//!
//! Repeating an identical read-only call is only safe while the files it
//! read are unchanged, so a cached result is keyed by the call (tool +
//! canonical arguments) *and* by a signature of every path it read:
//! `path + size + mtime + content hash`. On a lookup the signature is
//! recomputed and compared; the content hash makes a same-size,
//! same-mtime edit a miss regardless of `mtime` resolution, so a stale
//! result is never served.
//!
//! Invalidation is effect-driven, never time-based: any recorded file
//! mutation drops entries whose signature path overlaps a changed path,
//! and any `run_command` drops the whole cache (a command can change
//! anything, so the cache is conservatively emptied). Only read-only,
//! side-effect-free calls are ever stored; `run_command` is never cached.
//!
//! Locking discipline (ADR-018): the signature walk ([`compute_signature`])
//! does file I/O and SHA-256 hashing, so it is **separate** from the map
//! operations. The caller (the tool engine) computes signatures on a
//! blocking thread and hands the result to [`ToolCache::lookup`] /
//! [`ToolCache::store`], which only touch the in-memory map — so hashing
//! never runs on an async worker and never runs while the cache mutex is
//! held (concurrent read-only calls hash in parallel).
//!
//! Shared through [`ToolContext::cache`](crate::tool::ToolContext::cache).

use std::collections::hash_map::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::tool::{ToolEffects, ToolOutput, paths_overlap};

/// One file's identity when a result was cached. `hash` is `None` for a
/// path that did not exist then; `size`/`modified` are then unused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSignature {
    relative: String,
    size: u64,
    modified: Option<SystemTime>,
    hash: Option<[u8; 32]>,
}

/// Computes the signature of every path in `reads`, or `None` when any
/// path is a directory or cannot be inspected — such a call is not
/// cacheable. A missing path is a valid signature (cached as absent, so a
/// later creation is a miss).
///
/// Blocking (metadata + full-file reads for the content hashes); callers
/// run it on a blocking thread, never inside the cache lock.
pub(crate) fn compute_signature(workspace: &Path, reads: &[String]) -> Option<Vec<FileSignature>> {
    let mut signature = Vec::with_capacity(reads.len());
    for relative in reads {
        let full = workspace.join(relative);
        match fs::metadata(&full) {
            Ok(meta) if meta.is_file() => {
                signature.push(FileSignature {
                    relative: relative.clone(),
                    size: meta.len(),
                    modified: meta.modified().ok(),
                    hash: Some(hash_file(&full).ok()?),
                });
            }
            Ok(_) => return None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                signature.push(FileSignature {
                    relative: relative.clone(),
                    size: 0,
                    modified: None,
                    hash: None,
                });
            }
            Err(_) => return None,
        }
    }
    Some(signature)
}

/// Content hash of a file's bytes.
fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().into())
}

/// A cached call: the signature it was stored under, the id of the call
/// that produced it, and the raw output (before bounding, so a hit is
/// re-bounded exactly like a fresh call).
#[derive(Debug, Clone)]
struct Entry {
    signature: Vec<FileSignature>,
    call_id: String,
    output: ToolOutput,
}

/// The process-wide result cache for read-only calls (2G, ADR-018).
///
/// Keyed by `(tool, canonical arguments)`; every hit re-verifies the
/// stored file signature by content hash. Bounded only by what was
/// stored — invocation scope is a single agent run, and any mutation or
/// `run_command` clears the affected entries.
#[derive(Debug, Default)]
pub struct ToolCache {
    entries: HashMap<(String, String), Entry>,
    hits: u64,
    misses: u64,
}

impl ToolCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries served from the cache so far.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Number of lookups that found no usable entry.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no call is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `(tool, args)` has an entry, without counting a lookup.
    ///
    /// A cheap pre-probe: lets the caller skip the (blocking) signature
    /// walk entirely for keys that were never stored.
    #[must_use]
    pub(crate) fn has_entry(&self, tool: &str, args: &Value) -> bool {
        self.entries
            .contains_key(&(tool.to_owned(), args.to_string()))
    }

    /// Returns the cached output for `(tool, args)` and the id of the call
    /// that produced it, when `current` — the caller-computed
    /// [`compute_signature`] of the call's read set — matches the stored
    /// signature. A mismatch drops the entry.
    ///
    /// Map access only: no file I/O, safe to call under the cache lock.
    pub(crate) fn lookup(
        &mut self,
        tool: &str,
        args: &Value,
        current: Option<Vec<FileSignature>>,
    ) -> Option<(ToolOutput, String)> {
        let key = (tool.to_owned(), args.to_string());
        if !self.entries.contains_key(&key) {
            self.misses += 1;
            return None;
        }
        let hit = match (&current, self.entries.get(&key)) {
            (Some(current), Some(entry)) => *current == entry.signature,
            _ => false,
        };
        if hit {
            self.hits += 1;
            return self
                .entries
                .get(&key)
                .map(|entry| (entry.output.clone(), entry.call_id.clone()));
        }
        self.entries.remove(&key);
        self.misses += 1;
        None
    }

    /// Stores `output` for `(tool, args)` under `signature` — the
    /// caller-computed [`compute_signature`] of the call's read set — as
    /// the result of `call_id`. Does nothing when the output had side
    /// effects (only read-only, effect-free results are cacheable) or when
    /// the signature is `None` (some read path is not a plain file).
    ///
    /// Map access only: no file I/O, safe to call under the cache lock.
    pub(crate) fn store(
        &mut self,
        tool: &str,
        args: &Value,
        signature: Option<Vec<FileSignature>>,
        call_id: &str,
        output: &ToolOutput,
    ) {
        if !output.effects.is_empty() {
            return;
        }
        let Some(signature) = signature else {
            return;
        };
        self.entries.insert(
            (tool.to_owned(), args.to_string()),
            Entry {
                signature,
                call_id: call_id.to_owned(),
                output: output.clone(),
            },
        );
    }

    /// Drops entries invalidated by recorded effects: any `run_command`
    /// clears the whole cache, and a file mutation drops entries whose
    /// signature path overlaps a changed path.
    pub fn invalidate_effects(&mut self, effects: &ToolEffects) {
        if !effects.commands_run.is_empty() {
            self.entries.clear();
            return;
        }
        let changed: Vec<&String> = effects
            .files_written
            .iter()
            .chain(effects.files_deleted.iter())
            .collect();
        if changed.is_empty() {
            return;
        }
        self.entries.retain(|_, entry| {
            !entry.signature.iter().any(|signature| {
                changed
                    .iter()
                    .any(|path| paths_overlap(&signature.relative, path))
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolEffects;

    fn workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-cache-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).expect("create temp workspace");
        dir
    }

    fn write(dir: &Path, relative: &str, body: &str) {
        let full = dir.join(relative);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(full, body).expect("write file");
    }

    fn output(body: &str) -> ToolOutput {
        ToolOutput::new(body)
    }

    /// `store`/`lookup` pairs as the tool engine issues them: signature
    /// computed first, then handed to the map operations.
    fn sig(dir: &Path, reads: &[String]) -> Option<Vec<FileSignature>> {
        compute_signature(dir, reads)
    }

    #[test]
    fn stores_and_serves_an_unchanged_read() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        let args = serde_json::json!({ "path": "src/a.rs" });
        let reads = vec!["src/a.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args,
            sig(&dir, &reads),
            "call-1",
            &output("fn a() {}"),
        );
        let hit = cache.lookup("read_file", &args, sig(&dir, &reads));
        assert_eq!(
            hit.map(|(out, _)| out.content),
            Some("fn a() {}".to_owned())
        );
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn a_hit_carries_the_producing_call_id() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        let args = serde_json::json!({ "path": "src/a.rs" });
        let reads = vec!["src/a.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args,
            sig(&dir, &reads),
            "toolu_1",
            &output("fn a() {}"),
        );
        let hit = cache.lookup("read_file", &args, sig(&dir, &reads));
        assert_eq!(
            hit.map(|(_, id)| id),
            Some("toolu_1".to_owned()),
            "a hit must report which call produced the cached output"
        );
    }

    #[test]
    fn a_content_change_is_a_miss() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        let args = serde_json::json!({ "path": "src/a.rs" });
        let reads = vec!["src/a.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args,
            sig(&dir, &reads),
            "call-1",
            &output("fn a() {}"),
        );
        // Same size, same mtime resolution risk: the content hash decides.
        write(&dir, "src/a.rs", "fn b() {}");
        assert!(
            cache
                .lookup("read_file", &args, sig(&dir, &reads))
                .is_none()
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn a_missing_file_is_cached_as_absent_then_misses_when_created() {
        let dir = workspace();
        let args = serde_json::json!({ "path": "src/new.rs" });
        let reads = vec!["src/new.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args,
            sig(&dir, &reads),
            "call-1",
            &output("not found"),
        );
        assert!(
            cache
                .lookup("read_file", &args, sig(&dir, &reads))
                .is_some()
        );
        write(&dir, "src/new.rs", "fn new() {}");
        assert!(
            cache
                .lookup("read_file", &args, sig(&dir, &reads))
                .is_none()
        );
    }

    #[test]
    fn a_directory_read_is_never_cached() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        let args = serde_json::json!({ "path": "src" });
        let reads = vec!["src".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "list_dir",
            &args,
            sig(&dir, &reads),
            "call-1",
            &output("a.rs"),
        );
        assert!(cache.is_empty());
        assert!(cache.lookup("list_dir", &args, sig(&dir, &reads)).is_none());
    }

    #[test]
    fn a_stored_output_with_effects_is_not_cached() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        let args = serde_json::json!({ "path": "src/a.rs" });
        let reads = vec!["src/a.rs".to_owned()];

        let mut cache = ToolCache::new();
        let side_effecting =
            output("fn a() {}").with_effects(ToolEffects::none().with_file_written("src/a.rs"));
        cache.store(
            "read_file",
            &args,
            sig(&dir, &reads),
            "call-1",
            &side_effecting,
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn a_write_effect_invalidates_overlapping_entries_only() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        write(&dir, "src/b.rs", "fn b() {}");
        let args_a = serde_json::json!({ "path": "src/a.rs" });
        let args_b = serde_json::json!({ "path": "src/b.rs" });
        let reads_a = vec!["src/a.rs".to_owned()];
        let reads_b = vec!["src/b.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args_a,
            sig(&dir, &reads_a),
            "call-a",
            &output("a"),
        );
        cache.store(
            "read_file",
            &args_b,
            sig(&dir, &reads_b),
            "call-b",
            &output("b"),
        );
        assert_eq!(cache.len(), 2);

        cache.invalidate_effects(&ToolEffects::none().with_file_written("src/a.rs"));
        assert_eq!(cache.len(), 1);
        assert!(
            cache
                .lookup("read_file", &args_a, sig(&dir, &reads_a))
                .is_none()
        );
        assert!(
            cache
                .lookup("read_file", &args_b, sig(&dir, &reads_b))
                .is_some()
        );
    }

    #[test]
    fn a_run_command_clears_the_whole_cache() {
        let dir = workspace();
        write(&dir, "src/a.rs", "fn a() {}");
        write(&dir, "src/b.rs", "fn b() {}");
        let args_a = serde_json::json!({ "path": "src/a.rs" });
        let args_b = serde_json::json!({ "path": "src/b.rs" });
        let reads_a = vec!["src/a.rs".to_owned()];
        let reads_b = vec!["src/b.rs".to_owned()];

        let mut cache = ToolCache::new();
        cache.store(
            "read_file",
            &args_a,
            sig(&dir, &reads_a),
            "call-a",
            &output("a"),
        );
        cache.store(
            "read_file",
            &args_b,
            sig(&dir, &reads_b),
            "call-b",
            &output("b"),
        );

        let command = crate::tool::CommandEffect::new("cargo", vec!["test".to_owned()], Some(0));
        cache.invalidate_effects(&ToolEffects::none().with_command(command));
        assert!(cache.is_empty());
    }
}
