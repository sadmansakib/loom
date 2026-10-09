//! `loom stats` end to end: the binary reads a store it did not create
//! and reports the same transcript analytics the eval harness computes
//! per run — same function, same numbers, zero tokens.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use loom_core::kernel::artifact::ArtifactRef;
use loom_core::kernel::id::{ArtifactId, SessionId};
use loom_core::kernel::model::StopReason;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolFailure, ToolResult, ToolStatus,
};
use loom_core::store::SqliteStore;
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(args).unwrap(),
    }
}

fn result(id: &str, status: ToolStatus, content: &str) -> ToolResult {
    ToolResult::new(
        loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        status,
        content,
        false,
    )
}

fn turn(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    }
}

/// Mirrors the engine's persist-then-commit flow into the store.
fn commit(
    store: &SqliteStore,
    id: &SessionId,
    session: &mut Session,
    assistant: AssistantMessage,
    results: Vec<ToolResult>,
) {
    let planned = session.plan_turn(assistant, results).unwrap();
    store.append_turn(id, &planned).unwrap();
    session.apply_planned_turn(planned);
}

/// Builds a two-session store: one busy session (success, model-side
/// fault, an oversize spill) and one minimal session.
fn seeded_store(db: &std::path::Path) {
    let store = SqliteStore::open(db).unwrap();

    let busy = SessionId::new();
    let mut session = Session::new(busy);
    store.ensure_session(&busy).unwrap();
    let user = Item::User {
        text: "fix the bug".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&busy, index, &user).unwrap();

    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "reading",
            vec![call("a", "read_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result("a", ToolStatus::Success, "fn main() {}")],
    );
    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "editing",
            vec![call("b", "edit_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result(
            "b",
            ToolStatus::Error(ToolFailure::InvalidInput),
            "bad args",
        )],
    );
    let mut spilled = result("c", ToolStatus::Success, "[artifact: full output omitted]");
    spilled.artifact = Some(ArtifactRef::new(
        ArtifactId::new(),
        "tool_output",
        4_096,
        "cafebabe",
    ));
    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "verifying",
            vec![call("c", "run_command", json!({"program": "cargo"}))],
        ),
        vec![spilled],
    );

    let quiet = SessionId::new();
    let mut session = Session::new(quiet);
    store.ensure_session(&quiet).unwrap();
    commit(
        &store,
        &quiet,
        &mut session,
        turn("done", vec![call("z", "read_file", json!({"path": "x"}))]),
        vec![result("z", ToolStatus::Success, "x")],
    );
}

fn stats(db: &std::path::Path, extra: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .arg("stats")
        .args(extra)
        .env("LOOM_DB", db)
        .output()
        .expect("spawn loom");
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn stats_reports_stored_sessions_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store(&db);

    let (ok, stdout, stderr) = stats(&db, &["--json"]);
    assert!(ok, "loom stats --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = report["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let totals = &report["totals"];
    assert_eq!(totals["turns"], json!(4));
    assert_eq!(totals["calls"], json!(4));
    assert_eq!(totals["failed"], json!(1));
    assert_eq!(totals["malformed_calls"], json!(1));
    assert_eq!(totals["tool_error_rate"], json!(0.25));
    assert_eq!(totals["malformed_rate"], json!(0.25));
    assert_eq!(totals["edit_apply"], json!({"attempts": 1, "applied": 0}));
    assert_eq!(totals["spills"], json!(1));
    // The stub is 31 bytes; the body 4 KiB.
    assert_eq!(totals["spill_saved_bytes"], json!(4_096 - 31));
}

#[test]
fn stats_filters_by_session_and_prints_human_output() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store(&db);

    // A filter that cannot match any v7 id (those open with a hex
    // timestamp): the empty report is a clean success, not an error.
    let (ok, stdout, stderr) = stats(&db, &["--session", "zz-not-a-session", "--json"]);
    assert!(ok, "filter failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["sessions"].as_array().unwrap().len(), 0);

    let (ok, stdout, stderr) = stats(&db, &[]);
    assert!(ok, "human output failed: {stderr}");
    assert!(stdout.contains("sessions in"), "header missing: {stdout}");
    assert!(
        stdout.contains("bytes kept out of context"),
        "spill savings missing: {stdout}"
    );
    assert!(stdout.contains("4 turns"), "totals line missing: {stdout}");
}
