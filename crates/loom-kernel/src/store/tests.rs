use super::*;
use crate::id::{ArtifactId, SessionId, TaskId};
use crate::model::StopReason;
use crate::session::{Session, TaskRecord, TaskStatus, TerminalTaskStatus};
use crate::transcript::{
    AssistantMessage, Item, ItemIndex, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};

fn call_id(id: &str) -> ToolCallId {
    ToolCallId::try_from(id.to_owned()).unwrap()
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: call_id(id),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({})).unwrap(),
    }
}

fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        stop_reason: if calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolCalls
        },
        tool_calls: calls,
    }
}

fn result(id: &str) -> ToolResult {
    ToolResult::new(call_id(id), ToolStatus::Success, "contents", false)
}

#[test]
fn append_then_load_round_trips_a_session() {
    let id = SessionId::new();
    let store = MemorySessionStore::new();
    store.ensure_session(&id).unwrap();

    // The in-memory session is the reference; the store must mirror it
    // item for item at the same seams the loop uses.
    let mut session = Session::new(id);

    let user = Item::User { text: "hi".into() };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    let planned = session
        .plan_turn(
            assistant("working", vec![call("call_1", "read_file")]),
            vec![result("call_1")],
        )
        .unwrap();
    store.append_turn(&id, &planned).unwrap();
    session.apply_planned_turn(planned);

    let record = TaskRecord::new(TaskId::new(), 1_000);
    store.upsert_task(&id, &record).unwrap();
    let mut record = record;
    record.finish(TerminalTaskStatus::Completed, 2_000, "end_turn".to_owned());
    store.upsert_task(&id, &record).unwrap();

    let loaded = store.load(&id).unwrap().expect("session");
    assert_eq!(loaded.transcript, session.transcript);
    assert_eq!(loaded.tasks.len(), 1);
    assert_eq!(loaded.tasks[0].status, TaskStatus::Completed);
    assert_eq!(loaded.tasks[0].end_reason.as_deref(), Some("end_turn"));
    assert_eq!(loaded.tasks[0].completed_at_millis, Some(2_000));
    assert_eq!(store.session_ids().unwrap(), vec![id]);
}

#[test]
fn append_item_rejects_a_gapped_index() {
    let store = MemorySessionStore::new();
    let id = SessionId::new();
    store.ensure_session(&id).unwrap();
    let error = store
        .append_item(&id, ItemIndex::from(5), &Item::User { text: "x".into() })
        .unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)));
}

#[test]
fn appending_to_an_unknown_session_is_corrupt() {
    let store = MemorySessionStore::new();
    let id = SessionId::new();
    let error = store
        .append_item(&id, ItemIndex::from(0), &Item::User { text: "x".into() })
        .unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)));
}

#[test]
fn loading_an_unknown_session_is_none() {
    let store = MemorySessionStore::new();
    assert!(store.load(&SessionId::new()).unwrap().is_none());
}

/// A seeded three-item session: user input then one committed turn.
fn seeded_session() -> (SessionId, Session) {
    let id = SessionId::new();
    let mut session = Session::new(id);
    let user = Item::User { text: "fix".into() };
    let index = session.push_item(user).unwrap();
    debug_assert_eq!(index, ItemIndex::from(0));

    let planned = session
        .plan_turn(
            assistant("working", vec![call("call_1", "read_file")]),
            vec![result("call_1")],
        )
        .unwrap();
    session.apply_planned_turn(planned);
    (id, session)
}

#[test]
fn fork_copies_a_prefix_preserves_indices_and_records_lineage() {
    let (id, session) = seeded_session();
    let store = MemorySessionStore::with_session(session);
    let fork_id = SessionId::new();

    // Cut before the assistant turn: keep the user item only.
    let provenance = store.fork(&id, ItemIndex::from(1), &fork_id).unwrap();
    assert_eq!(provenance.forked_from, id);
    assert_eq!(provenance.boundary, ItemIndex::from(1));

    let forked = store.load(&fork_id).unwrap().expect("forked session");
    assert_eq!(forked.transcript.len(), 1);
    // Indices are positions, so the copy keeps them: item 0 is the
    // user item, not a renumbered one.
    assert_eq!(forked.transcript[0], Item::User { text: "fix".into() });
    // Run state stays with the source; the fork is conversation only.
    assert!(forked.tasks.is_empty());
    assert!(forked.todos.is_empty());

    // The source is intact — a fork never truncates.
    let source = store.load(&id).unwrap().expect("source session");
    assert_eq!(source.transcript.len(), 3);
    assert_eq!(store.session_ids().unwrap().len(), 2);

    // Lineage is store-backed and queryable both ways.
    assert_eq!(store.fork_of(&fork_id).unwrap(), Some(provenance));
    assert_eq!(store.fork_of(&id).unwrap(), None);
}

#[test]
fn start_checkpoints_round_trip_replace_and_travel_with_forks() {
    let (id, session) = seeded_session();
    let store = MemorySessionStore::with_session(session);

    // Absent until recorded: pre-feature sessions are tier-0 refusals.
    assert_eq!(store.start_checkpoint(&id).unwrap(), None);

    let dirty = StartCheckpoint {
        workspace: "/tmp/repo".into(),
        head: None,
        reason: Some("workspace was dirty at session start (2 changed paths)".into()),
    };
    store.record_start_checkpoint(&id, &dirty).unwrap();
    assert_eq!(store.start_checkpoint(&id).unwrap(), Some(dirty));

    let clean = StartCheckpoint {
        workspace: "/tmp/repo".into(),
        head: Some("b".repeat(40)),
        reason: None,
    };
    store.record_start_checkpoint(&id, &clean).unwrap();
    assert_eq!(
        store.start_checkpoint(&id).unwrap(),
        Some(clean.clone()),
        "re-recording replaces the baseline"
    );

    // The fork carries the lineage's restore point.
    let fork_id = SessionId::new();
    store.fork(&id, ItemIndex::from(1), &fork_id).unwrap();
    assert_eq!(store.start_checkpoint(&fork_id).unwrap(), Some(clean));
}

#[test]
fn fork_refuses_unknown_source_past_tip_and_duplicate_id() {
    let (id, session) = seeded_session();
    let store = MemorySessionStore::with_session(session);

    let stranger = SessionId::new();
    assert!(matches!(
        store.fork(&stranger, ItemIndex::from(0), &SessionId::new()),
        Err(StoreError::Corrupt(_))
    ));

    // The tip is 3; boundary 4 is past it.
    let past_tip = SessionId::new();
    let error = store.fork(&id, ItemIndex::from(4), &past_tip).unwrap_err();
    assert!(
        error.to_string().contains("past the transcript tip"),
        "wrong error: {error}"
    );

    // A fork id that already exists would collide with live data.
    let duplicate = SessionId::new();
    store.ensure_session(&duplicate).unwrap();
    assert!(matches!(
        store.fork(&id, ItemIndex::from(0), &duplicate),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn memory_event_store_round_trips_in_write_order() {
    use crate::event::{EventEnvelope, EventKind};
    use crate::id::TaskId;

    let id = SessionId::new();
    let store = MemoryEventStore::with_session(id);
    assert!(
        store.load_events(&id).unwrap().is_empty(),
        "a session without events is empty, not an error"
    );

    let task = TaskId::new();
    let envelope = |sequence, kind| EventEnvelope {
        schema_version: crate::event::EVENT_SCHEMA_VERSION,
        sequence,
        task_id: task,
        kind,
    };
    // Two runs share the session: sequences restart, write order wins.
    store
        .append_event(&id, &envelope(0, EventKind::TaskStarted))
        .unwrap();
    store
        .append_event(&id, &envelope(0, EventKind::TaskStarted))
        .unwrap();
    store
        .append_event(&id, &envelope(1, EventKind::TaskCompleted))
        .unwrap();

    let loaded = store.load_events(&id).unwrap();
    assert_eq!(loaded.len(), 3);
    let sequences: Vec<u64> = loaded.iter().map(|event| event.sequence).collect();
    assert_eq!(
        sequences,
        vec![0, 0, 1],
        "insertion order, not sequence order"
    );
    assert_eq!(loaded[0].task_id, task);
    assert_eq!(loaded[2].kind, EventKind::TaskCompleted);
}

#[test]
fn memory_event_store_rejects_an_unknown_session() {
    use crate::event::{EventEnvelope, EventKind};
    use crate::id::TaskId;

    let store = MemoryEventStore::new();
    let error = store
        .append_event(
            &SessionId::new(),
            &EventEnvelope {
                schema_version: crate::event::EVENT_SCHEMA_VERSION,
                sequence: 0,
                task_id: TaskId::new(),
                kind: EventKind::TaskStarted,
            },
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)));
}

#[test]
fn artifact_store_round_trips_and_finds_by_hash() {
    let store = MemoryArtifactStore::new();
    let id = ArtifactId::new();
    let sha = crate::artifact::sha256_hex(b"hello");
    let artifact = StoredArtifact::new(id, "read_file", sha.clone(), b"hello".to_vec());
    assert_eq!(artifact.bytes, 5);
    store.put(&artifact).unwrap();

    assert_eq!(store.get(&id).unwrap(), Some(artifact.clone()));
    assert_eq!(store.find_by_sha256(&sha).unwrap(), Some(artifact));
    assert_eq!(store.find_by_sha256("missing").unwrap(), None);
    assert_eq!(store.get(&ArtifactId::new()).unwrap(), None);
}

#[test]
fn a_drifted_artifact_body_refuses_instead_of_returning() {
    // Review S2: `put` trusts the caller's sha (the kernel computes
    // it); `get` verifies it against the body before anything
    // flows back out — a body swapped out from under its record
    // refuses rather than feeds a restore.
    let store = MemoryArtifactStore::new();
    let id = ArtifactId::new();
    let sha = crate::artifact::sha256_hex(b"original");
    store
        .put(&StoredArtifact::new(
            id,
            "read_file",
            sha.clone(),
            b"original".to_vec(),
        ))
        .unwrap();
    store
        .put(&StoredArtifact::new(
            id,
            "read_file",
            sha.clone(),
            b"tampered".to_vec(),
        ))
        .unwrap();
    let error = store.get(&id).unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
    let error = store.find_by_sha256(&sha).unwrap_err();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
}
