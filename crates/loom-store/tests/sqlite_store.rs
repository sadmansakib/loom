//! Integration tests for the SQLite store (ADR-008).
//!
//! Each test drives the store through the kernel traits exactly as the agent
//! loop does: persist the planned turn *before* applying it, then load the
//! session back and assert it round-trips.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use loom_kernel::id::{ArtifactId, SessionId, TaskId};
use loom_kernel::model::StopReason;
use loom_kernel::session::{Session, TaskRecord, TerminalTaskStatus};
use loom_kernel::store::{ArtifactStore, SessionStore, StoreError, StoredArtifact};
use loom_kernel::tool::ToolEffects;
use loom_kernel::transcript::{
    AssistantMessage, Item, ItemIndex, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use loom_store::SqliteStore;

fn call_id(id: &str) -> ToolCallId {
    ToolCallId::try_from(id.to_owned()).unwrap()
}

fn arguments() -> ToolArguments {
    ToolArguments::try_from(serde_json::json!({ "path": "src/lib.rs" })).unwrap()
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: call_id(id),
        name: name.to_owned(),
        arguments: arguments(),
    }
}

fn result(id: &str) -> ToolResult {
    ToolResult::new(call_id(id), ToolStatus::Success, "ok", false)
}

fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    let stop_reason = if calls.is_empty() {
        StopReason::EndTurn
    } else {
        StopReason::ToolCalls
    };
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason,
    }
}

#[test]
fn session_round_trips_through_sqlite() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let mut session = Session::new(id);

    // A user item, persisted before the in-memory push.
    let user = Item::User {
        text: "make a change".to_owned(),
    };
    store.append_item(&id, session.next_index(), &user).unwrap();
    session.push_item(user).unwrap();

    // A turn whose tool result carries effects (ADR-012 decision 2).
    let mut result = result("call_1");
    result.effects = ToolEffects::none().with_file_written("src/lib.rs");
    let planned = session
        .plan_turn(
            assistant("calling read_file", vec![call("call_1", "read_file")]),
            vec![result],
        )
        .unwrap();
    store.append_turn(&id, &planned).unwrap();
    session.apply_planned_turn(planned);

    // A task record, upserted running then finished (the second upsert wins).
    let mut task = TaskRecord::new(TaskId::new(), 1_000);
    store.upsert_task(&id, &task).unwrap();
    task.finish(TerminalTaskStatus::Completed, 2_000, "end_turn".to_owned());
    store.upsert_task(&id, &task).unwrap();
    session.push_task(task);

    let loaded = store.load(&id).unwrap().expect("session present");
    assert_eq!(loaded.id, id);
    assert_eq!(loaded.transcript, session.transcript);
    assert_eq!(loaded.tasks, session.tasks);
    assert_eq!(store.session_ids().unwrap(), vec![id]);

    // The effects survived the JSON round-trip.
    let stored = loaded
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("a tool result");
    assert_eq!(stored.effects.files_written, ["src/lib.rs"]);
}

#[test]
fn a_session_survives_reopening_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("loom.db");
    let id = SessionId::new();

    {
        let store = SqliteStore::open(&path).unwrap();
        store.ensure_session(&id).unwrap();
        let user = Item::User {
            text: "hello".to_owned(),
        };
        store.append_item(&id, ItemIndex::from(0), &user).unwrap();
    }

    // Reopening runs the (idempotent) migration and reads the row back.
    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.load(&id).unwrap().expect("session present");
    assert_eq!(loaded.transcript.len(), 1);
    assert_eq!(store.session_ids().unwrap(), vec![id]);
}

#[test]
fn a_duplicate_item_index_is_rejected() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let item = Item::User {
        text: "once".to_owned(),
    };
    store.append_item(&id, ItemIndex::from(0), &item).unwrap();

    let error = store
        .append_item(&id, ItemIndex::from(0), &item)
        .unwrap_err();
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}

#[test]
fn appending_to_an_unknown_session_is_a_backend_failure() {
    let store = SqliteStore::open_in_memory().unwrap();
    let item = Item::User {
        text: "orphan".to_owned(),
    };
    // Foreign keys are on: the items row cannot reference a missing session.
    let error = store
        .append_item(&SessionId::new(), ItemIndex::from(0), &item)
        .unwrap_err();
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}

#[test]
fn loading_an_unknown_session_is_none() {
    let store = SqliteStore::open_in_memory().unwrap();
    assert!(store.load(&SessionId::new()).unwrap().is_none());
}

#[test]
fn a_lease_admits_one_owner_and_can_be_renewed_released_or_stolen() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();

    let lease = store
        .acquire_lease(&id, "owner-a", 10_000)
        .unwrap()
        .expect("first owner acquires");
    assert_eq!(lease.owner(), "owner-a");
    assert_eq!(lease.session(), &id);

    // A second owner is refused while the lease is live, but the current
    // owner may re-acquire it.
    assert!(
        store
            .acquire_lease(&id, "owner-b", 10_000)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .acquire_lease(&id, "owner-a", 10_000)
            .unwrap()
            .is_some()
    );

    assert!(store.renew_lease(&id, "owner-a", 20_000).unwrap());
    assert!(!store.renew_lease(&id, "owner-b", 20_000).unwrap());
    assert!(!store.release_lease(&id, "owner-b").unwrap());
    assert!(store.release_lease(&id, "owner-a").unwrap());

    // A zero TTL expires immediately, so another owner can steal it.
    assert!(store.acquire_lease(&id, "owner-a", 0).unwrap().is_some());
    assert!(
        store
            .acquire_lease(&id, "owner-b", 10_000)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_lease_for_an_unknown_session_is_corrupt() {
    let store = SqliteStore::open_in_memory().unwrap();
    let error = store
        .acquire_lease(&SessionId::new(), "owner-a", 1_000)
        .unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
}

#[test]
fn artifacts_round_trip_and_are_findable_by_hash() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = ArtifactId::new();
    let artifact = StoredArtifact::new(id, "tool_output", "abc123", b"body".to_vec());
    store.put(&artifact).unwrap();

    assert_eq!(store.get(&id).unwrap(), Some(artifact.clone()));
    assert_eq!(store.find_by_sha256("abc123").unwrap(), Some(artifact));
    assert!(store.get(&ArtifactId::new()).unwrap().is_none());
    assert!(store.find_by_sha256("missing").unwrap().is_none());
}

#[test]
fn a_newer_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loom.db");

    // Create the schema, then forge a version this build does not know.
    drop(SqliteStore::open(&path).unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '99') \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
    drop(connection);

    let error = match SqliteStore::open(&path) {
        Ok(_) => panic!("opening a newer schema must fail"),
        Err(error) => error,
    };
    match error {
        StoreError::Corrupt(message) => assert!(message.contains("newer"), "{message}"),
        other => panic!("expected a corrupt-schema error, got {other:?}"),
    }
}
