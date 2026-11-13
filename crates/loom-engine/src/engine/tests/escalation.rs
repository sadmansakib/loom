//! Stall escalation by fork (item 36) and the no-model default.

use super::*;

/// One script: a read-only tool call that never mutates the
/// workspace (item 36's stall shape — progress-less turns).
fn read_only_turn_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("ro-{}", std::process::id())).unwrap(),
        name: "read_file".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "path": "Cargo.toml",
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

fn escalation_agent() -> Agent {
    Agent::new(
        Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
            "the escalation model recovered",
        )])),
        Box::new(DefaultContextCompiler::default()),
        loom_kernel::tool::ToolRegistry::new(),
        AgentConfig::new(PathBuf::from("."), "test escalation"),
        "escalation-model",
    )
}

/// Item 36: a task that exhausts its turns after a loop nudge is
/// retried on a fork with the escalation model; the original
/// session keeps its stalled transcript and the fork carries the
/// recovery.
#[tokio::test]
async fn stalled_task_forks_and_escalates_on_a_second_model() {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("Cargo.toml"), "[workspace]\n").expect("fixture");
    let mut config = test_config();
    config.workspace = base.path().to_path_buf();
    config.db_path = base.path().join("loom.db");
    config.persist = true;
    config.max_turns = 14; // nudge lands at 12, exhaustion at 14
    let main_scripts: Vec<_> = std::iter::repeat_n(read_only_turn_script(), 14).collect();
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(main_scripts))),
            fast_paths: Some(false),
            stall_escalation: Some(("escalation-model".to_owned(), Arc::new(escalation_agent()))),
            ..Default::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let summary = engine
        .run_task(
            &mut session,
            "churn without changing anything",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("the escalated attempt reports success");

    // The nudge fired, the escalation notice names the fork and
    // the model, and the success came from the escalation agent.
    let kinds = drain(&mut receiver);
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::LoopNudge { .. }))
    );
    // EventKind is #[non_exhaustive]; this probe only wants the escalation notice
    #[allow(clippy::wildcard_enum_match_arm)]
    let notice = kinds
        .iter()
        .find_map(|kind| match kind {
            EventKind::HarnessNotice { category, detail } if category == "escalation" => {
                Some(detail.clone())
            }
            _ => None,
        })
        .expect("escalation notice");
    assert!(notice.starts_with("stalled after nudge; forked to "));
    assert!(notice.ends_with(", retrying with escalation-model"));
    assert!(summary.turns >= 1);

    // Never-worse on the original: the caller's session keeps the
    // stalled transcript — the recovery lives on the fork.
    let last_assistant = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            loom_kernel::transcript::Item::Assistant(message) => Some(message.content.clone()),
            loom_kernel::transcript::Item::User { .. }
            | loom_kernel::transcript::Item::ToolResult(_)
            | _ => None,
        })
        .unwrap_or_default();
    assert_eq!(last_assistant, "");
}

/// Item 36's default-off contract: without an escalation model
/// wired, an exhausted task surfaces its original error — the
/// fork path never engages.
#[tokio::test]
async fn plain_exhaustion_without_nudge_does_not_escalate() {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("Cargo.toml"), "[workspace]\n").expect("fixture");
    let mut config = test_config();
    config.workspace = base.path().to_path_buf();
    config.db_path = base.path().join("loom.db");
    config.persist = true;
    config.max_turns = 3; // below the nudge threshold
    let main_scripts: Vec<_> = std::iter::repeat_n(read_only_turn_script(), 3).collect();
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(main_scripts))),
            fast_paths: Some(false),
            ..Default::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let outcome = engine
        .run_task(
            &mut session,
            "churn without changing anything",
            &sink,
            &crate::TaskControl::default(),
        )
        .await;
    // Never-worse default: the original error surfaces untouched.
    assert!(matches!(outcome, Err(TaskError::MaxTurns { .. })));
    let kinds = drain(&mut receiver);
    assert!(!kinds.iter().any(|kind| matches!(
        kind,
        EventKind::HarnessNotice { category, .. } if category == "escalation"
    )));
}
