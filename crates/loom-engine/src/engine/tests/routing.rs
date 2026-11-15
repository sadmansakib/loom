//! Tier routing: the Easy-tier agent pick and its harness notice.

use super::*;

#[tokio::test]
async fn routing_picks_the_tier_agent_and_surfaces_the_choice() {
    // Storeless engine and tier agent: routing must not depend on
    // persistence, and the injected agent cannot share the main
    // agent's in-memory store handle.
    let tier_agent = Agent::new(
        Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
            "the cheap model answered",
        )])),
        Box::new(DefaultContextCompiler::default()),
        loom_kernel::tool::ToolRegistry::new(),
        AgentConfig::new(PathBuf::from("."), "test agent"),
        "cheap-model",
    );
    let engine = Engine {
        agent: Agent::new(
            Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
                "the run model answered",
            )])),
            Box::new(DefaultContextCompiler::default()),
            loom_kernel::tool::ToolRegistry::new(),
            AgentConfig::new(PathBuf::from("."), "test agent"),
            "fake-model",
        ),
        routing_easy: Some(RoutedAgent {
            agent: Arc::new(tier_agent),
            model: "cheap-model".to_owned(),
        }),
        routing_hard: None,
        background: None,
        stall_escalation: None,
        db_path: None,
        lsp_servers: Vec::new(),
        workspace: PathBuf::from("."),
        policy: CommandPolicy::default(),
        fast_paths: false,
        resolution: None,
        notices_flushed: std::sync::OnceLock::new(),
        events: None,
        mcp_servers: Vec::new(),
        prune_ops: None,
    };

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();

    // Question-shaped text classifies Easy and lands on the tier
    // agent; the decision is attributed to the task post-hoc.
    engine
        .run_task(
            &mut session,
            "what does this repo do?",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("easy task");
    // The tier agent's scripted reply proves it ran (and only it:
    // the engine's own agent scripted a different line).
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
    assert_eq!(last_assistant, "the cheap model answered");
    let kinds = drain(&mut receiver);
    assert!(kinds.contains(&EventKind::HarnessNotice {
        category: "routing".to_owned(),
        detail: "task routed to cheap-model (tier easy)".to_owned(),
    }));

    // A work order classifies Medium and stays on the run's own
    // agent — no routing notice for it.
    engine
        .run_task(
            &mut session,
            "Fix the failing timeout test in the runner",
            &sink,
            &crate::TaskControl::default(),
        )
        .await
        .expect("medium task");
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
    assert_eq!(last_assistant, "the run model answered");
    let kinds = drain(&mut receiver);
    assert!(!kinds.iter().any(|kind| matches!(
        kind,
        EventKind::HarnessNotice { category, .. } if category == "routing"
    )));
}
