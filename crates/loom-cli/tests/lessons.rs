//! `loom lessons` end to end (roadmap item 20.4): the binary mines
//! failure-to-fix patterns from a store it did not create and writes the
//! `.loom/lessons.json` store `loom run --lessons` reads. Zero tokens.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use loom_core::kernel::id::SessionId;
use loom_core::kernel::model::StopReason;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::{
    AssistantMessage, ToolArguments, ToolCall, ToolFailure, ToolResult, ToolStatus,
};
use loom_core::store::SqliteStore;
use serde_json::json;

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(json!({})).unwrap(),
    }
}

fn result(id: &str, status: ToolStatus) -> ToolResult {
    ToolResult::new(
        loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        status,
        "x",
        false,
    )
}

fn commit(
    store: &SqliteStore,
    id: &SessionId,
    session: &mut Session,
    name: &str,
    statuses: Vec<ToolStatus>,
) {
    for (index, status) in statuses.into_iter().enumerate() {
        let call_id = format!("c{index}");
        let planned = session
            .plan_turn(
                AssistantMessage {
                    content: String::new(),
                    reasoning: None,
                    tool_calls: vec![call(&call_id, name)],
                    stop_reason: StopReason::ToolCalls,
                },
                vec![result(&call_id, status)],
            )
            .unwrap();
        store.append_turn(id, &planned).unwrap();
        session.apply_planned_turn(planned);
    }
}

/// Three sessions, each with one invalid-input edit_file failure
/// recovered by a same-tool success — the minimal qualifying pattern.
fn qualifying_store(db: &std::path::Path) {
    let store = SqliteStore::open(db).unwrap();
    for _ in 0..3 {
        let id = SessionId::new();
        let mut session = Session::new(id);
        store.ensure_session(&id).unwrap();
        commit(
            &store,
            &id,
            &mut session,
            "edit_file",
            vec![
                ToolStatus::Error(ToolFailure::InvalidInput),
                ToolStatus::Success,
            ],
        );
    }
}

fn lessons(
    db: &std::path::Path,
    workspace: &std::path::Path,
    extra: &[&str],
) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom"));
    cmd.arg("lessons")
        .args(extra)
        .arg("--workspace")
        .arg(workspace)
        .env("LOOM_DB", db);
    let output = cmd.output().expect("spawn loom");
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn lessons_mines_and_writes_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    qualifying_store(&db);

    let (ok, stdout, stderr) = lessons(&db, &ws, &["--json"]);
    assert!(ok, "loom lessons --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["sessions"], json!(3));
    let rows = report["lessons"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tool"], json!("edit_file"));
    assert_eq!(rows[0]["failure"], json!("invalid_input"));
    assert_eq!(rows[0]["occurrences"], json!(3));
    assert_eq!(rows[0]["recoveries"], json!(3));
    assert_eq!(
        rows[0]["evidence"],
        json!("3 failures, 3 recovered, 3 sessions")
    );

    // The file the facts seam reads exists and parses identically.
    let store: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(ws.join(".loom/lessons.json")).unwrap())
            .unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store[0]["tool"], json!("edit_file"));
}

#[test]
fn lessons_dry_run_writes_nothing_and_empty_history_evicts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    qualifying_store(&db);

    let (ok, stdout, stderr) = lessons(&db, &ws, &["--dry-run"]);
    assert!(ok, "dry run failed: {stderr}");
    assert!(stdout.contains("would write"), "{stdout}");
    assert!(!ws.join(".loom/lessons.json").exists());

    // An empty store mines nothing and writes the empty array — the
    // eviction mechanism for dead rules.
    let empty_db = dir.path().join("empty.db");
    SqliteStore::open(&empty_db).unwrap();
    let (ok, _, stderr) = lessons(&empty_db, &ws, &[]);
    assert!(ok, "empty mine failed: {stderr}");
    let body = std::fs::read_to_string(ws.join(".loom/lessons.json")).unwrap();
    assert_eq!(body, "[]\n");
}
