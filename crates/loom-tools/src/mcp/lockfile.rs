//! The `.loom/mcp.lock` pin store: the SHA-256 digests a tool
//! definition must keep to stay attached.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::LOCK_RELATIVE_PATH;
use super::McpError;

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// The parsed `.loom/mcp.lock`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct McpLock {
    #[serde(default)]
    pub(super) servers: BTreeMap<String, BTreeMap<String, String>>,
}

pub(super) fn load_lock(root: &Path) -> Result<McpLock, McpError> {
    let path = root.join(LOCK_RELATIVE_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(McpLock::default()),
        Err(error) => {
            return Err(McpError::Lock(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    toml::from_str(&text)
        .map_err(|error| McpError::Lock(format!("cannot parse {}: {error}", path.display())))
}

/// Writes the lock back (regenerated, with the managed header). Best
/// effort: a write failure warns and leaves attach otherwise unaffected —
/// the next run re-pins on first sight.
pub(super) fn write_lock(root: &Path, lock: &McpLock) {
    let path = root.join(LOCK_RELATIVE_PATH);
    let body = toml::to_string_pretty(lock).unwrap_or_default();
    let text = format!(
        "# Managed by loom (docs/mcp-design.md §3). Pins each MCP tool\n\
         # definition (server, name, description, schema) by SHA-256. A\n\
         # mismatch quarantines the tool until its pin is deleted.\n{body}"
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic replace (review item 2.3): a crash mid-write can no
    // longer leave a truncated lockfile that would quarantine every
    // pinned tool on the next attach.
    if let Err(error) = crate::atomic::atomic_write(&path, text.as_bytes()) {
        tracing::warn!(path = %path.display(), %error, "cannot write MCP lockfile");
    }
}
