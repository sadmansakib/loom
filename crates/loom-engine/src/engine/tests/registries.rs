//! The full vs. bare registry contract (roadmap item 8 phase B).

use super::*;

/// Roadmap item 8 phase B: the kernel-owned `todo` tool is a
/// full-registry lever. The full engine exposes it; the bare-loop
/// arm — the basic-harness baseline — must keep excluding it
/// (bare = read/list/write/edit/shell).
#[test]
fn todo_tool_is_exposed_in_the_full_registry_and_excluded_from_bare() {
    let built = build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
        None,
    )
    .expect("built-in registry");
    let full: Vec<&str> = built.specs().map(|spec| spec.name.as_str()).collect();
    assert!(
        full.contains(&"todo"),
        "the full registry must expose the kernel todo tool: {full:?}"
    );

    let bare_registry = bare_registry(built);
    let bare: Vec<&str> = bare_registry
        .specs()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(!bare.contains(&"todo"), "the bare arm must not: {bare:?}");
    assert_eq!(
        bare,
        [
            "read_file",
            "list_dir",
            "write_file",
            "edit_file",
            "run_command"
        ],
        "the bare arm is exactly the five primitives, in order"
    );
}

/// Roadmap item 40: the read-only profile is enforcement, not a label.
/// It keeps exactly the read-only tools and drops every mutating one.
#[test]
fn read_only_registry_drops_every_mutating_tool() {
    let built = build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
        None,
    )
    .expect("built-in registry");
    let registry = read_only_registry(built);
    let names: Vec<&str> = registry.specs().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        ["read_file", "list_dir", "search", "artifact_read"],
        "the read-only profile exposes only read-only built-ins"
    );
    for name in ["write_file", "edit_file", "run_command", "verify", "todo"] {
        assert!(
            !names.contains(&name),
            "the read-only profile must drop {name}: {names:?}"
        );
    }
}

/// The single-build A/B seam: a denied tool is absent and everything
/// else is untouched, so the contrast arm differs by exactly one tool.
#[test]
fn without_tools_removes_only_the_named_tool() {
    let built = build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
        None,
    )
    .expect("built-in registry");
    let all: Vec<String> = built.specs().map(|spec| spec.name.clone()).collect();
    assert!(all.iter().any(|name| name == "replace_all"), "{all:?}");

    let registry = super::super::assembly::without_tools(
        built,
        &["replace_all".to_owned(), "nope".to_owned()],
    );
    let names: Vec<String> = registry.specs().map(|spec| spec.name.clone()).collect();
    assert!(!names.iter().any(|name| name == "replace_all"), "{names:?}");
    // One name removed; the unknown one is ignored.
    assert_eq!(names.len(), all.len() - 1, "{names:?}");
    assert_eq!(
        names.iter().filter(|name| *name == "edit_file").count(),
        1,
        "the codemod tool's neighbour survives: {names:?}"
    );
}

/// The seam end to end: with `--deny-tool replace_all` the same
/// scripted call is refused as unknown and the file is untouched; the
/// same binary without the denial rewrites it. This is the arm pair a
/// single-build sweep runs.
#[tokio::test]
async fn a_denied_tool_refuses_where_the_same_build_would_rewrite() {
    async fn run(denied: bool) -> (Vec<EventKind>, String) {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let file = workspace.join("a.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("seed");
        let mut config = test_config();
        config.workspace = workspace;
        config.db_path = base.path().join("loom.db");
        config.max_turns = 2;
        let call = {
            use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
            ToolCall {
                id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
                name: "replace_all".to_owned(),
                arguments: ToolArguments::try_from(serde_json::json!({
                    "pattern": "alpha",
                    "replacement": "omega",
                }))
                .expect("arguments"),
            }
        };
        let provider = Box::new(FakeProvider::scripted(vec![
            vec![Ok(ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: String::new(),
                    reasoning: None,
                    tool_calls: vec![call],
                    stop_reason: StopReason::ToolCalls,
                },
                usage: Usage::default(),
            }))],
            FakeProvider::end_turn_script("done"),
        ]));
        let overrides = EngineOverrides::default().with_provider(provider);
        let overrides = if denied {
            overrides.without_tools(["replace_all".to_owned()])
        } else {
            overrides
        };
        let engine = Engine::open_with(config, overrides).expect("engine");
        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let _ = engine
            .run_task(
                &mut session,
                "rename alpha",
                &sink,
                &crate::TaskControl::default(),
            )
            .await;
        (
            drain(&mut receiver),
            std::fs::read_to_string(&file).expect("read"),
        )
    }

    let (kinds, body) = run(true).await;
    let refused = kinds.iter().any(|kind| {
        matches!(
            kind,
            EventKind::ToolCompleted { name, content_head, .. }
                if name == "replace_all" && content_head.contains("unknown tool")
        )
    });
    assert!(refused, "a denied tool must answer as unknown");
    assert_eq!(body, "fn alpha() {}\n", "nothing was written");

    let (_kinds, body) = run(false).await;
    assert_eq!(body, "fn omega() {}\n", "the same build rewrites it");
}
