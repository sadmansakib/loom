//! Integration tests for the SQLite store (ADR-008).
//!
//! Each test drives the store through the kernel traits exactly as the agent
//! loop does: persist the planned turn *before* applying it, then load the
//! session back and assert it round-trips.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use loom_kernel::event::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind};
use loom_kernel::id::{ArtifactId, SessionId, TaskId};
use loom_kernel::model::StopReason;
use loom_kernel::session::{Session, TaskRecord, TerminalTaskStatus};
use loom_kernel::store::{ArtifactStore, EventStore, SessionStore, StoreError, StoredArtifact};
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
fn artifact_listing_is_oldest_first_and_delete_removes_only_that_body() {
    let store = SqliteStore::open_in_memory().unwrap();
    let older = StoredArtifact::new(ArtifactId::new(), "tool_output", "aa", b"older".to_vec());
    let newer = StoredArtifact::new(ArtifactId::new(), "tool_output", "bb", b"newer!".to_vec());
    let older_id = older.id;
    store.put(&older).unwrap();
    // created_at has millisecond resolution, so same-tick writes tie and
    // the secondary key (id ASC) orders them — v7 ids make that insertion
    // order. Assert the pair rather than which specific tiebreak fired.
    std::thread::sleep(std::time::Duration::from_millis(2));
    store.put(&newer).unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, older_id);
    assert_eq!(listed[0].bytes, 5);
    assert_eq!(listed[0].kind, "tool_output");
    assert!(listed[0].created_at <= listed[1].created_at);

    store.delete(&older_id).unwrap();
    assert!(store.get(&older_id).unwrap().is_none());
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(store.get(&newer.id).unwrap().is_some());
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

/// Builds an envelope with a rich payload, proving full-fidelity
/// round-trips (the same data a `--json` trace carries).
fn envelope(sequence: u64, task: TaskId, kind: EventKind) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        sequence,
        task_id: task,
        kind,
    }
}

#[test]
fn events_round_trip_and_survive_reopening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loom.db");
    let id = SessionId::new();
    let task = TaskId::new();

    let stream: Vec<EventEnvelope> = vec![
        envelope(0, task, EventKind::TaskStarted),
        envelope(
            1,
            task,
            EventKind::ModelCompleted {
                response: loom_kernel::model::ModelResponse {
                    message: AssistantMessage {
                        content: "done".to_owned(),
                        reasoning: None,
                        tool_calls: vec![call("call_1", "read_file")],
                        stop_reason: StopReason::ToolCalls,
                    },
                    usage: loom_kernel::model::Usage {
                        input_tokens: 100,
                        output_tokens: 5,
                        cached_input_tokens: Some(3),
                        thinking_tokens: None,
                        cache_creation_input_tokens: None,
                        cost_usd: None,
                    },
                },
            },
        ),
        envelope(2, task, EventKind::TaskCompleted),
    ];

    {
        let store = SqliteStore::open(&path).unwrap();
        store.ensure_session(&id).unwrap();
        for event in &stream {
            store.append_event(&id, event).unwrap();
        }
    }

    // Reopen: the stored stream is identical, in write order.
    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.load_events(&id).unwrap();
    assert_eq!(loaded, stream, "envelopes survive a full serde round-trip");
}

#[test]
fn a_session_without_events_still_loads_and_reads_empty() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "before events existed".to_owned(),
    };
    store.append_item(&id, ItemIndex::from(0), &user).unwrap();

    // The additive seam: no event rows, no change to the session load.
    let loaded = store.load(&id).unwrap().expect("session present");
    assert_eq!(loaded.transcript.len(), 1);
    assert!(store.load_events(&id).unwrap().is_empty());
}

#[test]
fn a_v1_database_migrates_in_place_and_keeps_its_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loom.db");

    // Forge a v1 database the way an older build left it: v1 tables,
    // meta pinned to 1, no `events` table.
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO meta (key, value) VALUES ('schema_version', '1'); \
                 CREATE TABLE sessions (\
                     id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, \
                     lease_owner TEXT, lease_until INTEGER); \
                 CREATE TABLE items (\
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, \
                     idx INTEGER NOT NULL, kind TEXT NOT NULL, body_json TEXT NOT NULL, \
                     PRIMARY KEY (session_id, idx));",
            )
            .unwrap();
        let id = SessionId::new();
        let item = Item::User {
            text: "from the old build".to_owned(),
        };
        connection
            .execute(
                "INSERT INTO sessions (id, created_at) VALUES (?1, 0)",
                [id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO items (session_id, idx, kind, body_json) VALUES (?1, 0, 'user', ?2)",
                rusqlite::params![id.to_string(), serde_json::to_string(&item).unwrap()],
            )
            .unwrap();
    }

    // Opening with this build migrates to v2 additively.
    let store = SqliteStore::open(&path).unwrap();
    let ids = store.session_ids().unwrap();
    assert_eq!(ids.len(), 1);
    let loaded = store.load(&ids[0]).unwrap().expect("old session loads");
    assert_eq!(loaded.transcript.len(), 1, "the v1 rows survived");
    assert!(store.load_events(&ids[0]).unwrap().is_empty());

    // The migrated table accepts events.
    store.ensure_session(&ids[0]).unwrap();
    let task = TaskId::new();
    store
        .append_event(&ids[0], &envelope(0, task, EventKind::TaskStarted))
        .unwrap();
    assert_eq!(store.load_events(&ids[0]).unwrap().len(), 1);
}

#[test]
fn event_rows_keep_insertion_order_across_resumed_runs() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let first = TaskId::new();
    let second = TaskId::new();

    // Two runs share the session; each sink numbers from zero, so the
    // sequences collide and only insertion order disambiguates.
    for task in [first, second] {
        store
            .append_event(&id, &envelope(0, task, EventKind::TaskStarted))
            .unwrap();
        store
            .append_event(&id, &envelope(1, task, EventKind::TaskCompleted))
            .unwrap();
    }

    let loaded = store.load_events(&id).unwrap();
    let tasks: Vec<TaskId> = loaded.iter().map(|event| event.task_id).collect();
    assert_eq!(
        tasks,
        vec![first, first, second, second],
        "write order, not sequence order"
    );
}

#[test]
fn appending_an_event_for_an_unknown_session_is_a_backend_failure() {
    let store = SqliteStore::open_in_memory().unwrap();
    let error = store
        .append_event(
            &SessionId::new(),
            &envelope(0, TaskId::new(), EventKind::TaskStarted),
        )
        .unwrap_err();
    // Foreign keys are on: the events row cannot reference a missing
    // session, exactly like items.
    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}
