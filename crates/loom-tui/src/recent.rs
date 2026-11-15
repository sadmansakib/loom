//! Recent sessions for the idle view's RECENT list.
//!
//! Reads the persisted store with the same calls `loom sessions` uses. A
//! failure yields an empty list: a missing or unreadable store must not
//! stop the TUI from starting.

use std::path::Path;

use loom_core::kernel::session::TaskStatus;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::Item;

/// How many recent sessions the idle view shows.
pub(crate) const RECENT_LIMIT: usize = 3;

/// Longest RECENT title in characters; longer prompts are cut with `…`.
const TITLE_LIMIT: usize = 48;

/// One RECENT row: the first prompt, the last task outcome, and its turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentSession {
    pub(crate) title: String,
    pub(crate) turns: u32,
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
            let title = session.transcript.iter().find_map(|item| {
                if let Item::User { text } = item {
                    Some(title_of(text))
                } else {
                    None
                }
            })?;
            let last = session.tasks.last();
            Some(RecentSession {
                title,
                turns: last.map_or(0, |task| task.turns),
                status: last.map(|task| task.status),
            })
        })
        .take(RECENT_LIMIT)
        .collect()
}

/// First line of a prompt, cut to [`TITLE_LIMIT`] characters.
fn title_of(text: &str) -> String {
    first_line(text, TITLE_LIMIT)
}

/// First line of a text, cut to `limit` characters with `…`.
pub(crate) fn first_line(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= limit {
        return line.to_owned();
    }
    let cut: String = line.chars().take(limit - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_yields_no_recent_sessions() {
        let path = std::env::temp_dir().join("loom-recent-missing-does-not-exist.db");
        assert_eq!(load(&path), Vec::<RecentSession>::new());
    }

    #[test]
    fn title_uses_first_line_and_truncates_long_prompts() {
        assert_eq!(title_of("fix the bug\nmore detail"), "fix the bug");
        let long = "x".repeat(60);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn recent_reads_first_prompt_and_last_task_from_a_real_store() {
        let path = std::env::temp_dir().join(format!("loom-recent-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let store = loom_core::store::SqliteStore::open(&path).unwrap();
            let id = loom_core::kernel::id::SessionId::new();
            store.ensure_session(&id).unwrap();
            let prompt = loom_core::kernel::transcript::Item::User {
                text: "Inspect the retry path\nand report".to_owned(),
            };
            store
                .append_item(
                    &id,
                    loom_core::kernel::transcript::ItemIndex::from(0),
                    &prompt,
                )
                .unwrap();
        }
        let recent = load(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "Inspect the retry path");
        assert_eq!(recent[0].turns, 0);
        assert_eq!(recent[0].status, None);
    }
}
