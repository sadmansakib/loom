//! Integration tests for the SQLite store (ADR-008).
//!
//! Each test drives the store through the kernel traits exactly as the agent
//! loop does: persist the planned turn *before* applying it, then load the
//! session back and assert it round-trips.

// Integration tests assert failure paths directly, so unwrapping results and
// panicking on unexpected outcomes is the intended style here.
// Integration tests sit outside Clippy.toml's allow-*-in-tests reach
// (rust-clippy#13981); unwrap/expect/panic are correct test behavior.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use loom_kernel::artifact::sha256_hex;
use loom_kernel::event::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind};
use loom_kernel::id::{ArtifactId, SessionId, TaskId};
use loom_kernel::model::StopReason;
use loom_kernel::session::{Session, TaskRecord, TerminalTaskStatus};
use loom_kernel::store::{
    ArtifactStore, EventStore, SessionStore, StartCheckpoint, StoreError, StoredArtifact,
};
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
        .find_map(|item| {
            if let Item::ToolResult(result) = item {
                Some(result)
            } else {
                None
            }
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
    // The recorded sha must be the body's real content hash: reads
    // verify it (review S2).
    let sha = sha256_hex(b"body");
    let artifact = StoredArtifact::new(id, "tool_output", sha.clone(), b"body".to_vec());
    store.put(&artifact).unwrap();

    assert_eq!(store.get(&id).unwrap(), Some(artifact.clone()));
    assert_eq!(store.find_by_sha256(&sha).unwrap(), Some(artifact));
    assert!(store.get(&ArtifactId::new()).unwrap().is_none());
    assert!(store.find_by_sha256("missing").unwrap().is_none());
}

#[test]
fn a_drifted_artifact_body_refuses_instead_of_returning() {
    // Review S2: the sha column is metadata; the body is what a rewind
    // restore writes. A blob that drifted from its content address
    // (bit rot, a hand edit through the backend) must refuse, not
    // round-trip.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let store = SqliteStore::open(&db).unwrap();
    let id = ArtifactId::new();
    let sha = sha256_hex(b"original");
    store
        .put(&StoredArtifact::new(
            id,
            "pre_image",
            sha.clone(),
            b"original".to_vec(),
        ))
        .unwrap();
    assert_eq!(store.get(&id).unwrap().unwrap().body, b"original".to_vec());

    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE artifacts SET body = ?1 WHERE id = ?2",
            rusqlite::params![b"tampered".to_vec(), id.to_string()],
        )
        .unwrap();

    let error = store.get(&id).unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
    let error = store.find_by_sha256(&sha).unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
}

#[test]
fn artifact_listing_is_oldest_first_and_delete_removes_only_that_body() {
    let store = SqliteStore::open_in_memory().unwrap();
    let older = StoredArtifact::new(
        ArtifactId::new(),
        "tool_output",
        sha256_hex(b"older"),
        b"older".to_vec(),
    );
    let newer = StoredArtifact::new(
        ArtifactId::new(),
        "tool_output",
        sha256_hex(b"newer!"),
        b"newer!".to_vec(),
    );
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

    let Err(error) = SqliteStore::open(&path) else {
        panic!("opening a newer schema must fail")
    };
    let StoreError::Corrupt(message) = error else {
        panic!("expected a corrupt-schema error, got {error:?}")
    };
    assert!(message.contains("newer"), "{message}");
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

/// Review S7: an envelope this build cannot parse used to surface as
/// an opaque unknown-variant Corrupt error. When the recorded schema
/// version is newer, the message must name the version gap — the
/// session belongs to a newer loom, it is not damaged.
#[test]
fn a_newer_schema_stream_names_the_version_not_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loom.db");
    let id = SessionId::new();
    let task = TaskId::new();

    {
        let store = SqliteStore::open(&path).unwrap();
        store.ensure_session(&id).unwrap();
        store
            .append_event(&id, &envelope(0, task, EventKind::TaskStarted))
            .unwrap();
    }
    // A newer loom appends a kind this build has never heard of.
    let future = serde_json::json!({
        "schema_version": EVENT_SCHEMA_VERSION + 1,
        "sequence": 1,
        "task_id": task.to_string(),
        "kind": {"type": "kind_from_the_future", "payload": {}},
    })
    .to_string();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO events (session_id, task_id, body_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.to_string(), task.to_string(), future],
        )
        .unwrap();
    drop(connection);

    let store = SqliteStore::open(&path).unwrap();
    let error = store.load_events(&id).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("newer loom"), "{text}");
    assert!(
        text.contains(&format!("v{}", EVENT_SCHEMA_VERSION + 1)),
        "{text}"
    );
    assert!(
        text.contains(&format!("v{EVENT_SCHEMA_VERSION}")),
        "the message also names what this build understands: {text}"
    );

    // Same session, no version escape hatch: a genuinely malformed
    // row stays a plain corruption error.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "DELETE FROM events WHERE body_json LIKE '%kind_from_the_future%'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events (session_id, task_id, body_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                id.to_string(),
                task.to_string(),
                serde_json::json!({
                    "schema_version": 1,
                    "sequence": 1,
                    "task_id": "x",
                    "kind": "not-an-object",
                })
                .to_string()
            ],
        )
        .unwrap();
    drop(connection);
    let error = store.load_events(&id).unwrap_err();
    assert!(!error.to_string().contains("newer loom"), "{error}");
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
fn fork_copies_a_prefix_keeps_indices_and_records_lineage() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "fix".to_owned(),
    };
    store.append_item(&id, ItemIndex::from(0), &user).unwrap();

    // One committed turn on top: items 1 (assistant) and 2 (result).
    let mut session = Session::new(id);
    session.push_item(user.clone()).unwrap();
    let planned = session
        .plan_turn(
            assistant("working", vec![call("call_1", "read_file")]),
            vec![result("call_1")],
        )
        .unwrap();
    session.apply_planned_turn(planned.clone());
    store.append_turn(&id, &planned).unwrap();
    store
        .upsert_task(&id, &TaskRecord::new(TaskId::new(), 1_000))
        .unwrap();

    // Cut after the assistant item, before its result.
    let fork_id = SessionId::new();
    let provenance = store.fork(&id, ItemIndex::from(2), &fork_id).unwrap();
    assert_eq!(provenance.forked_from, id);
    assert_eq!(provenance.boundary, ItemIndex::from(2));

    let forked = store.load(&fork_id).unwrap().expect("forked session");
    assert_eq!(forked.transcript.len(), 2);
    assert_eq!(forked.transcript[0], user, "indices survive the copy");
    assert!(forked.tasks.is_empty(), "task records stay with the source");
    assert_eq!(store.fork_of(&fork_id).unwrap(), Some(provenance));
    assert_eq!(store.fork_of(&id).unwrap(), None);

    // The source keeps its whole transcript and its tasks.
    let source = store.load(&id).unwrap().expect("source intact");
    assert_eq!(source.transcript.len(), 3);
    assert_eq!(source.tasks.len(), 1);
}

#[test]
fn start_checkpoints_round_trip_and_travel_with_forks() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    store
        .append_item(
            &id,
            ItemIndex::from(0),
            &Item::User {
                text: "hi".to_owned(),
            },
        )
        .unwrap();

    // No row: a session that predates the feature is a tier-0 refusal
    // for `--restore-workspace`, never a guess.
    assert_eq!(store.start_checkpoint(&id).unwrap(), None);

    // A dirty start keeps the refusal reason instead of a head.
    let dirty = StartCheckpoint {
        workspace: "/tmp/repo".into(),
        head: None,
        reason: Some("workspace was dirty at session start (2 changed paths)".to_owned()),
    };
    store.record_start_checkpoint(&id, &dirty).unwrap();
    assert_eq!(store.start_checkpoint(&id).unwrap(), Some(dirty));

    // Re-recording replaces: the honest baseline is whichever run
    // actually started the session.
    let clean = StartCheckpoint {
        workspace: "/tmp/repo".into(),
        head: Some("a".repeat(40)),
        reason: None,
    };
    store.record_start_checkpoint(&id, &clean).unwrap();
    assert_eq!(store.start_checkpoint(&id).unwrap(), Some(clean.clone()));

    // The fork inherits the checkpoint: it describes the workspace the
    // lineage started in, which is the fork's restore point too.
    let fork_id = SessionId::new();
    store.fork(&id, ItemIndex::from(1), &fork_id).unwrap();
    assert_eq!(store.start_checkpoint(&fork_id).unwrap(), Some(clean));
    assert_eq!(
        store.start_checkpoint(&SessionId::new()).unwrap(),
        None,
        "unknown sessions read as absent"
    );
}

#[test]
fn fork_refuses_to_race_a_live_lease_and_refuses_bad_boundaries() {
    let store = SqliteStore::open_in_memory().unwrap();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    store
        .append_item(
            &id,
            ItemIndex::from(0),
            &Item::User {
                text: "hi".to_owned(),
            },
        )
        .unwrap();

    // A live lease means a writer may still be moving the tip.
    let lease = store
        .acquire_lease(&id, "writer", 60_000)
        .unwrap()
        .expect("lease acquired");
    let error = store
        .fork(&id, ItemIndex::from(0), &SessionId::new())
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Leased { .. }),
        "wrong error: {error}"
    );

    // Released: the same fork goes through.
    store.release_lease(&id, lease.owner()).unwrap();
    let fork_id = SessionId::new();
    store.fork(&id, ItemIndex::from(1), &fork_id).unwrap();
    assert_eq!(store.load(&fork_id).unwrap().unwrap().transcript.len(), 1);

    // Boundary past the tip refuses, and a reused id refuses.
    let error = store
        .fork(&id, ItemIndex::from(5), &SessionId::new())
        .unwrap_err();
    assert!(
        error.to_string().contains("past the transcript tip"),
        "wrong error: {error}"
    );
    assert!(matches!(
        store.fork(&id, ItemIndex::from(0), &fork_id),
        Err(StoreError::Corrupt(_))
    ));
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
