//! Git tools (item 19): opt-in status reporting.

use super::*;

fn git_status_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "git_status".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({})).expect("arguments"),
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

fn git_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace)
            .output()
            .expect("git spawns")
    };
    git(&["-c", "init.defaultBranch=main", "init", "--quiet"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(workspace.join("f.txt"), "one\n").expect("file");
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "first"]);
    std::fs::write(workspace.join("f.txt"), "one\ntwo\n").expect("edit");
    workspace
}

async fn run_git_task(config: EngineConfig, overrides: EngineOverrides) -> Vec<EventKind> {
    let engine = Engine::open_with(config, overrides).expect("engine");
    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let _ = engine
        .run_task(
            &mut session,
            "Show the workspace status",
            &sink,
            &crate::TaskControl::default(),
        )
        .await;
    drain(&mut receiver)
}

// EventKind is #[non_exhaustive]; this probe only wants git_status completions
#[allow(clippy::wildcard_enum_match_arm)]
fn git_status_content(kinds: &[EventKind]) -> Option<&str> {
    kinds.iter().find_map(|kind| match kind {
        EventKind::ToolCompleted {
            name,
            status: loom_kernel::transcript::ToolStatus::Success,
            content_head,
            ..
        } if name == "git_status" => Some(content_head.as_str()),
        _ => None,
    })
}

fn read_file_script(path: &str) -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "read_file".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({ "path": path })).expect("arguments"),
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

// EventKind is #[non_exhaustive]; this probe only wants read_file completions
#[allow(clippy::wildcard_enum_match_arm)]
fn read_file_content(kinds: &[EventKind]) -> Option<&str> {
    kinds.iter().find_map(|kind| match kind {
        EventKind::ToolCompleted {
            name,
            status: loom_kernel::transcript::ToolStatus::Success,
            content_head,
            ..
        } if name == "read_file" => Some(content_head.as_str()),
        _ => None,
    })
}

#[tokio::test]
async fn engine_honors_custom_read_budget() {
    // Item 20.2: a sweep shrinks the read window through
    // EngineConfig.budgets; the omission marker proves the custom
    // budget shaped the output, not the 56 KiB default.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(workspace.join("big.txt"), "x".repeat(10_000)).expect("file");
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.budgets = loom_tools::ToolBudgets {
        read_file_head_bytes: 64,
        ..loom_tools::ToolBudgets::default()
    };
    let provider = Box::new(FakeProvider::scripted(vec![
        read_file_script("big.txt"),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_git_task(config, EngineOverrides::default().with_provider(provider)).await;
    let content = read_file_content(&kinds).expect("read_file ran");
    assert!(content.contains("bytes omitted"), "{content}");
    assert!(content.len() < 200, "{content}");
}

#[tokio::test]
async fn git_tools_are_opt_in_and_report_status() {
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = git_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.git = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        git_status_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_git_task(config, EngineOverrides::default().with_provider(provider)).await;
    let content = git_status_content(&kinds).expect("git_status ran successfully when opted in");
    assert!(content.contains("branch: main"), "{content}");
    assert!(
        content.contains("M  f.txt") || content.contains(" M f.txt"),
        "{content}"
    );
}

#[tokio::test]
async fn git_tools_stay_out_of_the_default_and_bare_registries() {
    // Default: the call fails as an unknown tool.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = git_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    let provider = Box::new(FakeProvider::scripted(vec![
        git_status_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_git_task(config, EngineOverrides::default().with_provider(provider)).await;
    assert!(
        git_status_content(&kinds).is_none(),
        "default registry must not expose git_status"
    );

    // Bare arm: even an explicit opt-in is ignored — the arm
    // measures the plain loop.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = git_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.git = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        git_status_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_git_task(
        config,
        EngineOverrides::default()
            .with_provider(provider)
            .bare_loop(),
    )
    .await;
    assert!(
        git_status_content(&kinds).is_none(),
        "the bare arm must never expose git_status"
    );
}
