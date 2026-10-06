//! Agent-loop integration tests (plan 1F and 1G) driven by a scripted fake
//! `ModelProvider` and fake tools.
//!
//! 1F: the real `Agent::run_task` loop end to end — context compilation, the
//! streaming seam, stop-reason handling, atomic turn commits, the tool
//! engine, usage accounting, and terminal events.
//!
//! 1G: cross-cutting scenarios — retry before/after output, exhausted
//! attempts, non-retryable kinds, cancellation during stream and during a
//! tool batch, discarded deltas, a broken event consumer, and monotonic
//! event sequences.

// Integration tests are a separate crate; helpers below are not `#[test]`
// functions, so the workspace's `unwrap_used` / `expect_used` warnings are
// allowed file-wide for test ergonomics.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream;
use loom_kernel::agent::{Agent, RetryPolicy, TaskBudget, TaskError};
use loom_kernel::context::{AgentConfig, DefaultContextCompiler};
use loom_kernel::event::{ChannelEventSink, EventEnvelope, EventKind};
use loom_kernel::id::SessionId;
use loom_kernel::model::{
    Message, ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider,
    ModelRequest, ModelResponse, ModelStream, StopReason, Usage,
};
use loom_kernel::session::{Session, TaskStatus};
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolSpec};
use loom_kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolFailure, ToolStatus,
};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// One scripted stream: the events and errors a single `stream()` call emits
/// before ending.
type Script = Vec<Result<ModelEvent, ModelError>>;

/// A provider that replays a scripted list of event sequences, one per
/// `stream()` call, and records every request it received.
struct FakeProvider {
    capabilities: ModelCapabilities,
    scripts: Mutex<VecDeque<Script>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    repeat_last: bool,
    /// When set, `stream()` returns a stream that never yields — used to
    /// exercise cancellation while a request is in flight (§6.6).
    pending: bool,
    /// When set, `stream()` cancels this token as soon as a request begins,
    /// so the test needs no concurrent cancel task.
    cancel_token: Option<CancellationToken>,
}

impl FakeProvider {
    fn new(capabilities: ModelCapabilities, scripts: Vec<Script>) -> Self {
        Self {
            capabilities,
            scripts: Mutex::new(VecDeque::from(scripts)),
            requests: Arc::new(Mutex::new(Vec::new())),
            repeat_last: false,
            pending: false,
            cancel_token: None,
        }
    }

    /// Repeats the final script for every further `stream()` call — used by
    /// the turn-limit test, where the model keeps requesting tools.
    fn repeat_last(mut self) -> Self {
        self.repeat_last = true;
        self
    }

    /// Cancels `token` the moment a request begins and then hangs the stream
    /// — used by the cancel-during-stream test. `run_task` observes the
    /// token in its biased `select!` and drops the stream (§6.6), so the task
    /// returns without ever committing, and the test needs no concurrent
    /// cancel task.
    fn cancel_on_stream(mut self, token: CancellationToken) -> Self {
        self.pending = true;
        self.cancel_token = Some(token);
        self
    }

    /// A shared handle to the recorded requests, usable after the provider is
    /// moved into the agent.
    fn requests_handle(&self) -> Arc<Mutex<Vec<ModelRequest>>> {
        Arc::clone(&self.requests)
    }
}

impl ModelProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
        Some(self.capabilities)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        self.requests.lock().unwrap().push(request.clone());
        if self.pending {
            if let Some(token) = &self.cancel_token {
                token.cancel();
            }
            // A stream that never yields; the loop's biased `select!` sees
            // the token and returns, dropping this stream (§6.6).
            return Box::pin(stream::pending());
        }
        let events = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.len() > 1 {
                scripts.pop_front().unwrap()
            } else if self.repeat_last {
                scripts.front().cloned().unwrap_or_default()
            } else {
                scripts.pop_front().unwrap_or_default()
            }
        };
        Box::pin(stream::iter(events))
    }
}

/// A read-only tool that echoes its input, enough to exercise the engine.
struct FakeTool {
    spec: ToolSpec,
    read_only: bool,
}

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn call<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { Ok(ToolOutput::new(format!("echo: {input}"))) })
    }
}

/// A tool that cancels the task's own token on entry and then hangs.
///
/// `execute_call`'s first poll drives the tool future, which cancels the
/// token and then stays pending. The next poll sees the now-ready
/// cancellation future and reports `ToolStatus::Cancelled { started: true }`
/// deterministically, without the test racing the batch from another task.
struct BlockingTool {
    spec: ToolSpec,
}

impl Tool for BlockingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            ctx.cancellation.cancel();
            // Never resolve: the tool is "in flight" while the engine's
            // select observes the now-ready cancellation future and reports
            // `Cancelled { started: true }`.
            std::future::pending::<()>().await;
            Ok(ToolOutput::new("cancelled"))
        })
    }
}

/// A mutating tool that always fails and counts how many times it was
/// actually invoked — lets a test prove that a refused repeat never ran.
struct FailingTool {
    spec: ToolSpec,
    invocations: Arc<Mutex<u32>>,
}

impl Tool for FailingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let invocations = Arc::clone(&self.invocations);
        Box::pin(async move {
            *invocations.lock().unwrap() += 1;
            Err(ToolError::Execution("boom".to_owned()))
        })
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities::new(100_000)
        .with_max_output_tokens(4_096)
        .with_tool_calls(true)
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: "test tool".to_owned(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

fn registry_with(tool: &str) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(FakeTool {
            spec: spec(tool),
            read_only: true,
        }))
        .unwrap();
    registry
}

/// A registry with a fast echo and a blocking tool. The echo is marked
/// mutating so the batch runs sequentially in call order: the echo's result
/// is committed before the blocking call is even polled, making the
/// "completed results plus cancelled filler" split deterministic.
fn registry_for_cancel_batch() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(FakeTool {
            spec: spec("echo"),
            read_only: false,
        }))
        .unwrap();
    registry
        .register(Box::new(BlockingTool {
            spec: spec("block"),
        }))
        .unwrap();
    registry
}

fn assistant(content: &str, stop_reason: StopReason, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: content.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason,
    }
}

fn completed(message: AssistantMessage, usage: Usage) -> Script {
    vec![Ok(ModelEvent::Completed(ModelResponse { message, usage }))]
}

fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(arguments).unwrap(),
    }
}

fn agent(provider: FakeProvider, registry: ToolRegistry, max_turns: u32) -> Agent {
    agent_with_retry(
        provider,
        registry,
        max_turns,
        RetryPolicy::new()
            .with_initial_backoff(Duration::from_millis(1))
            .with_jitter_fraction(0.0),
    )
}

fn agent_with_retry(
    provider: FakeProvider,
    registry: ToolRegistry,
    max_turns: u32,
    retry: RetryPolicy,
) -> Agent {
    Agent::new(
        Box::new(provider),
        Box::new(DefaultContextCompiler),
        registry,
        AgentConfig::new(PathBuf::from("."), "you are a test agent"),
        "fake-model",
    )
    .with_max_turns(max_turns)
    .with_retry(retry)
}

fn drain(receiver: &mut UnboundedReceiver<EventEnvelope>) -> Vec<EventKind> {
    drain_envelopes(receiver)
        .into_iter()
        .map(|envelope| envelope.kind)
        .collect()
}

fn usage(input: u64, output: u64, cached: Option<u64>) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
    }
}

#[tokio::test]
async fn end_turn_completes_the_task_and_commits_the_assistant() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(10, 3, Some(2)),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "hello", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.turns, 1);
    assert_eq!(summary.model_requests, 1);
    assert_eq!(summary.input_tokens, 10);
    assert_eq!(summary.output_tokens, 3);
    assert_eq!(summary.cached_input_tokens, Some(2));
    assert_eq!(summary.successful_tool_calls, 0);

    // Transcript: the user item then the committed assistant.
    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(&session.transcript[0], Item::User { text } if text == "hello"));
    assert!(matches!(&session.transcript[1], Item::Assistant(_)));

    // Task record finalized as success.
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.turns, 1);
    assert_eq!(record.usage, usage(10, 3, Some(2)));
    assert_eq!(record.end_reason.as_deref(), Some("end_turn"));

    // Ordered events, starting and ending terminal.
    let kinds = drain(&mut receiver);
    assert!(matches!(kinds[0], EventKind::TaskStarted));
    assert!(matches!(kinds[1], EventKind::TurnStarted { turn: 1 }));
    assert!(matches!(
        kinds[2],
        EventKind::ModelRequestStarted { attempt: 1 }
    ));
    assert!(matches!(kinds[3], EventKind::ModelCompleted { .. }));
    assert!(matches!(kinds[4], EventKind::TaskCompleted));
    assert_eq!(kinds.len(), 5);
}

#[tokio::test]
async fn tool_round_trip_executes_and_commits_one_turn_per_model_response() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "echo", json!({ "q": "hi" }))],
                ),
                usage(20, 5, None),
            ),
            completed(
                assistant("all done", StopReason::EndTurn, Vec::new()),
                usage(30, 7, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_with("echo"), 10);

    let summary = agent
        .run_task(&mut session, "run the tool", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.turns, 2);
    assert_eq!(summary.model_requests, 2);
    assert_eq!(summary.successful_tool_calls, 1);
    assert_eq!(summary.failed_tool_calls, 0);
    assert_eq!(summary.input_tokens, 50);
    assert_eq!(summary.output_tokens, 12);

    // user, assistant(call), result, assistant(end)
    assert_eq!(session.transcript.len(), 4);
    assert!(matches!(&session.transcript[0], Item::User { .. }));
    assert!(matches!(&session.transcript[1], Item::Assistant(a) if a.tool_calls.len() == 1));
    let Item::ToolResult(result) = &session.transcript[2] else {
        panic!("expected a tool result");
    };
    assert_eq!(result.call_id.as_str(), "call_1");
    assert_eq!(result.content, "echo: {\"q\":\"hi\"}");
    assert!(matches!(&session.transcript[3], Item::Assistant(a) if a.tool_calls.is_empty()));

    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.turns, 2);

    // Both requests went through the compiler: system prefix, model id, and
    // exposed tool in the first; the tool result replayed in the second.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "fake-model");
    assert_eq!(requests[0].max_tokens, Some(4_096));
    assert!(
        requests[0].tools.iter().any(|tool| tool.name == "echo"),
        "the registered tool must be exposed"
    );
    assert!(
        matches!(&requests[0].messages[0], Message::System { text } if text == "you are a test agent")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_))),
        "the second request must replay the tool result"
    );

    let kinds = drain(&mut receiver);
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ToolStarted { name, .. } if name == "echo"))
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ToolCompleted { name, .. } if name == "echo"))
    );
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCompleted));
}

#[tokio::test]
async fn max_tokens_commits_the_assistant_then_fails() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("truncated", StopReason::MaxTokens, Vec::new()),
            usage(15, 4, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        TaskError::UnrecoverableStop {
            stop_reason: "max_tokens"
        }
    ));
    // The truncated reply is still committed for inspection.
    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(&session.transcript[1], Item::Assistant(a) if a.content == "truncated"));

    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.end_reason.as_deref(), Some("max_tokens"));
}

#[tokio::test]
async fn persistent_tool_calls_hit_the_turn_limit() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant(
                "",
                StopReason::ToolCalls,
                vec![tool_call("call_1", "echo", json!({}))],
            ),
            usage(5, 1, None),
        )],
    )
    .repeat_last();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_with("echo"), 3);

    let error = agent
        .run_task(&mut session, "loop", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(error, TaskError::MaxTurns { max_turns: 3 }));
    // user + 3 (assistant + result) turns.
    assert_eq!(session.transcript.len(), 7);
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.turns, 3);
    assert_eq!(record.end_reason.as_deref(), Some("max_turns"));

    let kinds = drain(&mut receiver);
    assert!(matches!(
        kinds.last().unwrap(),
        EventKind::TaskFailed { .. }
    ));
}

#[tokio::test]
async fn retryable_failure_is_retried_and_partial_output_discarded() {
    let failed_attempt: Script = vec![
        Ok(ModelEvent::TextDelta("partial".to_owned())),
        Err(ModelError::new(ModelErrorKind::Transport, "dropped")),
    ];
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            failed_attempt,
            completed(
                assistant("recovered", StopReason::EndTurn, Vec::new()),
                usage(8, 2, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.turns, 1);
    assert_eq!(summary.model_requests, 2);
    assert_eq!(requests.lock().unwrap().len(), 2);

    // Nothing from the failed attempt entered the transcript: only the user
    // item and the recovered assistant remain.
    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(&session.transcript[1], Item::Assistant(a) if a.content == "recovered"));

    let kinds = drain(&mut receiver);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        EventKind::ModelRetry {
            attempt: 1,
            response_started: true,
            ..
        }
    )));
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ModelRequestStarted { attempt: 2 }))
    );
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCompleted));
}

#[tokio::test]
async fn a_pre_cancelled_task_commits_nothing() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("never seen", StopReason::EndTurn, Vec::new()),
            usage(1, 1, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    token.cancel();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(error.is_cancelled());
    assert!(matches!(&error, TaskError::Cancelled));
    // Only the user input was appended; no model output was committed.
    assert_eq!(session.transcript.len(), 1);
    assert!(matches!(&session.transcript[0], Item::User { .. }));
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Cancelled);
    assert_eq!(record.end_reason.as_deref(), Some("user_cancelled"));

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCancelled));
}

// ---------------------------------------------------------------------------
// 1G: cancellation / retry / event scenarios
// ---------------------------------------------------------------------------

fn retryable(kind: ModelErrorKind, message: &str) -> Script {
    vec![Err(ModelError::new(kind, message))]
}

fn drain_envelopes(receiver: &mut UnboundedReceiver<EventEnvelope>) -> Vec<EventEnvelope> {
    let mut envelopes = Vec::new();
    while let Ok(envelope) = receiver.try_recv() {
        envelopes.push(envelope);
    }
    envelopes
}

/// Retry before any output: the failed attempt emitted no delta, so the
/// retry event reports `response_started: false`.
#[tokio::test]
async fn retry_before_output_started_reports_no_response() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            retryable(ModelErrorKind::Transport, "connect failed"),
            completed(
                assistant("clean", StopReason::EndTurn, Vec::new()),
                usage(4, 1, None),
            ),
        ],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.turns, 1);
    assert_eq!(summary.model_requests, 2);

    let kinds = drain(&mut receiver);
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        EventKind::ModelRetry {
            attempt: 1,
            response_started: false,
            ..
        }
    )));
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCompleted));
}

/// Exhausted retries: with a budget of three attempts and a provider that
/// always fails retryably, the loop issues exactly three requests, emits two
/// retry events, and then fails the task with no commit.
#[tokio::test]
async fn exhausted_attempts_fail_without_committing() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            retryable(ModelErrorKind::Overloaded, "busy 1"),
            retryable(ModelErrorKind::Overloaded, "busy 2"),
            retryable(ModelErrorKind::Overloaded, "busy 3"),
        ],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent_with_retry(
        provider,
        ToolRegistry::new(),
        10,
        RetryPolicy::new()
            .with_max_attempts(3)
            .with_initial_backoff(Duration::from_millis(1))
            .with_jitter_fraction(0.0),
    );

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(error, TaskError::Model(_)));
    // Only the user item was appended; the failed turn never committed.
    assert_eq!(session.transcript.len(), 1);
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.end_reason.as_deref(), Some("model_error"));

    let kinds = drain(&mut receiver);
    let requests = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::ModelRequestStarted { .. }))
        .count();
    let retries = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::ModelRetry { .. }))
        .count();
    assert_eq!(requests, 3);
    assert_eq!(retries, 2);
    assert!(matches!(
        kinds.last().unwrap(),
        EventKind::TaskFailed { .. }
    ));
}

/// A non-retryable kind fails on the first attempt: no retry event, one
/// request, no commit.
#[tokio::test]
async fn non_retryable_kind_fails_immediately() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![retryable(ModelErrorKind::Auth, "invalid key")],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(error, TaskError::Model(_)));
    assert_eq!(session.transcript.len(), 1);

    let kinds = drain(&mut receiver);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::ModelRequestStarted { .. }))
            .count(),
        1
    );
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ModelRetry { .. }))
    );
    assert!(matches!(
        kinds.last().unwrap(),
        EventKind::TaskFailed { .. }
    ));
}

/// Cancellation while a request is in flight: the streaming `select!` sees
/// the token, drops the stream, and the task ends with nothing committed.
#[tokio::test]
async fn cancellation_during_stream_commits_nothing() {
    let token = CancellationToken::new();
    let provider = FakeProvider::new(capabilities(), Vec::new()).cancel_on_stream(token.clone());
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(&error, TaskError::Cancelled));

    // Only the user item; the in-flight turn never committed.
    assert_eq!(session.transcript.len(), 1);
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Cancelled);
    assert_eq!(record.end_reason.as_deref(), Some("user_cancelled"));

    let kinds = drain(&mut receiver);
    // The request did start before cancellation was observed.
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ModelRequestStarted { .. }))
    );
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCancelled));
}

/// Cancellation during a tool batch: the turn is still committed, with the
/// completed result preserved and the cancelled call recorded as a filler.
#[tokio::test]
async fn cancellation_during_batch_commits_turn_with_fillers() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant(
                "",
                StopReason::ToolCalls,
                vec![
                    tool_call("call_echo", "echo", json!({ "q": "hi" })),
                    tool_call("call_block", "block", json!({})),
                ],
            ),
            usage(12, 6, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_for_cancel_batch(), 10);

    let error = agent
        .run_task(&mut session, "run both", &sink, &token)
        .await
        .unwrap_err();

    assert!(matches!(&error, TaskError::Cancelled));
    // user, assistant(2 calls), result(echo), result(block)
    assert_eq!(session.transcript.len(), 4);

    let Item::ToolResult(first) = &session.transcript[2] else {
        panic!("expected the echo result");
    };
    assert_eq!(first.call_id.as_str(), "call_echo");
    assert_eq!(first.status, ToolStatus::Success);

    let Item::ToolResult(second) = &session.transcript[3] else {
        panic!("expected the block result");
    };
    assert_eq!(second.call_id.as_str(), "call_block");
    assert_eq!(second.status, ToolStatus::Cancelled { started: true });

    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Cancelled);
    assert_eq!(record.turns, 1);

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCancelled));
}

/// Retried deltas never reach the transcript: the failed attempt streams
/// text, then errors; only the successful attempt's content is committed.
#[tokio::test]
async fn retried_deltas_are_never_committed() {
    let failed: Script = vec![
        Ok(ModelEvent::TextDelta("ghost".to_owned())),
        Ok(ModelEvent::ReasoningDelta("phantom".to_owned())),
        Err(ModelError::new(ModelErrorKind::Server, "lost")),
    ];
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            failed,
            completed(
                assistant("clean", StopReason::EndTurn, Vec::new()),
                usage(6, 2, None),
            ),
        ],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(&session.transcript[1], Item::Assistant(a) if a.content == "clean"));
    // No committed text carries the discarded attempt's content.
    for item in &session.transcript {
        if let Item::Assistant(message) = item {
            assert!(!message.content.contains("ghost"));
            assert!(message.reasoning.is_none());
        }
    }

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskCompleted));
}

/// A closed event consumer does not change the task outcome; the drop count
/// is reported in the summary.
#[tokio::test]
async fn a_broken_event_consumer_does_not_change_the_outcome() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(3, 1, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, receiver) = ChannelEventSink::new();
    // Drop the consumer before the task runs: every emit is now a drop.
    drop(receiver);
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.turns, 1);
    assert!(summary.events_dropped > 0, "drops must be reported");
    assert_eq!(session.transcript.len(), 2);
    assert_eq!(session.tasks[0].status, TaskStatus::Completed);
}

/// Envelope sequence numbers are strictly increasing within a task.
#[tokio::test]
async fn event_sequence_is_monotonic() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "echo", json!({}))],
                ),
                usage(9, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(7, 2, None),
            ),
        ],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_with("echo"), 10);

    agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    let envelopes = drain_envelopes(&mut receiver);
    assert!(envelopes.len() > 1);
    for pair in envelopes.windows(2) {
        assert!(
            pair[0].sequence < pair[1].sequence,
            "sequence must strictly increase: {} then {}",
            pair[0].sequence,
            pair[1].sequence
        );
    }
    assert!(matches!(
        envelopes.last().unwrap().kind,
        EventKind::TaskCompleted
    ));
}

// ---------------------------------------------------------------------------
// Tool-failure policy: the loop refuses blind repetition
// ---------------------------------------------------------------------------

/// A registry with a failing tool (`boom`) and a succeeding echo, plus the
/// failing tool's invocation counter. `boom` is mutating so batches run
/// sequentially in call order.
fn registry_for_repeat() -> (ToolRegistry, Arc<Mutex<u32>>) {
    let mut registry = ToolRegistry::new();
    let invocations = Arc::new(Mutex::new(0_u32));
    registry
        .register(Box::new(FailingTool {
            spec: spec("boom"),
            invocations: Arc::clone(&invocations),
        }))
        .unwrap();
    registry
        .register(Box::new(FakeTool {
            spec: spec("echo"),
            read_only: true,
        }))
        .unwrap();
    (registry, invocations)
}

/// A failed call repeated unchanged in a later turn is refused by the loop
/// with `ToolFailure::RepeatedCall` and never reaches the tool again.
#[tokio::test]
async fn repeated_failed_call_is_refused_and_not_rereun() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "boom", json!({ "q": 1 }))],
                ),
                usage(5, 1, None),
            ),
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_2", "boom", json!({ "q": 1 }))],
                ),
                usage(5, 1, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(5, 1, None),
            ),
        ],
    );
    let (registry, invocations) = registry_for_repeat();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "run it twice", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.successful_tool_calls, 0);
    assert_eq!(summary.failed_tool_calls, 2);
    // The tool ran exactly once: the second call was refused, not re-run.
    assert_eq!(*invocations.lock().unwrap(), 1);

    // user, assistant, result(boom: execution), assistant, result(boom: repeated), assistant
    assert_eq!(session.transcript.len(), 6);
    let Item::ToolResult(first) = &session.transcript[2] else {
        panic!("expected the first boom result");
    };
    assert!(matches!(
        &first.status,
        ToolStatus::Error(ToolFailure::Execution)
    ));
    assert!(first.content.contains("error: execution failed: boom"));
    assert!(
        first.content.contains("Hint:"),
        "a real failure carries a forward hint"
    );

    let Item::ToolResult(second) = &session.transcript[4] else {
        panic!("expected the repeated boom result");
    };
    assert!(matches!(
        &second.status,
        ToolStatus::Error(ToolFailure::RepeatedCall)
    ));
    assert!(
        second
            .content
            .contains("repeating it unchanged is not permitted")
    );
    assert!(
        !second.content.contains("Hint:"),
        "the refusal is self-describing; it does not stack another hint"
    );
}

/// Progress re-legitimises a previously failed call: once any tool call in a
/// later turn succeeds, the ledger clears, so an identical call is allowed to
/// run again (and genuinely re-executes).
#[tokio::test]
async fn a_successful_call_clears_the_failure_ledger() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "boom", json!({ "q": 1 }))],
                ),
                usage(5, 1, None),
            ),
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_2", "echo", json!({ "q": "x" }))],
                ),
                usage(5, 1, None),
            ),
            completed(
                assistant(
                    "",
                    StopReason::ToolCalls,
                    vec![tool_call("call_3", "boom", json!({ "q": 1 }))],
                ),
                usage(5, 1, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(5, 1, None),
            ),
        ],
    );
    let (registry, invocations) = registry_for_repeat();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "fail, succeed, fail again", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.successful_tool_calls, 1);
    assert_eq!(summary.failed_tool_calls, 2);
    // boom ran twice: the echo success cleared the ledger between them.
    assert_eq!(*invocations.lock().unwrap(), 2);

    // user, a, r(boom), a, r(echo), a, r(boom), a
    assert_eq!(session.transcript.len(), 8);
    let Item::ToolResult(second) = &session.transcript[4] else {
        panic!("expected the echo result");
    };
    assert_eq!(second.status, ToolStatus::Success);

    let Item::ToolResult(third) = &session.transcript[6] else {
        panic!("expected the second boom result");
    };
    assert!(
        matches!(&third.status, ToolStatus::Error(ToolFailure::Execution)),
        "the third call re-ran instead of being refused"
    );
}

#[tokio::test]
async fn total_token_budget_stops_the_task_before_the_next_request() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant(
                "",
                StopReason::ToolCalls,
                vec![tool_call("call_1", "echo", json!({}))],
            ),
            usage(700, 100, None),
        )],
    )
    .repeat_last();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_with("echo"), 10)
        .with_budget(TaskBudget::new().with_max_total_tokens(1_000));

    let error = agent
        .run_task(&mut session, "loop", &sink, &token)
        .await
        .unwrap_err();

    // Turn 1 reaches 800 (< 1000) and commits; turn 2 reaches 1600 and the
    // turn-3 check refuses to spend more.
    assert!(
        matches!(
            error,
            TaskError::BudgetExhausted {
                limit: "total_tokens"
            }
        ),
        "unexpected error: {error:?}"
    );
    assert!(!error.is_cancelled());
    assert_eq!(error.end_reason(), "budget_exhausted");

    // user + two (assistant + result) turns committed before the stop.
    assert_eq!(session.transcript.len(), 5);
    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.turns, 2);
    assert_eq!(record.end_reason.as_deref(), Some("budget_exhausted"));
}

#[tokio::test]
async fn tool_call_budget_stops_after_the_batch_that_crossed_it() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant(
                "",
                StopReason::ToolCalls,
                vec![tool_call("call_1", "echo", json!({}))],
            ),
            usage(5, 1, None),
        )],
    )
    .repeat_last();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, registry_with("echo"), 10)
        .with_budget(TaskBudget::new().with_max_tool_calls(1));

    let error = agent
        .run_task(&mut session, "loop", &sink, &token)
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            TaskError::BudgetExhausted {
                limit: "tool_calls"
            }
        ),
        "unexpected error: {error:?}"
    );
    // The one allowed batch ran and committed; the next turn then stopped.
    assert_eq!(session.transcript.len(), 3);
    assert_eq!(session.tasks[0].turns, 1);
}

#[tokio::test]
async fn wall_time_budget_can_stop_before_the_first_request() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(5, 1, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10)
        .with_budget(TaskBudget::new().with_max_wall_time(Duration::ZERO));

    let error = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap_err();

    assert!(
        matches!(error, TaskError::BudgetExhausted { limit: "wall_time" }),
        "unexpected error: {error:?}"
    );
    // Only the input user item exists: no request was ever sent.
    assert_eq!(session.transcript.len(), 1);
    assert!(matches!(&session.transcript[0], Item::User { .. }));
}

#[tokio::test]
async fn remaining_budget_is_surfaced_on_success() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(10, 5, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10).with_budget(
        TaskBudget::new()
            .with_max_total_tokens(100)
            .with_max_tool_calls(3)
            .with_max_wall_time(Duration::from_secs(3_600)),
    );

    let summary = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    let remaining = summary.remaining_budget;
    assert_eq!(remaining.total_tokens, Some(85));
    assert_eq!(remaining.tool_calls, Some(3));
    assert_eq!(remaining.input_tokens, None);
    assert_eq!(remaining.output_tokens, None);
    let left = remaining.wall_time.expect("wall-time ceiling was set");
    assert!(left <= Duration::from_secs(3_600));
    assert!(left > Duration::from_secs(3_590));
}

#[tokio::test]
async fn summary_carries_the_final_turn_context_census() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(10, 5, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "inspect the repo", &sink, &token)
        .await
        .unwrap();

    // ADR-016/V18: the compiler attributes the final request's context.
    let census = summary.last_context_census;
    assert!(census.system_prompt > 0, "system prompt attributed");
    assert!(census.user_items > 0, "task item attributed");
    assert!(census.total > 0, "authoritative total is reported");
}

#[tokio::test]
async fn default_budget_is_unbounded_and_reports_no_ceilings() {
    let provider = FakeProvider::new(
        capabilities(),
        vec![completed(
            assistant("done", StopReason::EndTurn, Vec::new()),
            usage(10, 5, None),
        )],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &token)
        .await
        .unwrap();

    assert_eq!(summary.remaining_budget.input_tokens, None);
    assert_eq!(summary.remaining_budget.output_tokens, None);
    assert_eq!(summary.remaining_budget.total_tokens, None);
    assert_eq!(summary.remaining_budget.tool_calls, None);
    assert_eq!(summary.remaining_budget.wall_time, None);
}
