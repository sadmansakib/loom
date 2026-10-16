//! `loom sessions`: list persisted sessions from `loom.db`, oldest first.
//!
//! A resume picker's data source (roadmap item 15): one line per session
//! with its id, transcript length, and the status of its last task —
//! enough to choose which session to `loom run --session` into. Reads
//! the SQLite store directly — no engine open, so no provider or
//! credentials are needed to inspect sessions.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::kernel::id::SessionId;
use loom_core::kernel::session::{Session, TaskStatus};
use loom_core::kernel::store::SessionStore;

/// Lists every persisted session. `0` sessions is success, not an error:
/// an empty store is a valid state.
pub fn run() -> anyhow::Result<ExitCode> {
    let store = loom_core::store::SqliteStore::open(loom_core::paths::db_path())
        .context("opening the session store")?;

    let ids: Vec<SessionId> = store.session_ids().context("listing sessions")?;
    if ids.is_empty() {
        println!("no persisted sessions");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<38}  {:>5}  LAST TASK", "ID", "ITEMS");
    for id in ids {
        match store.load(&id) {
            Ok(Some(session)) => print_session(&session),
            Ok(None) => println!("{id:<38}  (unavailable)"),
            Err(error) => eprintln!("warning: session {id}: {error}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_session(session: &Session) {
    let last = session.tasks.last().map_or_else(
        || "(no tasks)".to_owned(),
        |task| {
            let status = match task.status {
                TaskStatus::Completed => "completed",
                TaskStatus::Failed => "failed",
                TaskStatus::Cancelled => "cancelled",
                TaskStatus::Paused => "paused",
                TaskStatus::Running => "running",
                _ => "unknown",
            };
            match &task.end_reason {
                Some(reason) => format!("{status} ({reason})"),
                None => status.to_owned(),
            }
        },
    );
    println!(
        "{:<38}  {:>5}  {last}",
        session.id,
        session.transcript.len()
    );
}
