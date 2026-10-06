//! Durable-state locations for the `loom` binary.
//!
//! Deciding where `loom.db` lives is a frontend responsibility: the engine
//! accepts an explicit path and never consults the environment. This module
//! is the CLI's policy — accounts, sessions, and artifacts share one SQLite
//! file, kept outside the workspace because the workspace is walked by the
//! `search` tool and may be committed (ADR-008).

use std::path::PathBuf;

/// Root of Loom's per-user data directory.
///
/// Resolution order: `LOOM_DATA_DIR` (tests, CI) → `$XDG_DATA_HOME/loom` →
/// `$HOME/.local/share/loom`. Falls back to the current directory only when
/// neither variable is set, so the process never panics on a stripped
/// environment.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOOM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("loom")
}

/// Path to the SQLite state database, honouring the `LOOM_DB` override.
///
/// Holds both account metadata and durable session transcripts (ADR-008).
pub fn db_path() -> PathBuf {
    std::env::var_os("LOOM_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("loom.db"))
}
