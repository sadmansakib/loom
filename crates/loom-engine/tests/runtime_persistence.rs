//! Cross-component runtime coverage for built-in tools and persisted sessions.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::stream;
use loom_engine::{
    Engine, EngineConfig, EngineOverrides, ReasoningEffort, TaskControl, TurnBudget,
};
use loom_kernel::agent::{TaskError, TaskSummary};
use loom_kernel::context::PruneConfig;
use loom_kernel::event::ChannelEventSink;
use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream, StopReason, Usage,
};
use loom_kernel::session::TaskStatus;
use loom_kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolStatus,
};
use loom_tools::ToolBudgets;
use serde_json::json;

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    action: Mutex<Option<StreamAction>>,
}

enum StreamAction {
    Pause(TaskControl),
    CancelAndPend(TaskControl),
}

impl ScriptedProvider {
    fn new(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            action: Mutex::new(None),
        }
    }

    fn with_action(self, action: StreamAction) -> Self {
        *self
            .action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(action);
        self
    }

    fn response(
        content: &str,
        tool_calls: Vec<ToolCall>,
        stop_reason: StopReason,
    ) -> Vec<Result<ModelEvent, ModelError>> {
        vec![Ok(ModelEvent::Completed(ModelResponse {
            message: AssistantMessage {
                content: content.to_owned(),
                reasoning: None,
                tool_calls,
                stop_reason,
            },
            usage: Usage::default(),
        }))]
    }
}

impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted-integration-test"
    }

    fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
        Some(ModelCapabilities::new(8192).with_tool_calls(true))
    }

    fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
        if let Some(action) = self
            .action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            match action {
                StreamAction::Pause(control) => control.pause(),
                StreamAction::CancelAndPend(control) => {
                    control.cancel();
                    return Box::pin(stream::pending());
                }
            }
        }
        let script = self
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(script))
    }
}

fn config(workspace: std::path::PathBuf, db_path: std::path::PathBuf) -> EngineConfig {
    EngineConfig {
        provider: "deepseek".to_owned(),
        model: "deepseek-flash".to_owned(),
        base_url: None,
        models_url: None,
        headers: Vec::new(),
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace,
        system_prompt: None,
        max_turns: 4,
        turn_budget: TurnBudget::default(),
        context_editing: None,
        db_path,
        persist: true,
        catalog: None,
        scout: None,
        routing: None,
        stall_escalation_model: None,
        background_commands: false,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        mcp: false,
        lsp: false,
        budgets: ToolBudgets::default(),
        prune: Some(PruneConfig::default()),
        autofix: false,
    }
}

#[allow(clippy::expect_used)] // test setup must fail immediately with a useful cause
fn open_engine(base: &tempfile::TempDir, provider: ScriptedProvider) -> Engine {
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let mut overrides = EngineOverrides::default().with_provider(Box::new(provider));
    overrides.fast_paths = Some(false);
    overrides.bare_loop = true;
    Engine::open_with(config(workspace, base.path().join("loom.db")), overrides)
        .expect("open persisted engine")
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end persistence/resume scenario with ordered assertions
async fn built_in_tool_result_survives_reload_and_session_resume() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let base = tempfile::tempdir().expect("temporary directory");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");

        let write_call = ToolCall {
            id: ToolCallId::try_from("write-1".to_owned()).expect("valid tool call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/generated.txt",
                "content": "created by the built-in tool\n"
            }))
            .expect("object arguments"),
        };
        let provider = ScriptedProvider::new(vec![
            ScriptedProvider::response(
                "Writing the requested file.",
                vec![write_call],
                StopReason::ToolCalls,
            ),
            ScriptedProvider::response("The file is written.", Vec::new(), StopReason::EndTurn),
            ScriptedProvider::response(
                "The session was reloaded successfully.",
                Vec::new(),
                StopReason::EndTurn,
            ),
        ]);
        let mut overrides = EngineOverrides::default().with_provider(Box::new(provider));
        overrides.fast_paths = Some(false);
        // Keep this scenario focused on the built-in tool, SQLite, and
        // resume boundary; feedback/nudge behavior has its own kernel tests.
        overrides.bare_loop = true;
        let engine = Engine::open_with(
            config(workspace.clone(), base.path().join("loom.db")),
            overrides,
        )
        .expect("open persisted engine");

        let (sink, _events) = ChannelEventSink::new();
        let mut session = engine.new_session();
        let first = engine
            .run_task(
                &mut session,
                "Write src/generated.txt with the requested content.",
                &sink,
                &TaskControl::default(),
            )
            .await
            .expect("first task completes");

        assert_eq!(first.successful_tool_calls, 1);
        assert_eq!(first.failed_tool_calls, 0);
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/generated.txt")).expect("written file"),
            "created by the built-in tool\n"
        );
        assert_eq!(session.tasks.len(), 1);
        assert_eq!(session.tasks[0].status, TaskStatus::Completed);
        assert!(session.transcript.iter().any(|item| matches!(
            item,
            Item::ToolResult(result) if result.status == ToolStatus::Success
        )));

        let session_id = session.id;
        let reloaded = engine
            .load_session(&session_id)
            .expect("load persisted session")
            .expect("session exists");
        assert_eq!(reloaded.transcript, session.transcript);
        assert_eq!(reloaded.tasks, session.tasks);
        assert_eq!(reloaded.tasks[0].status, TaskStatus::Completed);

        let continued = engine
            .continue_session(
                &session_id,
                "Confirm the saved file and finish.",
                &sink,
                &TaskControl::default(),
            )
            .await
            .expect("resume persisted session");
        let second: TaskSummary = continued.outcome.expect("continued task completes");
        assert_eq!(continued.session.tasks.len(), 2);
        assert_eq!(second.successful_tool_calls, 0);
        assert!(
            continued
                .session
                .tasks
                .iter()
                .all(|task| task.status == TaskStatus::Completed)
        );

        let durable = engine
            .load_session(&session_id)
            .expect("reload after continuation")
            .expect("continued session remains persisted");
        assert_eq!(durable.tasks.len(), 2);
        assert_eq!(durable.tasks, continued.session.tasks);
        assert_eq!(durable.transcript, continued.session.transcript);
        assert!(durable.transcript.iter().any(|item| matches!(
            item,
            Item::User { text } if text == "Confirm the saved file and finish."
        )));
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/generated.txt")).expect("file remains"),
            "created by the built-in tool\n"
        );
    })
    .await
    .expect("integration scenario should finish before its deadline");
}

#[tokio::test]
async fn provider_and_protocol_failures_are_persisted_without_corrupting_sessions() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let invalid_response = ScriptedProvider::response(
            "invalid response",
            vec![ToolCall {
                id: ToolCallId::try_from("invalid-1".to_owned()).expect("valid id"),
                name: "write_file".to_owned(),
                arguments: ToolArguments::try_from(json!({
                    "path": "should-not-exist.txt",
                    "content": "not committed"
                }))
                .expect("object arguments"),
            }],
            StopReason::EndTurn,
        );
        let cases = [
            (
                "provider error",
                vec![Err(ModelError::new(ModelErrorKind::Auth, "rejected key"))],
                "model_error",
                true,
            ),
            (
                "invalid stop/tool-call combination",
                invalid_response,
                "protocol",
                false,
            ),
        ];

        for (label, script, end_reason, is_model_error) in cases {
            let base = tempfile::tempdir().expect("temporary directory");
            let engine = open_engine(&base, ScriptedProvider::new(vec![script]));
            let (sink, _events) = ChannelEventSink::new();
            let mut session = engine.new_session();
            let error = engine
                .run_task(
                    &mut session,
                    format!("exercise {label}"),
                    &sink,
                    &TaskControl::default(),
                )
                .await
                .expect_err("scenario must fail cleanly");

            assert_eq!(
                matches!(error, TaskError::Model(_)),
                is_model_error,
                "{label}: {error:?}"
            );
            assert_eq!(
                matches!(error, TaskError::Protocol(_)),
                !is_model_error,
                "{label}: {error:?}"
            );
            assert_eq!(session.tasks.len(), 1, "{label}");
            assert_eq!(session.tasks[0].status, TaskStatus::Failed, "{label}");
            assert_eq!(
                session.tasks[0].end_reason.as_deref(),
                Some(end_reason),
                "{label}"
            );
            assert!(!base.path().join("workspace/should-not-exist.txt").exists());

            let durable = engine
                .load_session(&session.id)
                .expect("load failed session")
                .expect("failed session is persisted");
            assert_eq!(durable.tasks, session.tasks, "{label}");
            assert_eq!(durable.transcript, session.transcript, "{label}");
        }
    })
    .await
    .expect("failure scenarios should finish before their deadline");
}

#[tokio::test]
async fn cancellation_during_pending_model_stream_persists_session() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let base = tempfile::tempdir().expect("temporary directory");
        let control = TaskControl::default();
        let provider = ScriptedProvider::new(Vec::new())
            .with_action(StreamAction::CancelAndPend(control.clone()));
        let engine = open_engine(&base, provider);
        let (sink, _events) = ChannelEventSink::new();
        let mut session = engine.new_session();

        let error = engine
            .run_task(&mut session, "cancel while waiting", &sink, &control)
            .await
            .expect_err("cancelled stream must not complete");
        assert!(matches!(error, TaskError::Cancelled));
        assert_eq!(session.tasks.len(), 1);
        assert_eq!(session.tasks[0].status, TaskStatus::Cancelled);
        assert_eq!(
            session.tasks[0].end_reason.as_deref(),
            Some("user_cancelled")
        );

        let durable = engine
            .load_session(&session.id)
            .expect("load cancelled session")
            .expect("cancelled session is persisted");
        assert_eq!(durable.tasks, session.tasks);
        assert_eq!(durable.transcript, session.transcript);
    })
    .await
    .expect("cancellation scenario should finish before its deadline");
}

#[tokio::test]
async fn pause_commits_tool_work_and_session_can_resume() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let base = tempfile::tempdir().expect("temporary directory");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let control = TaskControl::default();
        let write_call = ToolCall {
            id: ToolCallId::try_from("pause-write".to_owned()).expect("valid tool call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "paused.txt",
                "content": "committed before pause\n"
            }))
            .expect("object arguments"),
        };
        let provider = ScriptedProvider::new(vec![
            ScriptedProvider::response("write then pause", vec![write_call], StopReason::ToolCalls),
            ScriptedProvider::response("resumed and complete", Vec::new(), StopReason::EndTurn),
        ])
        .with_action(StreamAction::Pause(control.clone()));
        let mut overrides = EngineOverrides::default().with_provider(Box::new(provider));
        overrides.fast_paths = Some(false);
        overrides.bare_loop = true;
        let engine = Engine::open_with(
            config(workspace.clone(), base.path().join("loom.db")),
            overrides,
        )
        .expect("open persisted engine");
        let (sink, _events) = ChannelEventSink::new();
        let mut session = engine.new_session();

        let error = engine
            .run_task(&mut session, "write and pause", &sink, &control)
            .await
            .expect_err("pause should stop before the next turn");
        assert!(matches!(error, TaskError::Paused));
        assert_eq!(session.tasks[0].status, TaskStatus::Paused);
        assert_eq!(session.tasks[0].end_reason.as_deref(), Some("paused"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("paused.txt")).expect("written file"),
            "committed before pause\n"
        );
        assert!(session.transcript.iter().any(|item| matches!(
            item,
            Item::ToolResult(result) if result.status == ToolStatus::Success
        )));

        let resumed = engine
            .continue_session(
                &session.id,
                "continue after pause",
                &sink,
                &TaskControl::default(),
            )
            .await
            .expect("resume paused session");
        resumed.outcome.expect("resumed task completes");
        assert_eq!(resumed.session.tasks.len(), 2);
        assert_eq!(resumed.session.tasks[0].status, TaskStatus::Paused);
        assert_eq!(resumed.session.tasks[1].status, TaskStatus::Completed);

        let durable = engine
            .load_session(&resumed.session.id)
            .expect("load resumed session")
            .expect("session remains persisted");
        assert_eq!(durable.tasks, resumed.session.tasks);
        assert_eq!(durable.transcript, resumed.session.transcript);
    })
    .await
    .expect("pause and resume scenario should finish before its deadline");
}
