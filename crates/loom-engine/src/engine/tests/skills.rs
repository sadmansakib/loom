//! The skills seam (roadmap iteration 5): opt-in `load_skill`.

use super::*;

/// Roadmap iteration 5: the skills seam is opt-in — `skills: true`
/// exposes `load_skill` (and the facts block grows the skill menu
/// pointing at it); the default (and the bare-loop baseline) keeps
/// it out, so the frozen SWE-bench protocol is unchanged until a
/// frontend opts in.
fn skill_load_script() -> Vec<Result<ModelEvent, ModelError>> {
    use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
    let call = ToolCall {
        id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
        name: "load_skill".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({
            "name": "alpha",
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

fn skills_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("workspace");
    let dir = workspace.join(".agents").join("skills");
    std::fs::create_dir_all(&dir).expect("skills dir");
    std::fs::write(
        dir.join("alpha.md"),
        "# Alpha: release checklist\n\nRun cargo test before tagging.",
    )
    .expect("fixture");
    workspace
}

async fn run_skills_task(config: EngineConfig, overrides: EngineOverrides) -> Vec<EventKind> {
    let engine = Engine::open_with(config, overrides).expect("engine");
    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    let _ = engine
        .run_task(
            &mut session,
            "Load the alpha skill",
            &sink,
            &crate::TaskControl::default(),
        )
        .await;
    drain(&mut receiver)
}

// EventKind is #[non_exhaustive]; this probe only wants load_skill completions
#[allow(clippy::wildcard_enum_match_arm)]
fn skill_content(kinds: &[EventKind]) -> Option<&str> {
    kinds.iter().find_map(|kind| match kind {
        EventKind::ToolCompleted {
            name,
            status: loom_kernel::transcript::ToolStatus::Success,
            content_head,
            ..
        } if name == "load_skill" => Some(content_head.as_str()),
        // EventKind is #[non_exhaustive]; this probe only wants load_skill completions
        #[allow(clippy::wildcard_enum_match_arm)]
        _ => None,
    })
}

#[tokio::test]
async fn skill_tool_is_opt_in_and_serves_bodies() {
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = skills_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.skills = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        skill_load_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_skills_task(config, EngineOverrides::default().with_provider(provider)).await;
    let content = skill_content(&kinds).expect("load_skill ran successfully when opted in");
    assert!(content.contains("# skill: alpha"), "{content}");
    assert!(content.contains("Run cargo test"), "{content}");
}

#[tokio::test]
async fn skill_tool_stays_out_of_the_default_and_bare_registries() {
    // Default: the call fails as an unknown tool.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = skills_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    let provider = Box::new(FakeProvider::scripted(vec![
        skill_load_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_skills_task(config, EngineOverrides::default().with_provider(provider)).await;
    assert!(
        skill_content(&kinds).is_none(),
        "default registry must not expose load_skill"
    );

    // Bare arm: even an explicit opt-in is ignored — the arm
    // measures the plain loop.
    let base = tempfile::tempdir().expect("tempdir");
    let mut config = test_config();
    config.workspace = skills_fixture(base.path());
    config.db_path = base.path().join("loom.db");
    config.max_turns = 2;
    config.skills = true;
    let provider = Box::new(FakeProvider::scripted(vec![
        skill_load_script(),
        FakeProvider::end_turn_script("done"),
    ]));
    let kinds = run_skills_task(
        config,
        EngineOverrides::default()
            .with_provider(provider)
            .bare_loop(),
    )
    .await;
    assert!(
        skill_content(&kinds).is_none(),
        "the bare arm must never expose load_skill"
    );
}
