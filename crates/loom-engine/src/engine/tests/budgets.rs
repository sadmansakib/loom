//! Turn-budget grants (item 26), the inert default, and prune gating.

use super::*;

/// One script: an assistant turn that writes one file — a mutating
/// call, the progress evidence item 26 reads.
fn write_file_script(path: &str, content: &str) -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "write_file".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "path": path,
            "content": content,
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

/// One script: a non-mutating `run_command` turn.
fn echo_command_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "run_command".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "program": "echo",
            "args": ["hi"],
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

#[tokio::test]
async fn a_mutation_at_the_grant_limit_extends_the_turn_budget() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    config.max_turns = 3;
    config.turn_budget = TurnBudget {
        initial_turns: 1,
        extension_turns: 2,
        max_extensions: 1,
    };
    // The bare loop keeps the completion gate out of the picture:
    // this test pins the budget gate alone.
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                write_file_script("note.txt", "hello"),
                FakeProvider::end_turn_script("done"),
            ]))),
            bare_loop: true,
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let summary = engine
        .run_task(&mut session, "Write a note.", &sink, &Default::default())
        .await
        .expect("agent task");

    assert_eq!(
        summary.model_requests, 2,
        "the grant funded the second turn"
    );
    let kinds = drain(&mut receiver);
    assert!(kinds.contains(&EventKind::TurnBudgetExtended {
        turn: 2,
        reason: loom_kernel::event::TurnBudgetReason::MutationAtLimit,
        new_limit: 3,
    }));
}

#[tokio::test]
async fn an_idle_run_at_the_grant_limit_stops() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    config.max_turns = 3;
    config.turn_budget = TurnBudget {
        initial_turns: 1,
        extension_turns: 2,
        max_extensions: 5,
    };
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(
                vec![echo_command_script()],
            ))),
            bare_loop: true,
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let error = engine
        .run_task(&mut session, "Say hi.", &sink, &Default::default())
        .await
        .expect_err("no evidence, no extension");
    let TaskError::MaxTurns { max_turns } = error else {
        panic!("expected MaxTurns, got {error:?}");
    };
    assert_eq!(max_turns, 1);
    let kinds = drain(&mut receiver);
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::TurnBudgetExtended { .. }))
    );
}

#[tokio::test]
async fn the_default_turn_budget_is_inert() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                echo_command_script(),
                FakeProvider::end_turn_script("done"),
            ]))),
            bare_loop: true,
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let summary = engine
        .run_task(&mut session, "Say hi.", &sink, &Default::default())
        .await
        .expect("agent task");
    assert_eq!(summary.model_requests, 2);
    let kinds = drain(&mut receiver);
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::TurnBudgetExtended { .. }))
    );
}

/// The provider gate for reasoning elision (roadmap item 12): on
/// for GLM (drops only a cache benefit) and Anthropic (never
/// forwards reasoning on replay), off for the OpenAI-vocabulary
/// backends where a tool-bearing request must echo
/// `reasoning_content` verbatim (DeepSeek 400s otherwise).
#[test]
fn prune_config_gates_reasoning_elision_by_provider() {
    let glm = prune_config_for(loom_providers::GLM_PROVIDER_NAME);
    assert_eq!(glm.elide_reasoning, Some(8));

    let anthropic = prune_config_for(loom_providers::ANTHROPIC_PROVIDER_NAME);
    assert_eq!(anthropic.elide_reasoning, Some(8));

    let deepseek = prune_config_for(loom_providers::DEEPSEEK_PROVIDER_NAME);
    assert_eq!(
        deepseek.elide_reasoning, None,
        "DeepSeek rejects replay without reasoning_content"
    );

    let openrouter = prune_config_for(loom_providers::OPENROUTER_PROVIDER_NAME);
    assert_eq!(
        openrouter.elide_reasoning, None,
        "unverified OpenAI-wire behavior stays off"
    );

    // Everything else keeps the kernel defaults.
    assert_eq!(glm.elide_turns, PruneConfig::default().elide_turns);
    assert_eq!(glm.trigger_percent, PruneConfig::default().trigger_percent);
}
