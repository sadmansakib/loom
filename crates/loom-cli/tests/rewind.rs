//! `loom rewind` end to end: the binary reads a store it did not create,
//! lists conversation-valid boundaries, and forks at one — leaving the
//! source intact and the lineage visible in `loom sessions`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use loom_core::kernel::id::SessionId;
use loom_core::kernel::model::StopReason;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use loom_core::store::SqliteStore;
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(args).unwrap(),
    }
}

fn result(id: &str, content: &str) -> ToolResult {
    ToolResult::new(
        ToolCallId::try_from(id.to_owned()).unwrap(),
        ToolStatus::Success,
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

/// Seeds one session: user input, a read turn, an edit turn whose result
/// records a written file. Transcript: 0 user, 1 assistant, 2 result,
/// 3 assistant, 4 result — tip 5, boundaries 0/1/3.
fn seeded_store(db: &std::path::Path) -> SessionId {
    let store = SqliteStore::open(db).unwrap();
    let id = SessionId::new();
    let mut session = Session::new(id);
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "fix the bug".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    commit(
        &store,
        &id,
        &mut session,
        turn(
            "reading",
            vec![call("a", "read_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result("a", "fn main() {}")],
    );
    let mut edit = result("b", "applied");
    edit.effects.files_written = vec!["src/lib.rs".to_owned()];
    commit(
        &store,
        &id,
        &mut session,
        turn(
            "editing",
            vec![call("b", "edit_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![edit],
    );
    id
}

fn loom(db: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(args)
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
fn rewind_lists_boundaries_with_effects() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string()]);
    assert!(ok, "listing failed: {stderr}");
    assert!(stdout.contains("session"), "header missing: {stdout}");
    assert!(stdout.contains("user"), "user row missing: {stdout}");
    assert!(stdout.contains("turn 1"), "turn 1 row missing: {stdout}");
    assert!(stdout.contains("turn 2"), "turn 2 row missing: {stdout}");
    assert!(stdout.contains("tip"), "tip row missing: {stdout}");
    // The turn-1 row drops the edit turn, whose effects name the file.
    assert!(
        stdout.contains("src/lib.rs"),
        "dropped-region effects missing: {stdout}"
    );
    assert!(
        stdout.contains(&format!("fork with: loom rewind {id}")),
        "hint missing: {stdout}"
    );
}

#[test]
fn rewind_refuses_mid_turn_and_unknown_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    // Index 2 is a tool result: cutting there would strand the call.
    let (ok, _stdout, stderr) = loom(&db, &["rewind", &id.to_string(), "--at", "2"]);
    assert!(!ok, "mid-turn fork must fail");
    assert!(
        stderr.contains("not a rewind boundary"),
        "wrong refusal: {stderr}"
    );

    // A well-formed id that was never stored.
    let stranger = SessionId::new().to_string();
    let (ok, _stdout, stderr) = loom(&db, &["rewind", &stranger]);
    assert!(!ok, "unknown session must fail");
    assert!(
        stderr.contains("unknown session"),
        "wrong refusal: {stderr}"
    );
}

#[test]
fn rewind_forks_at_a_boundary_and_sessions_shows_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    // Boundary 3: before the edit turn — keep user + read turn.
    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string(), "--at", "3"]);
    assert!(ok, "fork failed: {stderr}");
    assert!(
        stdout.contains("kept [0, 3), dropped 2 item(s)"),
        "fork summary missing: {stdout}"
    );
    let new_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("forked "))
        .and_then(|rest| rest.split(' ').next())
        .expect("forked id in output")
        .to_owned();
    assert_ne!(new_id, id.to_string());
    assert!(
        stdout.contains(&format!("loom run --session {new_id}")),
        "continuation hint missing: {stdout}"
    );

    // The fork carries lineage; the source row is unchanged and the
    // fork's transcript is the kept prefix.
    let (ok, stdout, stderr) = loom(&db, &["sessions"]);
    assert!(ok, "sessions failed: {stderr}");
    assert!(
        stdout.contains(&format!("[fork of {id} @ 3]")),
        "lineage marker missing: {stdout}"
    );

    let store = SqliteStore::open(&db).unwrap();
    let forked: SessionId = new_id.parse().unwrap();
    let forked = store.load(&forked).unwrap().expect("forked session");
    assert_eq!(forked.transcript.len(), 3);
    let source = store.load(&id).unwrap().expect("source intact");
    assert_eq!(source.transcript.len(), 5);
}
