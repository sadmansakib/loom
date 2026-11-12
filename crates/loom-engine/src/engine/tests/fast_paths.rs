//! Fast-path seams: rename and dependency solvers, fall-through, override.

use super::*;

#[tokio::test]
async fn fast_path_solves_a_rename_with_zero_model_requests() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    // A scripted provider proves the negative: the queue is never
    // drained, and the summary reports zero requests.
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("should never be asked"),
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
            "Rename parse_opts to parse_options",
            &sink,
            &Default::default(),
        )
        .await
        .expect("fast path task");

    assert_eq!(summary.model_requests, 0, "no model calls were needed");
    assert_eq!(summary.turns, 0);
    let kinds = drain(&mut receiver);
    assert!(kinds.contains(&EventKind::FastPathHit {
        kind: "rename".to_owned(),
        files_changed: 1
    }));
    assert!(kinds.contains(&EventKind::TaskCompleted));
    assert_eq!(
        session
            .tasks
            .last()
            .and_then(|task| task.end_reason.as_deref()),
        Some("fast_path")
    );
    let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
    assert!(lib.contains("pub fn parse_options() -> u32"));
    assert!(!lib.contains("parse_opts"));
}

/// A minimal cargo fixture whose manifest pins one dependency the
/// fast path can bump.
fn dependency_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_deps\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ntiny-dep = \"1.0.0\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .expect("main");
    workspace
}

#[tokio::test]
async fn fast_path_solves_a_dependency_update_with_zero_model_requests() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = dependency_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("should never be asked"),
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
            "Update the dependency tiny-dep to 2.0.0.",
            &sink,
            &Default::default(),
        )
        .await
        .expect("fast path task");

    assert_eq!(summary.model_requests, 0, "no model calls were needed");
    assert_eq!(summary.turns, 0);
    let kinds = drain(&mut receiver);
    assert!(kinds.contains(&EventKind::FastPathHit {
        kind: "dependency".to_owned(),
        files_changed: 1
    }));
    assert!(kinds.contains(&EventKind::TaskCompleted));
    let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("tiny-dep = \"2.0.0\""));
    assert!(!manifest.contains("1.0.0"));
}

#[tokio::test]
async fn unmatched_instructions_fall_through_to_the_agent_loop() {
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
            &Default::default(),
        )
        .await
        .expect("agent task");

    assert_eq!(summary.model_requests, 1, "the agent loop ran normally");
    let kinds = drain(&mut receiver);
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::FastPathHit { .. }))
    );
    assert!(kinds.contains(&EventKind::TaskCompleted));
    // The attempted-and-declined side of the fast path is surfaced,
    // not swallowed (v8): no intent matched, the loop took over.
    assert!(kinds.contains(&EventKind::FastPathDeclined {
        reason: "no-intent-match".to_owned()
    }));
    // So is the authority that fixed the fake provider's
    // capabilities: its own table.
    assert!(kinds.contains(&EventKind::CapabilitiesResolved {
        model: "deepseek-flash".to_owned(),
        source: "provider-table".to_owned()
    }));
}

#[tokio::test]
async fn a_solver_decline_names_the_intent() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    // The analyzer matches a rename intent, but the target symbol
    // does not exist: the solver declines, and the decline carries
    // the intent kind rather than a bare no-match.
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
            "Rename missing_symbol to renamed_symbol",
            &sink,
            &Default::default(),
        )
        .await
        .expect("agent task");

    assert_eq!(summary.model_requests, 1, "the loop took over");
    let kinds = drain(&mut receiver);
    assert!(kinds.contains(&EventKind::FastPathDeclined {
        reason: "intent-rename-zero-occurrences".to_owned()
    }));
    // The fixture is untouched — a declined fast path never edits.
    let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
    assert!(lib.contains("pub fn parse_opts() -> u32"));
}

#[tokio::test]
async fn fast_paths_can_be_disabled_by_override() {
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
            ]))),
            fast_paths: Some(false),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let summary = engine
        .run_task(
            &mut session,
            "Rename parse_opts to parse_options",
            &sink,
            &Default::default(),
        )
        .await
        .expect("agent task");

    // The same instruction the fast path would solve for free runs
    // the loop instead: the override is honored.
    assert_eq!(summary.model_requests, 1);
    let kinds = drain(&mut receiver);
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::FastPathHit { .. }))
    );
    // And the workspace was left for the agent to edit.
    let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
    assert!(lib.contains("parse_opts"));
}
