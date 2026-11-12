use super::*;
use futures::stream;

use super::assembly::{bare_registry, build_registry};
use super::budgets::prune_config_for;
use crate::auth;
use loom_kernel::agent::{TaskError, TurnBudget};
use loom_kernel::event::EventKind;
use loom_kernel::id::SessionId;
use loom_providers::OPENROUTER_PROVIDER_NAME;
use loom_tools::ToolBudgets;
use loom_tools::policy::CommandPolicy;
use std::path::PathBuf;

use loom_kernel::context::{AgentConfig, DefaultContextCompiler, PruneConfig};
use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream, StopReason, Usage,
};
use loom_kernel::session::TaskStatus;
use loom_kernel::store::MemorySessionStore;
use loom_kernel::transcript::AssistantMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
/// A provider that replays one scripted event list per `stream()` call.
/// Enough engine coverage for the driving layer: the kernel loop's own
/// behaviour is pinned in `loom-kernel`'s `agent_loop` tests.
struct FakeProvider {
    scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
}

impl FakeProvider {
    /// A provider replaying the given scripts, one per `stream()` call.
    fn scripted(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
        Self {
            scripts: Mutex::new(VecDeque::from(scripts)),
        }
    }

    /// One script: a single response that answers with `text` and ends
    /// its turn.
    fn end_turn_script(text: &str) -> Vec<Result<ModelEvent, ModelError>> {
        vec![Ok(ModelEvent::Completed(ModelResponse {
            message: AssistantMessage {
                content: text.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage::default(),
        }))]
    }

    /// Scripts for two tasks: the first ends its turn, the second fails
    /// hard with `error`.
    fn end_turn_then_error(text: &str, error: ModelError) -> Self {
        Self::scripted(vec![Self::end_turn_script(text), vec![Err(error)]])
    }
}

impl ModelProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
        // Tool-calling on: scripts emit tool calls, and the default
        // compiler exposes tools only to a model that can call them
        // (§8.2) — with this off, every scripted call would bounce
        // back `NotExposed` instead of running.
        Some(ModelCapabilities::new(8192).with_tool_calls(true))
    }

    fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(stream::iter(script))
    }
}

/// An engine around a fake provider with an in-memory store — the
/// session-lifecycle seams without credentials, network, or files.
fn fake_engine(provider: FakeProvider) -> Engine {
    let agent = Agent::new(
        Box::new(provider),
        Box::new(DefaultContextCompiler::default()),
        loom_kernel::tool::ToolRegistry::new(),
        AgentConfig::new(PathBuf::from("."), "test agent"),
        "fake-model",
    )
    .with_store(Box::new(MemorySessionStore::new()));
    Engine {
        agent,
        routing_easy: None,
        background: None,
        stall_escalation: None,
        db_path: None,
        lsp_servers: Vec::new(),
        routing_hard: None,
        workspace: PathBuf::from("."),
        policy: CommandPolicy::default(),
        fast_paths: false,
        resolution: None,
        notices_flushed: std::sync::OnceLock::new(),
        events: None,
    }
}

/// A one-file cargo fixture the fast paths can rename inside.
fn rename_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_rename\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse_opts() -> u32 {\n    7\n}\n\n#[cfg(test)]\nmod tests {\n    \
         use super::*;\n\n    #[test]\n    fn calls_the_helper() {\n        assert_eq!(parse_opts(), 7);\n    }\n}\n",
    )
    .expect("lib");
    workspace
}

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
            &Default::default(),
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
            _ => None,
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
            &Default::default(),
        )
        .await
        .expect("medium task");
    let last_assistant = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            loom_kernel::transcript::Item::Assistant(message) => Some(message.content.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(last_assistant, "the run model answered");
    let kinds = drain(&mut receiver);
    assert!(!kinds.iter().any(|kind| matches!(
        kind,
        EventKind::HarnessNotice { category, .. } if category == "routing"
    )));
}

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
            &Default::default(),
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
            _ => None,
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
            &Default::default(),
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
            &Default::default(),
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
        .run_task(&mut session, "check on it", &sink, &Default::default())
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
async fn a_persisted_run_also_persists_its_event_stream() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.persist = true;
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
        .expect("task");
    assert_eq!(summary.model_requests, 1);
    let _ = drain(&mut receiver);

    // The durable stream carries the same run: task lifecycle,
    // authoritative completion, and the engine's idle marker, in
    // emission order with contiguous per-run sequences.
    use loom_kernel::store::EventStore;
    let store = loom_store::SqliteStore::open(base.path().join("loom.db")).expect("store");
    let events = store.load_events(&session.id).expect("events");
    assert!(events.len() >= 4, "expected a full stream, got {events:?}");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match &event.kind {
            EventKind::TaskStarted => "task_started",
            EventKind::ModelCompleted { .. } => "model_completed",
            EventKind::TaskCompleted => "task_completed",
            EventKind::AwaitingInput { .. } => "awaiting_input",
            EventKind::FastPathDeclined { .. } => "fast_path_declined",
            EventKind::CapabilitiesResolved { .. } => "capabilities_resolved",
            EventKind::HarnessNotice { .. } => "harness_notice",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds.first(), Some(&"task_started"));
    assert!(kinds.contains(&"model_completed"));
    assert!(kinds.contains(&"task_completed"));
    assert_eq!(kinds.last(), Some(&"awaiting_input"));
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, index as u64, "one run numbers contiguously");
    }
}

#[tokio::test]
async fn a_second_writer_cannot_drive_a_leased_session() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = rename_fixture(base.path());
    let mut config = test_config();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");
    config.persist = true;
    let engine = Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("done"),
                FakeProvider::end_turn_script("done again"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    engine
        .run_task(&mut session, "Say hello.", &sink, &Default::default())
        .await
        .expect("first task");

    // A foreign writer holds the session's lease (ADR-008): the
    // next run must refuse before persisting anything.
    let store = loom_store::SqliteStore::open(base.path().join("loom.db")).expect("store");
    let lease = store
        .acquire_lease(&session.id, "another-process", 60_000)
        .expect("lease")
        .expect("granted");
    assert_eq!(lease.owner(), "another-process");

    let continuation = engine
        .continue_session(&session.id, "Interloper.", &sink, &Default::default())
        .await
        .expect("continue returns the outcome, not an error");
    let error = continuation.outcome.expect_err("leased session refuses");
    match &error {
        loom_kernel::agent::TaskError::SessionLeased { owner, .. } => {
            assert_eq!(owner, "another-process");
        }
        other => panic!("expected SessionLeased, got {other:?}"),
    }

    // The refused run persisted nothing: the transcript tip is
    // still the first task's.
    let reloaded = engine
        .load_session(&session.id)
        .expect("load")
        .expect("known");
    assert_eq!(
        reloaded.transcript.len(),
        session.transcript.len(),
        "no items from the refused run"
    );

    // Released, the session accepts input again.
    assert!(
        store
            .release_lease(&session.id, "another-process")
            .expect("release")
    );
    engine
        .continue_session(&session.id, "Back for real.", &sink, &Default::default())
        .await
        .expect("continue")
        .outcome
        .expect("second task after release");
}

#[tokio::test]
async fn capability_resolution_surfaces_once_per_engine() {
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
                FakeProvider::end_turn_script("done again"),
            ]))),
            ..EngineOverrides::default()
        },
    )
    .expect("engine");

    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();
    engine
        .run_task(&mut session, "Say hello.", &sink, &Default::default())
        .await
        .expect("first task");
    engine
        .run_task(&mut session, "Say it again.", &sink, &Default::default())
        .await
        .expect("second task");

    let kinds = drain(&mut receiver);
    let resolutions = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::CapabilitiesResolved { .. }))
        .count();
    assert_eq!(
        resolutions, 1,
        "one engine, one capability resolution — not one per task"
    );
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
            &Default::default(),
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

#[tokio::test]
async fn continue_session_resumes_a_persisted_session() {
    let engine = fake_engine(FakeProvider::scripted(vec![
        FakeProvider::end_turn_script("first"),
        FakeProvider::end_turn_script("second"),
    ]));
    let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
    let mut session = engine.new_session();

    engine
        .run_task(
            &mut session,
            "do the first thing",
            &sink,
            &Default::default(),
        )
        .await
        .expect("first task");

    // Resume through the first-class flow: the id alone must suffice.
    let continuation = engine
        .continue_session(&session.id, "now the second", &sink, &Default::default())
        .await
        .expect("continue");

    let summary = continuation.outcome.expect("second task completed");
    assert_eq!(summary.turns, 1);
    assert_eq!(continuation.session.tasks.len(), 2);
    // The store is the authority: reloading the id sees both tasks.
    let reloaded = engine
        .load_session(&session.id)
        .expect("load")
        .expect("persisted");
    assert_eq!(reloaded.tasks.len(), 2);

    // Idle markers: one per task end, each naming the session, always
    // after the terminal event.
    let kinds = drain(&mut receiver);
    let idle: Vec<_> = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::AwaitingInput { .. }))
        .collect();
    assert_eq!(idle.len(), 2, "kinds: {kinds:?}");
    assert!(matches!(
        kinds.last(),
        Some(EventKind::AwaitingInput { session_id }) if *session_id == session.id
    ));
}

#[tokio::test]
async fn recorder_observes_the_engine_lifecycle_through_a_fanout() {
    // B5's consumer shape: fan a StatusRecorder clone out next to a
    // rendering channel; polling the retained handle must see the
    // task run and the engine's idle marker flip it back to Idle.
    let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
        "done",
    )]));
    let recorder = loom_kernel::event::StatusRecorder::new();
    let (channel, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let sink = loom_kernel::event::FanoutSink::new()
        .with(recorder.clone())
        .with(channel);
    let mut session = engine.new_session();

    engine
        .run_task(&mut session, "anything", &sink, &Default::default())
        .await
        .expect("task completes");

    let snapshot = recorder.snapshot();
    assert_eq!(snapshot.phase, loom_kernel::event::TaskPhase::Idle);
    assert_eq!(snapshot.session_id, Some(session.id));
    assert_eq!(
        snapshot.task_id,
        session.tasks.last().map(|record| record.id),
        "the idle marker's envelope names the ended task"
    );
    assert_eq!(snapshot.turn, Some(1));
    assert_eq!(snapshot.attempt, None);
    assert_eq!(snapshot.in_flight_tools, Vec::<String>::new());
}

#[tokio::test]
async fn continue_session_reports_unknown_ids() {
    let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
        "unused",
    )]));
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let error = engine
        .continue_session(&SessionId::new(), "anything", &sink, &Default::default())
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::SessionNotFound { .. }));
}

#[tokio::test]
async fn continue_session_returns_the_advanced_session_on_task_failure() {
    // First task fine, second task a hard model failure: the
    // continuation still carries the advanced session.
    let engine = fake_engine(FakeProvider::end_turn_then_error(
        "first",
        ModelError::new(ModelErrorKind::Auth, "key rejected"),
    ));
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    engine
        .run_task(&mut session, "first", &sink, &Default::default())
        .await
        .expect("first task");

    let continuation = engine
        .continue_session(&session.id, "second", &sink, &Default::default())
        .await
        .expect("continuation is not a load error");

    assert!(continuation.outcome.is_err(), "task failed");
    assert_eq!(continuation.session.tasks.len(), 2);
    // The failed task is persisted with its terminal status, so a
    // later continuation starts from honest state.
    assert_eq!(continuation.session.tasks[1].status, TaskStatus::Failed);
    // Idle is still signalled after a failed task; the marker itself
    // is pinned in the resume test above.
}

/// Drains every pending event kind from the channel.
fn drain(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<loom_kernel::event::EventEnvelope>,
) -> Vec<EventKind> {
    let mut kinds = Vec::new();
    while let Ok(envelope) = receiver.try_recv() {
        kinds.push(envelope.kind);
    }
    kinds
}

#[tokio::test]
async fn open_async_resolves_the_credential_before_fetching() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = OPENROUTER_PROVIDER_NAME.to_owned();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    // No cached catalog and no credential: the fetch path must reach
    // credential resolution (inside `catalog::load`) and report the
    // missing credential — never a network call in tests.
    let error = match Engine::open_async(config).await {
        Ok(_) => panic!("open succeeded without a credential"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            EngineError::Auth(auth::AuthError::NoCredential { .. })
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn open_with_skips_credential_resolution_for_an_overridden_provider() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = "replay".to_owned(); // no adapter, no credential
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    // Only the override path can succeed here: building "replay"
    // would fail with UnknownProvider before any credential work.
    let engine = Engine::open_with(
        config,
        EngineOverrides::default().with_provider(Box::new(FakeProvider::scripted(vec![
            FakeProvider::end_turn_script("overridden"),
        ]))),
    )
    .expect("no credential is resolved when the provider is overridden");
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let summary = engine
        .run_task(&mut session, "task", &sink, &Default::default())
        .await
        .expect("task runs on the overridden provider");
    assert_eq!(summary.turns, 1);
}

#[tokio::test]
async fn a_provider_hook_wraps_whichever_provider_results() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = "replay".to_owned();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    let engine = Engine::open_with(
        config,
        EngineOverrides::default()
            .with_provider(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("original"),
            ])))
            .with_provider_hook(Arc::new(|_built: Box<dyn ModelProvider>| {
                // The hook decides what runs: a different scripted
                // answer proves the replacement took effect.
                Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
                    "hooked",
                )]))
            })),
    )
    .expect("open");
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    engine
        .run_task(&mut session, "task", &sink, &Default::default())
        .await
        .expect("task runs on the hooked provider");
    let answer = session.transcript.iter().rev().find_map(|item| match item {
        loom_kernel::transcript::Item::Assistant(message) => Some(message.content.clone()),
        _ => None,
    });
    assert_eq!(answer.as_deref(), Some("hooked"));
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
            &Default::default(),
        )
        .await;
    drain(&mut receiver)
}

fn skill_content(kinds: &[EventKind]) -> Option<&str> {
    kinds.iter().find_map(|kind| match kind {
        EventKind::ToolCompleted {
            name,
            status: loom_kernel::transcript::ToolStatus::Success,
            content_head,
            ..
        } if name == "load_skill" => Some(content_head.as_str()),
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
            &Default::default(),
        )
        .await;
    drain(&mut receiver)
}

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
