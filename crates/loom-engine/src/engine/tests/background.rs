//! Background commands steering the next task (item 13).

use super::*;

/// One script: an assistant turn that detaches one background job.
fn run_background_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("bg-{}", std::process::id())).unwrap(),
        name: "run_background".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "program": "echo",
            "args": ["detached hi"],
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

/// One script: an assistant turn that asks `command_status` for the
/// background-job list.
fn command_status_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("st-{}", std::process::id())).unwrap(),
        name: "command_status".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "action": "list",
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
async fn background_completions_steer_into_the_next_task() {
    // Item 13, end-to-end offline: with the config flag on the
    // registry grows `run_background`/`command_status`, a detached
    // job does not block the task that spawned it, and its
    // completion notice — still queued when that task ends — is
    // steered into the opening turn of the next task instead of
    // being lost.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = base.path().to_path_buf();
    config.db_path = base.path().join("loom.db");
    config.background_commands = true;
    config.max_turns = 10;
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                run_background_script(),
                FakeProvider::end_turn_script("detached"),
                command_status_script(),
                FakeProvider::end_turn_script("observed"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();

    // Task one detaches the job and ends; the job (echo) finishes
    // after the task, so its notice stays queued.
    engine
        .run_task(
            &mut session,
            "start something long",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("detach task");
    assert!(drain(&mut receiver).iter().any(
        |kind| matches!(kind, EventKind::ToolCompleted { name, .. } if name == "run_background")
    ));
    // Give the detached echo a moment to land in the registry.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Task two opens with the pre-drained notice: the steering
    // queue applies it before turn 1's compile, and the same
    // registry answers `command_status`.
    engine
        .run_task(
            &mut session,
            "check on it",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("status task");
    let kinds = drain(&mut receiver);
    assert!(
        kinds.contains(&EventKind::SteeringApplied {
            turn: 1,
            messages: 1
        }),
        "the completion notice steered into the opening turn: {kinds:?}"
    );
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        EventKind::ToolCompleted { name, .. } if name == "command_status"
    )));
    // The steered notice is a persisted user item — the next
    // frontend sees it in the transcript, not just the live stream.
    assert!(session.transcript.iter().any(|item| matches!(
        item,
        loom_kernel::transcript::Item::User { text }
            if text.contains("[background]") && text.contains("exit status: 0")
    )));
}
