//! Recent sessions for the idle view's RECENT list.
//!
//! Reads the persisted store with the same calls `loom sessions` uses. A
//! failure yields an empty list: a missing or unreadable store must not
//! stop the TUI from starting.

use std::path::Path;

use loom_core::kernel::session::TaskStatus;
use loom_core::kernel::store::SessionStore;

/// How many recent sessions the idle view shows.
pub(crate) const RECENT_LIMIT: usize = 3;

/// One RECENT row: the session's last task outcome and its transcript size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentSession {
    pub(crate) id: String,
    pub(crate) items: usize,
    pub(crate) status: Option<TaskStatus>,
}

/// Loads up to [`RECENT_LIMIT`] sessions, newest first.
///
/// Session ids are v7 UUIDs, so descending id order is newest first.
pub(crate) fn load(db_path: &Path) -> Vec<RecentSession> {
    // `open` creates the file; a first run must not write a store just to show an empty list.
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(store) = loom_core::store::SqliteStore::open(db_path) else {
        return Vec::new();
    };
    let Ok(mut ids) = store.session_ids() else {
        return Vec::new();
    };
    ids.reverse();
    ids.into_iter()
        .filter_map(|id| {
            let session = store.load(&id).ok().flatten()?;
            Some(RecentSession {
                id: id.to_string(),
                items: session.transcript.len(),
                status: session.tasks.last().map(|task| task.status),
            })
        })
        .take(RECENT_LIMIT)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_yields_no_recent_sessions() {
        let path = std::env::temp_dir().join("loom-recent-missing-does-not-exist.db");
        assert_eq!(load(&path), Vec::<RecentSession>::new());
    }
}
