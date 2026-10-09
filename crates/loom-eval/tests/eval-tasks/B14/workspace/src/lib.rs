//! A result cache for read-only tool calls.
//!
//! ## Contract
//!
//! [`ToolCache::store`] records a call's output under its
//! `(tool, args)` key together with a signature of everything the
//! answer depended on: for each read path, the file's current
//! [`FileState`] — or `None` when the path does not exist. A call with
//! no read paths records an empty signature.
//!
//! [`ToolCache::lookup`] serves a hit only when the recorded answer is
//! still provably valid: the key matches **and** every recorded
//! `(path, state)` pair still equals the file's current state, absence
//! included. Any difference — content, size, mtime, or a file that
//! appeared or disappeared — is a miss.
//!
//! [`ToolCache::invalidate`] keeps the cache honest across effects:
//! a written or deleted path drops every entry that read that path or
//! a path overlapping it (equal, or one a directory ancestor of the
//! other), and [`Effect::CommandRun`] clears the whole cache, since a
//! command may change anything.

use std::collections::HashMap;

/// The observable state of one file: the tuple a hit must reproduce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileState {
    pub size: u64,
    pub mtime: u64,
    pub content_hash: String,
}

/// A recorded side effect that may invalidate cached answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    FileWritten(String),
    FileDeleted(String),
    CommandRun,
}

type Key = (String, String);

struct Entry {
    reads: Vec<(String, Option<FileState>)>,
    output: String,
}

/// Caches outputs of read-only calls, keyed by call and verified
/// against file state.
#[derive(Default)]
pub struct ToolCache {
    entries: HashMap<Key, Entry>,
}

impl ToolCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `output` for the call `(tool, args)` with the current
    /// state of every path it read.
    pub fn store<F>(&mut self, tool: &str, args: &str, reads: &[&str], output: &str, current: &F)
    where
        F: Fn(&str) -> Option<FileState>,
    {
        let signature = reads
            .iter()
            .map(|path| ((*path).to_owned(), current(path)))
            .collect();
        self.entries.insert(
            (tool.to_owned(), args.to_owned()),
            Entry {
                reads: signature,
                output: output.to_owned(),
            },
        );
    }

    /// Serves the recorded output when the key matches and every
    /// recorded file state is unchanged; otherwise `None`.
    pub fn lookup<F>(&mut self, tool: &str, args: &str, current: &F) -> Option<String>
    where
        F: Fn(&str) -> Option<FileState>,
    {
        self.entries
            .get(&(tool.to_owned(), args.to_owned()))
            .map(|entry| entry.output.clone())
    }

    /// Drops entries invalidated by the given effects.
    pub fn invalidate(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::CommandRun => self.entries.clear(),
                Effect::FileWritten(path) | Effect::FileDeleted(path) => {
                    self.entries
                        .retain(|_, entry| {
                            !entry
                                .reads
                                .iter()
                                .any(|(read, _)| paths_overlap(read, path))
                        });
                }
            }
        }
    }
}

/// Whether two workspace paths overlap: equal, or one a strict
/// ancestor directory of the other.
fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
}

fn is_ancestor(dir: &str, path: &str) -> bool {
    let mut dir_segments = dir.split('/');
    let mut segments = path.split('/');
    loop {
        match (dir_segments.next(), segments.next()) {
            (Some(d), Some(p)) if d == p => continue,
            (None, Some(_)) => return true,
            _ => return false,
        }
    }
}
