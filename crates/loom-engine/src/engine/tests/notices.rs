//! Harness-notice dedup within one turn's batch.

use super::*;

/// One script: an assistant turn that asks for two identical
/// `run_command` calls (same program, same arguments).
fn two_identical_commands_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = || ToolCall {
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
            tool_calls: vec![call(), call()],
            stop_reason: StopReason::ToolCalls,
        },
        usage: Usage::default(),
    }))]
}

#[tokio::test]
async fn harness_notices_dedupe_within_a_batch() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    // The manifest opts `echo` out of the OS sandbox: the loudest
    // escalation a rule can grant, and exactly the under-the-hood
    // deviation the v8 notices exist to surface.
    std::fs::create_dir_all(workspace.join(".loom")).expect("loom dir");
    std::fs::write(
        workspace.join(".loom/commands.toml"),
        "[[command]]\nprogram = \"echo\"\nsandbox = false\n",
    )
    .expect("manifest");
    // The escalation above (`sandbox = false`) only loads when the
    // manifest is pinned in the trust store (review F-12): pin it
    // into a per-process store so the passthrough notice fires.
    // Single test in this binary uses the override, so the
    // process-wide pointer cannot race another reader.
    loom_tools::trust::use_store(
        std::env::temp_dir().join(format!("loom-trust-engine-{}.toml", std::process::id())),
    );
    loom_tools::trust::pin(&workspace).expect("pin manifest");
    let mut config = test_config();
    config.workspace = workspace.clone();
    config.db_path = base.path().join("loom.db");
    // A tool turn plus the closing turn.
    config.max_turns = 2;
    // Two identical unconfined commands in one turn: the
    // sandbox-passthrough observation is a property of the batch,
    // not of each call — one event, not two.
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                two_identical_commands_script(),
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
            "Check the toolchain twice.",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("agent task");

    assert_eq!(summary.model_requests, 2, "tool turn plus final turn");
    let kinds = drain(&mut receiver);
    let completed = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::ToolCompleted { .. }))
        .count();
    assert_eq!(completed, 2, "both calls ran");
    let notices = kinds
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                EventKind::HarnessNotice { category, .. } if category == "sandbox.passthrough"
            )
        })
        .count();
    assert_eq!(
        notices, 1,
        "identical notices collapse to one event per batch"
    );
}
