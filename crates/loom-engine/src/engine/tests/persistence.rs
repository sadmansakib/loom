//! Persistence: event streams, session leasing, capability notices.

use super::*;

use loom_kernel::store::EventStore;

#[tokio::test]
async fn a_persisted_run_also_persists_its_event_stream() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.persist = true;
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("done"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let summary = engine
        .run_task(
            &mut session,
            "Say hello and stop.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("task");
    assert_eq!(summary.model_requests, 1);
    let _ = drain(&mut receiver);

    // The durable stream carries the same run: task lifecycle,
    // authoritative completion, and the engine's idle marker, in
    // emission order with contiguous per-run sequences.
    let store = loom_store::SqliteStore::open(base.path().join("loom.db")).expect("store");
    let events = store.load_events(&session.id).expect("events");
    assert!(events.len() >= 4, "expected a full stream, got {events:?}");
    // EventKind is #[non_exhaustive]; unlisted kinds are filler here
    #[allow(clippy::wildcard_enum_match_arm)]
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match &event.kind {
            EventKind::TaskStarted => "task_started",
            EventKind::ModelCompleted { .. } => "model_completed",
            EventKind::TaskCompleted => "task_completed",
            EventKind::AwaitingInput { .. } => "awaiting_input",
            EventKind::FastPathDeclined { .. } => "fast_path_declined",
            EventKind::CapabilitiesResolved { .. } => "capabilities_resolved",
            EventKind::HarnessNotice { .. } => "harness_notice",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds.first(), Some(&"task_started"));
    assert!(kinds.contains(&"model_completed"));
    assert!(kinds.contains(&"task_completed"));
    assert_eq!(kinds.last(), Some(&"awaiting_input"));
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, index as u64, "one run numbers contiguously");
    }
}

#[tokio::test]
async fn a_second_writer_cannot_drive_a_leased_session() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.persist = true;
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("done"),
                FakeProvider::end_turn_script("done again"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    engine
        .run_task(
            &mut session,
            "Say hello.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("first task");

    // A foreign writer holds the session's lease (ADR-008): the
    // next run must refuse before persisting anything.
    let store = loom_store::SqliteStore::open(base.path().join("loom.db")).expect("store");
    let lease = store
        .acquire_lease(&session.id, "another-process", 60_000)
        .expect("lease")
        .expect("granted");
    assert_eq!(lease.owner(), "another-process");

    let continuation = engine
        .continue_session(
            &session.id,
            "Interloper.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("continue returns the outcome, not an error");
    let error = continuation.outcome.expect_err("leased session refuses");
    // TaskError is #[non_exhaustive]; any other variant is a test failure here
    #[allow(clippy::wildcard_enum_match_arm)]
    match &error {
        loom_kernel::agent::TaskError::SessionLeased { owner, .. } => {
            assert_eq!(owner, "another-process");
        }
        other => panic!("expected SessionLeased, got {other:?}"),
    }

    // The refused run persisted nothing: the transcript tip is
    // still the first task's.
    let reloaded = engine
        .load_session(&session.id)
        .expect("load")
        .expect("known");
    assert_eq!(
        reloaded.transcript.len(),
        session.transcript.len(),
        "no items from the refused run"
    );

    // Released, the session accepts input again.
    assert!(
        store
            .release_lease(&session.id, "another-process")
            .expect("release")
    );
    engine
        .continue_session(
            &session.id,
            "Back for real.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("continue")
        .outcome
        .expect("second task after release");
}

#[tokio::test]
async fn capability_resolution_surfaces_once_per_engine() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("done"),
                FakeProvider::end_turn_script("done again"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    engine
        .run_task(
            &mut session,
            "Say hello.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("first task");
    engine
        .run_task(
            &mut session,
            "Say it again.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("second task");

    let kinds = drain(&mut receiver);
    let resolutions = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::CapabilitiesResolved { .. }))
        .count();
    assert_eq!(
        resolutions, 1,
        "one engine, one capability resolution — not one per task"
    );
}

#[tokio::test]
async fn continue_session_resumes_a_persisted_session() {
    let engine = fake_engine(FakeProvider::scripted(vec![
        FakeProvider::end_turn_script("first"),
        FakeProvider::end_turn_script("second"),
    ]));
    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();

    engine
        .run_task(
            &mut session,
            "do the first thing",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("first task");

    // Resume through the first-class flow: the id alone must suffice.
    let continuation = engine
        .continue_session(
            &session.id,
            "now the second",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("continue");

    let summary = continuation.outcome.expect("second task completed");
    assert_eq!(summary.turns, 1);
    assert_eq!(continuation.session.tasks.len(), 2);
    // The store is the authority: reloading the id sees both tasks.
    let reloaded = engine
        .load_session(&session.id)
        .expect("load")
        .expect("persisted");
    assert_eq!(reloaded.tasks.len(), 2);

    // Idle markers: one per task end, each naming the session, always
    // after the terminal event.
    let kinds = drain(&mut receiver);
    let idle: Vec<_> = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::AwaitingInput { .. }))
        .collect();
    assert_eq!(idle.len(), 2, "kinds: {kinds:?}");
    assert!(matches!(
        kinds.last(),
        Some(EventKind::AwaitingInput { session_id }) if *session_id == session.id
    ));
}

#[tokio::test]
async fn recorder_observes_the_engine_lifecycle_through_a_fanout() {
    // B5's consumer shape: fan a StatusRecorder clone out next to a
    // rendering channel; polling the retained handle must see the
    // task run and the engine's idle marker flip it back to Idle.
    let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
        "done",
    )]));
    let recorder = loom_kernel::event::StatusRecorder::new();
    let (channel, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let sink = loom_kernel::event::FanoutSink::new()
        .with(recorder.clone())
        .with(channel);
    let mut session = engine.new_session();

    engine
        .run_task(
            &mut session,
            "anything",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("task completes");

    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.phase, loom_kernel::event::TaskPhase::Idle);
    assert_eq!(snapshot.session_id, Some(session.id));
    assert_eq!(
        snapshot.task_id,
        session.tasks.last().map(|record| record.id),
        "the idle marker's envelope names the ended task"
    );
    assert_eq!(snapshot.turn, Some(1));
    assert_eq!(snapshot.attempt, None);
    assert_eq!(snapshot.in_flight_tools, Vec::<String>::new());
}

#[tokio::test]
async fn continue_session_reports_unknown_ids() {
    let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
        "unused",
    )]));
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let error = engine
        .continue_session(
            &SessionId::new(),
            "anything",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::SessionNotFound { .. }));
}

#[tokio::test]
async fn continue_session_returns_the_advanced_session_on_task_failure() {
    // First task fine, second task a hard model failure: the
    // continuation still carries the advanced session.
    let engine = fake_engine(FakeProvider::end_turn_then_error(
        "first",
        ModelError::new(ModelErrorKind::Auth, "key rejected"),
    ));
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    engine
        .run_task(&mut session, "first", &sink, &crate::TaskControl::default())
        .await
        .expect("first task");

    let continuation = engine
        .continue_session(&session.id, "second", &sink, &crate::TaskControl::default())
        .await
        .expect("continuation is not a load error");

    assert!(continuation.outcome.is_err(), "task failed");
    assert_eq!(continuation.session.tasks.len(), 2);
    // The failed task is persisted with its terminal status, so a
    // later continuation starts from honest state.
    assert_eq!(continuation.session.tasks[1].status, TaskStatus::Failed);
    // Idle is still signalled after a failed task; the marker itself
    // is pinned in the resume test above.
}
