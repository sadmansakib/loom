//! Symbol tools (item 18): opt-in overviews and edit-miss outlines.

use super::*;

/// One script: an `edit_file` turn whose `old_string` matches nothing,
/// so the call fails deterministically without touching the file.
fn edit_file_miss_script(path: &str, old: &str) -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "edit_file".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "path": path,
            "old_string": old,
            "new_string": "x",
        }))
        .expect("arguments"),
    };
    vec![Ok(ModelEvent::Completed(ModelResponse {
        message: AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call],
            stop_reason: StopReason::ToolCalls,
        },
        usage: Usage::default(),
    }))]
}

/// Roadmap item 18: the tree-sitter symbol tools are opt-in —
/// `symbols: true` exposes `symbols_overview` and `replace_symbol`;
/// the default (and the bare-loop baseline) keeps them out, so the
/// frozen SWE-bench protocol is unchanged until a frontend opts in.
fn symbols_overview_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "symbols_overview".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "path": "lib.rs",
            "depth": 1,
        }))
        .expect("arguments"),
    };
    vec![Ok(ModelEvent::Completed(ModelResponse {
        message: AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call],
            stop_reason: StopReason::ToolCalls,
        },
        usage: Usage::default(),
    }))]
}

fn symbols_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        workspace.join("lib.rs"),
        "const LIMIT: u32 = 3;\n\nstruct Config {\n    name: String,\n}\n\n\
         impl Config {\n    fn new() -> Self {\n        Self { name: String::new() }\n    }\n}\n",
    )
    .expect("fixture");
    workspace
}

async fn run_symbols_task(config: EngineConfig, overrides: EngineOverrides) -> Vec<EventKind> {
    let engine = Engine::open_with(config, overrides).expect("engine");
    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let _ = engine
        .run_task(&mut session, "List the symbols", &sink, &Default::default())
        .await;
    drain(&mut receiver)
}

fn overview_content(kinds: &[EventKind]) -> Option<&str> {
    kinds.iter().find_map(|kind| match kind {
        EventKind::ToolCompleted {
            name,
            status: loom_kernel::transcript::ToolStatus::Success,
            content_head,
            ..
        } if name == "symbols_overview" => Some(content_head.as_str()),
        _ => None,
    })
}

#[tokio::test]
async fn symbols_tools_are_opt_in_and_serve_overviews() {
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = symbols_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.symbols = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        symbols_overview_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_symbols_task(config, EngineOverrides::default().with_provider(provider)).await;
    let content =
        overview_content(&kinds).expect("symbols_overview ran successfully when opted in");
    assert!(content.contains("function Config/new"), "{content}");
    assert!(content.contains("struct Config"), "{content}");
}

#[tokio::test]
async fn symbols_tools_are_pruned_without_a_served_language() {
    // Item 30: an opted-in flag still prunes the symbol tools when
    // the facts scan finds no Rust/Python/TypeScript — a Go-only
    // workspace never sees them.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::write(workspace.join("main.go"), "package main\n").expect("write");
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.symbols = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        symbols_overview_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_symbols_task(config, EngineOverrides::default().with_provider(provider)).await;
    assert!(
        overview_content(&kinds).is_none(),
        "a workspace with no served language must not expose symbols_overview"
    );
}

#[tokio::test]
async fn two_failed_edits_on_one_file_inject_its_symbol_outline() {
    // Item 30 stall-breaker: two consecutive failed edits on one
    // file answer with that file's symbol outline — deterministic
    // context, no extra model call.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("mkdir");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn target() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.max_turns = 5;
    config.symbols = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        edit_file_miss_script("src/lib.rs", "absent-one"),
        edit_file_miss_script("src/lib.rs", "absent-two"),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_symbols_task(config, EngineOverrides::default().with_provider(provider)).await;
    let nudge = kinds
        .iter()
        .find_map(|kind| match kind {
            EventKind::LoopNudge { text } if text.contains("consecutive edits failed") => {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("stall-breaker nudge fired after two failed edits");
    assert!(nudge.contains("src/lib.rs"), "{nudge}");
    assert!(
        nudge.contains("target"),
        "the outline names the file's symbols: {nudge}"
    );
}

#[tokio::test]
async fn symbols_tools_stay_out_of_the_default_and_bare_registries() {
    // Default: the call fails as an unknown tool.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = symbols_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    let provider = Box::new(FakeProvider::scripted(vec![
        symbols_overview_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_symbols_task(config, EngineOverrides::default().with_provider(provider)).await;
    assert!(
        overview_content(&kinds).is_none(),
        "default registry must not expose symbols_overview"
    );

    // Bare arm: even an explicit opt-in is ignored — the arm
    // measures the plain loop.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = symbols_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.symbols = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        symbols_overview_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_symbols_task(
        config,
        EngineOverrides::default()
            .with_provider(provider)
            .bare_loop(),
    )
    .await;
    assert!(
        overview_content(&kinds).is_none(),
        "the bare arm must never expose symbols_overview"
    );
}
