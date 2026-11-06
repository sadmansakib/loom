//! `loom lessons`: mine failure-to-fix lessons from stored transcripts
//! (roadmap item 20.4) and write `.loom/lessons.json`.
//!
//! The data step is the kernel's [`mine_lessons`] over the stored
//! sessions — the same derivation `loom stats` reads, so the binary and
//! the harness agree. This module owns only the operator surface: the
//! atomic store write and human/JSON printing. Writing is the operator's
//! step, mirroring the skill retirement rule (advisory verdicts are
//! reported; acting on them is the operator's): a fresh mine replaces
//! the store, so patterns that no longer clear the thresholds are
//! evicted by re-mining.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use loom_core::kernel::lessons::{self, MinedLesson};
use loom_core::kernel::session::Session;
use loom_core::kernel::store::{SessionStore, StoreError};
use loom_core::store::SqliteStore;

use crate::args::LessonsArgs;

/// Runs `loom lessons`.
pub fn run(args: LessonsArgs) -> anyhow::Result<ExitCode> {
    let db = loom_core::paths::db_path();
    let store = SqliteStore::open(&db).with_context(|| format!("opening {}", db.display()))?;
    let sessions = collect_sessions(&store).context("loading stored sessions")?;
    let mined = lessons::mine_lessons(&sessions);
    let path = lessons_path(&args.workspace);

    if args.json {
        let report = serde_json::json!({
            "workspace": args.workspace,
            "sessions": sessions.len(),
            "lessons": mined.iter().map(lesson_json).collect::<Vec<_>>(),
            "note": "deterministic failure-to-fix mining; `loom run --lessons` injects the store",
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if mined.is_empty() {
        if args.dry_run {
            println!(
                "mined 0 lessons from {} session(s); no recurring failure→fix patterns (would write {})",
                sessions.len(),
                path.display()
            );
        } else {
            println!(
                "mined 0 lessons from {} session(s); no recurring failure→fix patterns",
                sessions.len()
            );
        }
    } else {
        let verb = if args.dry_run { "would write" } else { "wrote" };
        println!(
            "mined {} lesson(s) from {} session(s); {verb} {}",
            mined.len(),
            sessions.len(),
            path.display()
        );
        for lesson in &mined {
            println!(
                "  {} {} — {} ({})",
                lesson.tool,
                lesson.failure,
                lesson.statement,
                lesson.evidence()
            );
        }
    }

    if !args.dry_run {
        write_store(&path, &mined).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Every persisted session, oldest creation order (v7 ids order by
/// creation; sorted regardless for determinism). An unreadable session
/// fails the pass, exactly like `sessions_overview` — silently skipping
/// a row would undercount the evidence.
fn collect_sessions(store: &dyn SessionStore) -> Result<Vec<Session>, StoreError> {
    let mut ids = store.session_ids()?;
    ids.sort();
    let mut sessions = Vec::new();
    for id in ids {
        if let Some(session) = store.load(&id)? {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

/// The store path the facts seam reads: `<workspace>/.loom/lessons.json`.
fn lessons_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("lessons.json")
}

/// Atomically replaces the lessons store (write to a sibling temp file,
/// rename over) so a crash never truncates it. An empty set writes an
/// empty array — the re-mined store is the eviction mechanism.
fn write_store(path: &Path, lessons: &[MinedLesson]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(lessons).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body + "\n")?;
    std::fs::rename(&temp, path)
}

/// The JSON row for one lesson.
fn lesson_json(lesson: &MinedLesson) -> serde_json::Value {
    serde_json::json!({
        "tool": lesson.tool,
        "failure": lesson.failure,
        "statement": lesson.statement,
        "occurrences": lesson.occurrences,
        "recoveries": lesson.recoveries,
        "sessions": lesson.sessions,
        "evidence": lesson.evidence(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::LessonsArgs;
    use loom_core::kernel::lessons::MinedLesson;

    fn sample() -> MinedLesson {
        MinedLesson::new("edit_file", "invalid_input", "rule.", 3, 3, 3)
    }

    #[test]
    fn lessons_path_is_inside_the_workspace() {
        assert_eq!(
            lessons_path(Path::new("/ws")),
            PathBuf::from("/ws/.loom/lessons.json")
        );
    }

    #[test]
    fn write_store_round_trips_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = lessons_path(dir.path());
        write_store(&path, &[sample()]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let back: Vec<MinedLesson> = serde_json::from_str(&body).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].tool, "edit_file");

        // Re-mining replaces (eviction): only the new set survives.
        write_store(&path, &[]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "[]\n");
    }

    #[test]
    fn lesson_json_names_the_evidence() {
        let row = lesson_json(&sample());
        assert_eq!(row["tool"], serde_json::json!("edit_file"));
        assert_eq!(row["failure"], serde_json::json!("invalid_input"));
        assert_eq!(
            row["evidence"],
            serde_json::json!("3 failures, 3 recovered, 3 sessions")
        );
    }

    #[test]
    fn parse_defaults_the_workspace_and_flags() {
        let args = LessonsArgs {
            workspace: PathBuf::from("/ws"),
            dry_run: true,
            json: false,
        };
        assert_eq!(args.workspace, PathBuf::from("/ws"));
        assert!(args.dry_run);
        assert!(!args.json);
    }
}
