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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream;
use loom_kernel::agent::{Agent, FastPathSolve, RetryPolicy, TaskBudget, TaskControl, TaskError};
use loom_kernel::context::{
    AgentConfig, DefaultContextCompiler, GitState, KnownFacts, PruneConfig, PruningContextCompiler,
    StaticFacts,
};
use loom_kernel::event::{ChannelEventSink, EventEnvelope, EventKind};
use loom_kernel::id::SessionId;
use loom_kernel::model::{
    Message, ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider,
    ModelRequest, ModelResponse, ModelStream, StopReason, Usage,
};
use loom_kernel::session::{PlannedTurn, Session, TaskRecord, TaskStatus, TodoStatus};
use loom_kernel::store::{MemorySessionStore, SessionStore, StoreError};
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolRegistry, ToolSpec,
};
use loom_kernel::transcript::{
    AssistantMessage, Item, ItemIndex, ToolArguments, ToolCall, ToolCallId, ToolFailure, ToolStatus,
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

/// A tool that requests a graceful pause as it runs and then completes
/// normally — makes the pause-vs-turn-boundary outcome deterministic
/// without racing a spawned sleeper against a microsecond-scale turn.
struct PausingTool {
    spec: ToolSpec,
    control: TaskControl,
}

impl Tool for PausingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        self.control.pause();
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

/// A tool that records a file write, optionally with a fused green check —
/// drives the completion gate's mutation/green tracking without a filesystem.
struct WritingTool {
    spec: ToolSpec,
    green: bool,
    /// The command the fused check echoes; "cargo test" makes the
    /// result count as a test run for the completion gate.
    check: &'static str,
}

impl Tool for WritingTool {
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
        let mut effects = ToolEffects::default().with_file_written("src/lib.rs".to_owned());
        effects = effects.with_command(CommandEffect::new(
            "cargo",
            vec!["check".to_owned()],
            Some(0),
        ));
        let content = if self.green {
            format!(
                "edited src/lib.rs\n--- verify ---\n$ {}\nexit status: 0",
                self.check
            )
        } else {
            String::from("edited src/lib.rs")
        };
        Box::pin(async move { Ok(ToolOutput::new(content).with_effects(effects)) })
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
        Box::new(DefaultContextCompiler::default()),
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
        thinking_tokens: None,
        cache_creation_input_tokens: None,
        cost_usd: None,
    }
}

#[tokio::test]
async fn repository_facts_are_projected_as_a_stable_system_block() {
    // 2E (ADR-015): the facts block is a deterministic second system message,
    // identical in every turn of the task, so "what changed?" is answered
    // without a model turn and the cached prefix is never churned.
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "calling",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "echo", json!({"q": "hi"}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(20, 5, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let facts = KnownFacts::new(
        PathBuf::from("/ws"),
        vec!["Rust".to_owned()],
        vec!["Cargo".to_owned()],
        vec!["src".to_owned()],
        vec!["tests".to_owned()],
        vec!["Cargo.toml".to_owned()],
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        GitState::new(Some("main".to_owned()), vec!["src/lib.rs".to_owned()]),
        None,
        None,
    );
    let agent = Agent::new(
        Box::new(provider),
        Box::new(DefaultContextCompiler::default()),
        registry_with("echo"),
        AgentConfig::new(PathBuf::from("."), "you are a test agent"),
        "fake-model",
    )
    .with_max_turns(10)
    .with_facts_source(Box::new(StaticFacts::new(facts)));

    let summary = agent
        .run_task(&mut session, "what changed?", &sink, &control)
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(
            matches!(&request.messages[0], Message::System { text } if text == "you are a test agent")
        );
        let Message::System { text } = &request.messages[1] else {
            panic!("the facts block must follow the system prompt");
        };
        assert!(text.contains("Repository facts"), "{text}");
        assert!(text.contains("changed files: src/lib.rs"), "{text}");
    }
    // Byte-identical across turns.
    assert_eq!(requests[0].messages[1], requests[1].messages[1]);
    drop(requests);
    assert!(summary.last_context_census.repo_facts > 0);
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "hello", &sink, &control)
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
async fn an_unverified_finish_is_refused_until_the_cap_then_accepted() {
    // Roadmap item 8: a write lands, the model tries to finish without a
    // green check, the loop injects one refusal quoting the changed path
    // and the original task, and a second EndTurn completes the task.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(WritingTool {
            spec: spec("write"),
            green: false,
            check: "cargo check",
        }))
        .unwrap();
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "working",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "write", json!({}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(20, 5, None),
            ),
            completed(
                assistant("really done", StopReason::EndTurn, Vec::new()),
                usage(30, 7, None),
            ),
            completed(
                assistant("truly done", StopReason::EndTurn, Vec::new()),
                usage(40, 9, None),
            ),
            completed(
                assistant("actually done", StopReason::EndTurn, Vec::new()),
                usage(50, 11, None),
            ),
            completed(
                assistant("final answer", StopReason::EndTurn, Vec::new()),
                usage(60, 13, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "fix the bug", &sink, &control)
        .await
        .unwrap();

    // Six model requests: write, three green-check refusals (the cap),
    // one test-consultation refusal, accepted finish.
    assert_eq!(summary.model_requests, 6);
    // Schema 11: every loop-owned nudge is also an event, not just a
    // transcript item — both refusals surface here.
    let mut nudges = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let EventKind::LoopNudge { text } = event.kind {
            nudges.push(text);
        }
    }
    let green_nudges: Vec<&String> = nudges
        .iter()
        .filter(|t| t.contains("verification reminder"))
        .collect();
    assert_eq!(green_nudges.len(), 3, "green refusals fire up to the cap");
    assert!(green_nudges[0].contains("reminder 1 of 3"));
    assert!(green_nudges[1].contains("reminder 2 of 3"));
    assert!(green_nudges[2].contains("reminder 3 of 3"));
    assert!(
        nudges.iter().any(|t| t.contains("no existing test suite")),
        "the one-shot test-consultation refusal fires"
    );
    let requests = requests.lock().unwrap();
    let refusal_at = |i: usize, needle: &str| {
        requests[i]
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { text } if text.contains(needle)))
    };
    assert!(refusal_at(2, "verification reminder 1 of 3"));
    assert!(refusal_at(3, "verification reminder 2 of 3"));
    assert!(refusal_at(4, "verification reminder 3 of 3"));
    assert!(
        refusal_at(5, "no existing test suite"),
        "the test-consultation refusal must reach the model"
    );
    drop(requests);

    // Both refusals persist in the transcript as user items.
    assert!(
        session
            .transcript
            .iter()
            .any(|i| matches!(i, Item::User { text } if text.contains("Hold on")))
    );
    assert_eq!(summary.turns, 6);
}

#[tokio::test]
async fn the_bare_loop_never_refuses_an_unverified_finish() {
    // Roadmap item 1's bare-loop arm: with `with_loop_shaping(false)`
    // the identical write-then-finish script must complete in two
    // requests — no refusal turn, no injected user item — so A/B runs
    // measure the gate's value rather than assume it.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(WritingTool {
            spec: spec("write"),
            green: false,
            check: "cargo check",
        }))
        .unwrap();
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "working",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "write", json!({}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(20, 5, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10).with_loop_shaping(false);

    let summary = agent
        .run_task(&mut session, "fix the bug", &sink, &control)
        .await
        .unwrap();

    // Two model requests: write, finish. The refusal never fired.
    assert_eq!(summary.model_requests, 2);
    assert!(requests.lock().unwrap().len() == 2);
    assert!(
        !session
            .transcript
            .iter()
            .any(|i| matches!(i, Item::User { text } if text.contains("Hold on"))),
        "the bare loop must not inject a completion refusal"
    );
    assert_eq!(summary.turns, 2);
}

#[tokio::test]
async fn a_fused_green_check_still_gets_one_test_consultation_refusal() {
    // The fused `exit status: 0` check satisfies the verification gate,
    // but `cargo check` is not a test run: the first EndTurn gets the
    // once-only test-consultation nudge, the second completes.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(WritingTool {
            spec: spec("write"),
            green: true,
            check: "cargo check",
        }))
        .unwrap();
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "working",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "write", json!({}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(20, 5, None),
            ),
            completed(
                assistant("really done", StopReason::EndTurn, Vec::new()),
                usage(30, 7, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "fix the bug", &sink, &control)
        .await
        .unwrap();

    assert_eq!(summary.model_requests, 3, "one nudge turn, then done");
    let requests = requests.lock().unwrap();
    assert!(
        requests[2].messages.iter().any(
            |m| matches!(m, Message::User { text } if text.contains("no existing test suite"))
        ),
        "the test-consultation nudge must reach the model"
    );
    assert!(
        !requests.iter().any(|r| r.messages.iter().any(
            |m| matches!(m, Message::User { text } if text.contains("since the last green check"))
        )),
        "the verification refusal must not fire when the check is green"
    );
}

#[tokio::test]
async fn a_fused_green_test_run_finishes_without_any_refusal() {
    // A fused green check whose command IS a test run satisfies both
    // gates: the first EndTurn completes the task.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(WritingTool {
            spec: spec("write"),
            green: true,
            check: "cargo test",
        }))
        .unwrap();
    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "working",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "write", json!({}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(20, 5, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "fix the bug", &sink, &control)
        .await
        .unwrap();

    assert_eq!(
        summary.model_requests, 2,
        "no refusal when the check is green"
    );
    let requests = requests.lock().unwrap();
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { text } if text.contains("Hold on")))
    );
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_with("echo"), 10);

    let summary = agent
        .run_task(&mut session, "run the tool", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_with("echo"), 3);

    let error = agent
        .run_task(&mut session, "loop", &sink, &control)
        .await
        .unwrap_err();

    assert!(matches!(error, TaskError::MaxTurns { max_turns: 3 }));
    // user + 3 x (assistant + result), plus the turn-budget notice the
    // loop injects once at three fifths of the budget (turn 2 of 3).
    assert_eq!(session.transcript.len(), 8);
    assert!(
        session
            .transcript
            .iter()
            .any(|item| matches!(item, Item::User { text } if text.contains("of 3 turns"))),
        "the turn-budget notice is part of the transcript"
    );
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    control.cancel();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
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
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::from(token.clone());
    let provider = FakeProvider::new(capabilities(), Vec::new()).cancel_on_stream(token.clone());
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let error = agent
        .run_task(&mut session, "go", &sink, &control)
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

/// A pause requested before the task starts nothing: no model request,
/// no turn, and the record ends `paused` — not `cancelled`, not
/// `failed`.
#[tokio::test]
async fn pause_before_the_first_turn_starts_nothing() {
    let provider = FakeProvider::new(capabilities(), Vec::new());
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    control.pause();
    let error = agent
        .run_task(&mut session, "go", &sink, &control)
        .await
        .unwrap_err();

    assert!(matches!(&error, TaskError::Paused));
    assert!(!error.is_cancelled());
    assert_eq!(error.end_reason(), "paused");
    assert!(requests.lock().unwrap().is_empty(), "no model request");
    // Only the user item; no turn ever ran.
    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.tasks[0].status, TaskStatus::Paused);
    assert_eq!(session.tasks[0].end_reason.as_deref(), Some("paused"));

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskPaused));
}

/// A pause that fires mid-batch still finishes the turn: the tool's
/// result commits, the reply stays in the transcript, and the loop
/// stops before spending the next request. The scripted second turn
/// is never consumed.
#[tokio::test]
async fn pause_during_a_batch_finishes_the_in_flight_turn() {
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
    let control = TaskControl::new();
    // The tool pauses the task as it runs; the batch still completes and
    // commits before the loop stops.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(PausingTool {
            spec: spec("echo"),
            control: control.clone(),
        }))
        .unwrap();
    let agent = agent(provider, registry, 10);

    let error = agent
        .run_task(&mut session, "run the tool", &sink, &control)
        .await
        .unwrap_err();

    assert!(matches!(&error, TaskError::Paused));
    assert_eq!(session.tasks[0].status, TaskStatus::Paused);
    assert_eq!(session.tasks[0].turns, 1);
    // Only the first scripted turn was spent.
    assert_eq!(requests.lock().unwrap().len(), 1);
    // user, assistant(call), tool result — the turn committed in full.
    assert_eq!(session.transcript.len(), 3);

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last().unwrap(), EventKind::TaskPaused));
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::TaskCancelled)),
        "a pause is not a cancellation"
    );
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_for_cancel_batch(), 10);

    let error = agent
        .run_task(&mut session, "run both", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_with("echo"), 10);

    agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "run it twice", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry, 10);

    let summary = agent
        .run_task(&mut session, "fail, succeed, fail again", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_with("echo"), 10)
        .with_budget(TaskBudget::new().with_max_total_tokens(1_000));

    let error = agent
        .run_task(&mut session, "loop", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, registry_with("echo"), 10)
        .with_budget(TaskBudget::new().with_max_tool_calls(1));

    let error = agent
        .run_task(&mut session, "loop", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10)
        .with_budget(TaskBudget::new().with_max_wall_time(Duration::ZERO));

    let error = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10).with_budget(
        TaskBudget::new()
            .with_max_total_tokens(100)
            .with_max_tool_calls(3)
            .with_max_wall_time(Duration::from_secs(3_600)),
    );

    let summary = agent
        .run_task(&mut session, "go", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "inspect the repo", &sink, &control)
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
    let control = TaskControl::new();
    let agent = agent(provider, ToolRegistry::new(), 10);

    let summary = agent
        .run_task(&mut session, "go", &sink, &control)
        .await
        .unwrap();

    assert_eq!(summary.remaining_budget.input_tokens, None);
    assert_eq!(summary.remaining_budget.output_tokens, None);
    assert_eq!(summary.remaining_budget.total_tokens, None);
    assert_eq!(summary.remaining_budget.tool_calls, None);
    assert_eq!(summary.remaining_budget.wall_time, None);
}

#[tokio::test]
async fn cancellation_during_retry_backoff_stops_the_task() {
    // One retryable failure, then a long backoff. Cancellation arrives while
    // the loop is sleeping, which is the one cancellation window not covered
    // by the stream/batch tests (§6.3: cancellation wins over retry).
    let provider = FakeProvider::new(
        capabilities(),
        vec![retryable(ModelErrorKind::Server, "lost")],
    );
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent = agent_with_retry(
        provider,
        ToolRegistry::new(),
        10,
        RetryPolicy::new()
            .with_initial_backoff(Duration::from_secs(30))
            .with_jitter_fraction(0.0),
    );

    let canceller = control.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });
    let error = agent
        .run_task(&mut session, "hello", &sink, &control)
        .await
        .expect_err("cancelled while backing off");
    handle.await.ok();

    assert!(error.is_cancelled());

    // Exactly one attempt, one scheduled retry, no completion, terminal cancel.
    let kinds = drain(&mut receiver);
    assert!(matches!(kinds[0], EventKind::TaskStarted));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::ModelRequestStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::ModelRetry { .. }))
            .count(),
        1
    );
    assert!(
        !kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ModelCompleted { .. })),
        "a failed attempt must not surface a completion"
    );
    assert!(matches!(kinds.last(), Some(EventKind::TaskCancelled)));

    // Nothing beyond the user item was committed; the record is cancelled.
    assert_eq!(session.transcript.len(), 1);
    assert!(matches!(&session.transcript[0], Item::User { .. }));
    assert_eq!(session.tasks[0].status, TaskStatus::Cancelled);
    assert_eq!(
        session.tasks[0].end_reason.as_deref(),
        Some("user_cancelled")
    );
}

// ------------------------------------------------------------------ store ---

/// A [`SessionStore`] that can be told to fail the turn append, so the tests
/// can prove persist-then-commit: the store is written *before* the in-memory
/// transcript, so a store failure leaves memory untouched.
struct TestStore {
    inner: MemorySessionStore,
    fail_turn: AtomicBool,
}

impl SessionStore for TestStore {
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError> {
        self.inner.ensure_session(session)
    }

    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError> {
        self.inner.append_item(session, index, item)
    }

    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError> {
        if self.fail_turn.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("disk is full".to_owned()));
        }
        self.inner.append_turn(session, turn)
    }

    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError> {
        self.inner.upsert_task(session, record)
    }

    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError> {
        self.inner.load(session)
    }

    fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        self.inner.session_ids()
    }
}

/// Hands the agent a `Box<dyn SessionStore>` while the test keeps the `Arc` to
/// inspect what was persisted.
struct StoreHandle(Arc<TestStore>);

impl StoreHandle {
    fn new(store: &Arc<TestStore>) -> Self {
        Self(Arc::clone(store))
    }
}

impl SessionStore for StoreHandle {
    fn ensure_session(&self, session: &SessionId) -> Result<(), StoreError> {
        self.0.ensure_session(session)
    }

    fn append_item(
        &self,
        session: &SessionId,
        index: ItemIndex,
        item: &Item,
    ) -> Result<(), StoreError> {
        self.0.append_item(session, index, item)
    }

    fn append_turn(&self, session: &SessionId, turn: &PlannedTurn) -> Result<(), StoreError> {
        self.0.append_turn(session, turn)
    }

    fn upsert_task(&self, session: &SessionId, record: &TaskRecord) -> Result<(), StoreError> {
        self.0.upsert_task(session, record)
    }

    fn load(&self, session: &SessionId) -> Result<Option<Session>, StoreError> {
        self.0.load(session)
    }

    fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        self.0.session_ids()
    }
}

#[tokio::test]
async fn a_store_failure_fails_the_task_without_committing_memory() {
    // The turn append is the one durable write the loop makes per model
    // response. When it fails, persist-then-commit means the in-memory
    // transcript never gains the turn, and the task fails as `Persistence`.
    let store = Arc::new(TestStore {
        inner: MemorySessionStore::new(),
        fail_turn: AtomicBool::new(true),
    });
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
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent =
        agent(provider, registry_with("echo"), 10).with_store(Box::new(StoreHandle::new(&store)));

    let error = agent
        .run_task(&mut session, "run the tool", &sink, &control)
        .await
        .expect_err("the store rejected the turn");

    assert!(matches!(error, TaskError::Persistence(_)));
    assert_eq!(error.end_reason(), "persistence");

    // Persist-then-commit: only the user item (persisted before the turn) and
    // the task record exist in memory; the failed assistant turn does not.
    assert_eq!(session.transcript.len(), 1);
    assert!(matches!(&session.transcript[0], Item::User { .. }));
    let stored = store.load(&session.id).unwrap().unwrap();
    assert_eq!(stored.transcript.len(), 1);

    let record = &session.tasks[0];
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.end_reason.as_deref(), Some("persistence"));

    let kinds = drain(&mut receiver);
    assert!(matches!(kinds.last(), Some(EventKind::TaskFailed { .. })));
}

#[tokio::test]
async fn a_committed_task_is_mirrored_in_the_store() {
    let store = Arc::new(TestStore {
        inner: MemorySessionStore::new(),
        fail_turn: AtomicBool::new(false),
    });
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
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let agent =
        agent(provider, registry_with("echo"), 10).with_store(Box::new(StoreHandle::new(&store)));

    agent
        .run_task(&mut session, "run the tool", &sink, &control)
        .await
        .unwrap();

    // The durable record mirrors the completed in-memory transcript and task.
    let stored = store.load(&session.id).unwrap().unwrap();
    assert_eq!(stored.transcript, session.transcript);
    assert_eq!(stored.tasks, session.tasks);
    assert_eq!(stored.tasks[0].status, TaskStatus::Completed);
    assert_eq!(stored.tasks[0].turns, 2);
    assert_eq!(store.session_ids().unwrap(), vec![session.id]);
}

#[tokio::test]
async fn a_persisted_session_resumes_from_the_store() {
    // A frontend can drop its in-memory handle and resume: load the session
    // from the store and keep running tasks against it. The second run must
    // see the first run's transcript and task record.
    let store = Arc::new(TestStore {
        inner: MemorySessionStore::new(),
        fail_turn: AtomicBool::new(false),
    });
    let id = SessionId::new();
    let mut session = Session::new(id);
    let control = TaskControl::new();

    let first = agent(
        FakeProvider::new(
            capabilities(),
            vec![completed(
                assistant("one", StopReason::EndTurn, Vec::new()),
                usage(10, 2, None),
            )],
        ),
        ToolRegistry::new(),
        10,
    )
    .with_store(Box::new(StoreHandle::new(&store)));
    let (sink, _receiver) = ChannelEventSink::new();
    first
        .run_task(&mut session, "first task", &sink, &control)
        .await
        .unwrap();

    // Resume from the durable form, not the handle the first run used.
    let mut resumed = store.load(&id).unwrap().unwrap();
    assert_eq!(resumed.transcript, session.transcript);

    let second = agent(
        FakeProvider::new(
            capabilities(),
            vec![completed(
                assistant("two", StopReason::EndTurn, Vec::new()),
                usage(12, 3, None),
            )],
        ),
        ToolRegistry::new(),
        10,
    )
    .with_store(Box::new(StoreHandle::new(&store)));
    let (sink, _receiver) = ChannelEventSink::new();
    second
        .run_task(&mut resumed, "second task", &sink, &control)
        .await
        .unwrap();

    // Both tasks are recorded and the transcript continues in place.
    assert_eq!(resumed.transcript.len(), 4);
    assert!(matches!(&resumed.transcript[2], Item::User { text } if text == "second task"));
    assert_eq!(resumed.tasks.len(), 2);
    assert_eq!(resumed.tasks[0].turns, 1);
    assert_eq!(resumed.tasks[1].turns, 1);

    let stored = store.load(&id).unwrap().unwrap();
    assert_eq!(stored.transcript, resumed.transcript);
    assert_eq!(stored.tasks, resumed.tasks);
    assert_eq!(store.session_ids().unwrap(), vec![id]);
}

/// A mutating tool whose success result is enormous — written results
/// are unprunable by every rescue pass, so the only way past turn 2 is
/// the LLM handoff (roadmap item 12).
struct HugeTool {
    spec: ToolSpec,
}

impl Tool for HugeTool {
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
        // Mutating tool + success status alone: unprunable by every
        // rescue pass, and no file effects to trip the completion
        // gate's changed-since-green continuation.
        Box::pin(async move {
            Ok(ToolOutput::new(format!(
                "wrote src/big.rs\n{}",
                "y".repeat(300_000)
            )))
        })
    }
}

/// A context window big enough for the opening turns but far too small
/// for the huge written result, forcing a turn-2 `BudgetExceeded`.
fn small_capabilities() -> ModelCapabilities {
    ModelCapabilities::new(4_000)
        .with_max_output_tokens(1_024)
        .with_tool_calls(true)
}

fn handoff_agent(provider: FakeProvider, handoff: bool) -> Agent {
    let mut config = AgentConfig::new(PathBuf::from("."), "you are a test agent");
    config.handoff_compaction = handoff;
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(HugeTool { spec: spec("huge") }))
        .unwrap();
    Agent::new(
        Box::new(provider),
        Box::new(PruningContextCompiler::new(
            Box::new(DefaultContextCompiler::default()),
            PruneConfig::default(),
        )),
        registry,
        config,
        "fake-model",
    )
    .with_max_turns(10)
    .with_retry(
        RetryPolicy::new()
            .with_initial_backoff(Duration::from_millis(1))
            .with_jitter_fraction(0.0),
    )
}

/// Item 12: when the projection cannot fit and the deterministic rescue
/// has nothing left to reclaim, the loop buys one provider call — a
/// goal-directed handoff — and continues on the summarized view instead
/// of failing the task.
#[tokio::test]
async fn budget_overrun_with_unprunable_history_falls_through_to_handoff() {
    let provider = FakeProvider::new(
        small_capabilities(),
        vec![
            completed(
                assistant(
                    "calling",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "huge", json!({}))],
                ),
                usage(10, 3, None),
            ),
            // The handoff call: a terse EndTurn summary.
            completed(
                assistant("SUMMARY: keep going", StopReason::EndTurn, Vec::new()),
                usage(50, 20, None),
            ),
            // The post-handoff turn continues on the view.
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(30, 5, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let agent = handoff_agent(provider, true);
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    agent
        .run_task(&mut session, "explore the tree", &sink, &control)
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "turn call, handoff, final turn");

    // The handoff request is a plain two-message completion.
    let handoff = &requests[1];
    assert!(handoff.tools.is_empty(), "no tools on the handoff call");
    assert_eq!(handoff.max_tokens, Some(1_024));
    assert_eq!(handoff.messages.len(), 2);
    assert!(matches!(&handoff.messages[0], Message::System { .. }));
    assert!(
        matches!(&handoff.messages[1], Message::User { text } if text.contains("## Task")),
        "digest carries the task section"
    );

    // The final turn runs on the handoff view: brief + summary, never
    // the huge written result.
    let final_messages = serde_json::to_string(&requests[2].messages).unwrap();
    assert!(final_messages.contains("explore the tree"), "brief leads");
    assert!(
        final_messages.contains("SUMMARY: keep going"),
        "handoff text"
    );
    assert!(!final_messages.contains("yyyy"), "history summarized away");

    let events = drain(&mut receiver);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventKind::ContextHandoff { turn: 2, chars: 19 }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventKind::TaskCompleted)),
        "task still completes"
    );
}

/// `handoff_compaction: false` (the engine's bare arm) keeps the
/// deterministic-only contract: the budget error propagates.
#[tokio::test]
async fn handoff_disabled_propagates_the_budget_error() {
    let provider = FakeProvider::new(
        small_capabilities(),
        vec![completed(
            assistant(
                "calling",
                StopReason::ToolCalls,
                vec![tool_call("call_1", "huge", json!({}))],
            ),
            usage(10, 3, None),
        )],
    );
    let requests = provider.requests_handle();
    let agent = handoff_agent(provider, false);
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let error = agent
        .run_task(&mut session, "explore the tree", &sink, &control)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        TaskError::Context(loom_kernel::context::ContextError::BudgetExceeded { .. })
    ));
    assert_eq!(requests.lock().unwrap().len(), 1, "no handoff call spent");
}

/// The handoff is only accepted as a plain EndTurn summary: a scripted
/// tool-call response falls through to the original budget error.
#[tokio::test]
async fn handoff_guard_rejects_a_scripted_tool_call_response() {
    let provider = FakeProvider::new(
        small_capabilities(),
        vec![
            completed(
                assistant(
                    "calling",
                    StopReason::ToolCalls,
                    vec![tool_call("call_1", "huge", json!({}))],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant(
                    "trying to keep calling",
                    StopReason::ToolCalls,
                    vec![tool_call("call_2", "huge", json!({}))],
                ),
                usage(10, 3, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let agent = handoff_agent(provider, true);
    let mut session = Session::new(SessionId::new());
    let (sink, _receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let error = agent
        .run_task(&mut session, "explore the tree", &sink, &control)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        TaskError::Context(loom_kernel::context::ContextError::BudgetExceeded { .. })
    ));
    assert_eq!(requests.lock().unwrap().len(), 2, "handoff rejected");
}

#[tokio::test]
async fn record_fast_path_books_a_zero_request_completion() {
    let provider = FakeProvider::new(capabilities(), vec![]);
    let requests = provider.requests_handle();
    let agent = agent(provider, registry_with("echo"), 5);
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();

    let summary = agent
        .record_fast_path(
            &mut session,
            "Rename parse_opts to parse_options",
            FastPathSolve {
                kind: "rename".to_owned(),
                files_changed: 2,
                note: "Fast path (rename): `parse_opts` → `parse_options` \
                       across 2 file(s); verified with `cargo check`."
                    .to_owned(),
            },
            &sink,
        )
        .await
        .expect("recorded");

    // Zero model traffic, and the proof is in the accounting.
    assert_eq!(summary.model_requests, 0);
    assert_eq!(summary.turns, 0);
    assert_eq!(summary.input_tokens, 0);
    assert_eq!(summary.output_tokens, 0);
    assert!(requests.lock().unwrap().is_empty());

    // Transcript: the instruction and the solve's note, nothing else.
    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(session.transcript[0], Item::User { ref text } if text.contains("Rename")));
    match &session.transcript[1] {
        Item::Assistant(message) => {
            assert!(message.content.contains("parse_options"));
            assert_eq!(message.stop_reason, StopReason::EndTurn);
            assert!(message.tool_calls.is_empty());
        }
        other => panic!("expected an assistant item, got {other:?}"),
    }

    // The record finishes with the fast-path end reason.
    let record = session.tasks.last().expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.end_reason.as_deref(), Some("fast_path"));
    assert_eq!(record.turns, 0);

    // Event order: started, the hit marker, completed.
    let kinds = drain(&mut receiver);
    let positions: Vec<usize> = kinds
        .iter()
        .position(|kind| matches!(kind, EventKind::TaskStarted))
        .into_iter()
        .chain(
            kinds
                .iter()
                .position(|kind| matches!(kind, EventKind::FastPathHit { .. })),
        )
        .chain(
            kinds
                .iter()
                .position(|kind| matches!(kind, EventKind::TaskCompleted)),
        )
        .collect();
    assert_eq!(positions, vec![0, 1, 2], "events: {kinds:?}");
}

/// Roadmap item 8 phase B: the kernel-owned `todo` tool through the real
/// loop — the tail projection rule.
///
/// The current list is re-appended as a user item at the transcript tail
/// only when a turn's tool batch changed it, and projection keeps every
/// appended block in place (append-only), so:
///
/// * the request compiled right after a change ends with the fresh block;
/// * every earlier request is a byte-identical *prefix* of every later
///   one (eliding superseded blocks would rewrite that prefix mid-task
///   and forfeit provider cache hits past the elision point);
/// * a quiet turn — a batch that did not touch the list — appends
///   nothing and still extends the projection by pure prefix growth.
#[tokio::test]
async fn todo_list_re_appends_at_the_tail_only_when_changed() {
    // Registry: the real kernel tool plus a read-only echo for the quiet
    // turn. The todo tool mutates, so its calls serialize in model order
    // (the virtual write cell), which is what makes two adds in one
    // batch assign ids deterministically.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(loom_kernel::tool::TodoTool::new()))
        .unwrap();
    registry
        .register(Box::new(FakeTool {
            spec: spec("echo"),
            read_only: true,
        }))
        .unwrap();

    let provider = FakeProvider::new(
        capabilities(),
        vec![
            // Turn 1: two adds in one batch.
            completed(
                assistant(
                    "planning",
                    StopReason::ToolCalls,
                    vec![
                        tool_call("call_1", "todo", json!({"op": "add", "text": "step one"})),
                        tool_call("call_2", "todo", json!({"op": "add", "text": "step two"})),
                    ],
                ),
                usage(10, 3, None),
            ),
            // Turn 2: a mark — the list changes again.
            completed(
                assistant(
                    "working",
                    StopReason::ToolCalls,
                    vec![tool_call(
                        "call_3",
                        "todo",
                        json!({"op": "mark", "id": "1", "status": "done"}),
                    )],
                ),
                usage(20, 4, None),
            ),
            // Turn 3: a quiet batch — no todo call, so no new block.
            completed(
                assistant(
                    "checking",
                    StopReason::ToolCalls,
                    vec![tool_call("call_4", "echo", json!({"q": "hi"}))],
                ),
                usage(30, 5, None),
            ),
            // Turn 4: done.
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(40, 6, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let (sink, mut receiver) = ChannelEventSink::new();
    let agent = agent(provider, registry, 10);

    let mut session = Session::new(SessionId::new());
    let summary = agent
        .run_task(&mut session, "do the thing", &sink, &TaskControl::default())
        .await
        .expect("task completes");
    assert_eq!(summary.turns, 4);

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);

    // Count the todo blocks in a request's projection.
    let todo_blocks = |request: &ModelRequest| -> Vec<String> {
        request
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User { text } if text.starts_with("## todo (kernel)") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    };

    // Turn 1's compile ran before any todo op: no block.
    assert!(todo_blocks(&requests[0]).is_empty(), "no list existed yet");

    // Turn 2's compile, after the two adds: exactly one fresh block at
    // the tail, with both steps pending and stable ids.
    let first_block = todo_blocks(&requests[1]);
    assert_eq!(
        first_block,
        vec!["## todo (kernel)\n- [ ] 1: step one\n- [ ] 2: step two".to_owned()],
        "the changed list is re-appended once, at the tail"
    );
    assert!(matches!(
        requests[1].messages.last(),
        Some(Message::User { text }) if text.starts_with("## todo (kernel)")
    ));

    // Turn 3's compile, after the mark: the fresh block at the tail with
    // the superseded block still in place, and the entire previous
    // request a byte-identical prefix.
    let second_block = todo_blocks(&requests[2]);
    assert_eq!(
        second_block,
        vec![
            "## todo (kernel)\n- [ ] 1: step one\n- [ ] 2: step two".to_owned(),
            "## todo (kernel)\n- [x] 1: step one\n- [ ] 2: step two".to_owned(),
        ],
        "every appended block projects; the newest lands at the tail"
    );
    assert_eq!(
        requests[1].messages,
        requests[2].messages[..requests[1].messages.len()],
        "append-only projection keeps the earlier request a byte-identical prefix"
    );
    assert_eq!(
        requests[2].messages.len() - requests[1].messages.len(),
        3,
        "a changed list adds its assistant turn, its tool result, and the fresh block"
    );

    // Turn 4's compile, after the quiet echo turn: no new block, and the
    // previous request is again a pure prefix extension.
    assert_eq!(
        todo_blocks(&requests[3]),
        second_block,
        "an unchanged list is not re-appended"
    );
    assert_eq!(
        requests[2].messages,
        requests[3].messages[..requests[2].messages.len()],
        "a quiet turn grows the projection by prefix only"
    );
    assert_eq!(
        requests[3].messages.len() - requests[2].messages.len(),
        2,
        "the quiet turn adds its assistant turn and its tool result"
    );

    // Session state: the tool's mutations were copied back.
    assert_eq!(session.todos.items.len(), 2);
    assert_eq!(session.todos.items[0].status, TodoStatus::Done);
    // Transcript: one appended block per change (two total), each after
    // the turn that caused it.
    let appended = session
        .transcript
        .iter()
        .filter(|item| matches!(item, Item::User { text } if text.starts_with("## todo (kernel)")))
        .count();
    assert_eq!(appended, 2, "one durable block per change");

    // Events: exactly one TodoUpdated per changing turn, none for the
    // quiet turn, between the tool events and the terminal ones.
    let kinds = drain(&mut receiver);
    let updates = kinds
        .iter()
        .filter(|kind| matches!(kind, EventKind::TodoUpdated { .. }))
        .count();
    assert_eq!(updates, 2, "events: {kinds:?}");
}

/// Salvage path (malformed-arguments recovery): a tool call whose
/// arguments carry the malformed marker never executes; the model sees
/// an error result instead, reissues a well-formed call, and the task
/// still completes.
#[tokio::test]
async fn malformed_call_is_refused_and_recovered_from() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(loom_kernel::tool::TodoTool::new()))
        .unwrap();

    let provider = FakeProvider::new(
        capabilities(),
        vec![
            completed(
                assistant(
                    "planning",
                    StopReason::ToolCalls,
                    vec![tool_call(
                        "call_1",
                        "todo",
                        json!({"_loom_malformed_arguments": "not json"}),
                    )],
                ),
                usage(10, 3, None),
            ),
            completed(
                assistant(
                    "retrying",
                    StopReason::ToolCalls,
                    vec![tool_call(
                        "call_2",
                        "todo",
                        json!({"op": "add", "text": "step one"}),
                    )],
                ),
                usage(20, 4, None),
            ),
            completed(
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(30, 5, None),
            ),
        ],
    );
    let (sink, _receiver) = ChannelEventSink::new();
    let agent = agent(provider, registry, 10);

    let mut session = Session::new(SessionId::new());
    let summary = agent
        .run_task(&mut session, "do the thing", &sink, &TaskControl::default())
        .await
        .expect("task completes despite the malformed call");
    assert_eq!(summary.turns, 3);

    // The malformed call never executed; the reissued valid call did.
    assert_eq!(session.todos.items.len(), 1);
    assert_eq!(session.todos.items[0].text, "step one");
}

/// Three consecutive all-malformed turns trip the ceiling and fail the
/// task instead of burning the turn budget.
#[tokio::test]
async fn three_consecutive_malformed_turns_fail_the_task() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(loom_kernel::tool::TodoTool::new()))
        .unwrap();

    let malformed = || {
        completed(
            assistant(
                "garbled",
                StopReason::ToolCalls,
                vec![tool_call(
                    "call_1",
                    "todo",
                    json!({"_loom_malformed_arguments": "not json"}),
                )],
            ),
            usage(10, 3, None),
        )
    };
    let provider = FakeProvider::new(capabilities(), vec![malformed(), malformed(), malformed()]);
    let (sink, _receiver) = ChannelEventSink::new();
    let agent = agent(provider, registry, 10);

    let mut session = Session::new(SessionId::new());
    let error = agent
        .run_task(&mut session, "do the thing", &sink, &TaskControl::default())
        .await
        .expect_err("the third consecutive malformed turn fails the task");
    assert!(matches!(
        error,
        loom_kernel::agent::TaskError::Protocol(ref message)
            if message == "model emitted malformed tool-call arguments in 3 consecutive turns"
    ));
}

/// A tool that queues steering mid-batch and completes normally — the
/// queued message must land in the transcript at the next turn boundary
/// and be visible to that turn's compiled request (item 25).
struct SteeringTool {
    spec: ToolSpec,
    control: TaskControl,
}

impl Tool for SteeringTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn call<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        self.control.steer("actually, prefer the second option");
        Box::pin(async move { Ok(ToolOutput::new(format!("echo: {input}"))) })
    }
}

/// Steering queued during a turn's tool batch joins the transcript at the
/// next boundary: one `SteeringApplied` event, and the second request's
/// messages contain the steered user text.
#[tokio::test]
async fn steering_queued_mid_batch_is_applied_at_the_next_turn_boundary() {
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
                assistant("done", StopReason::EndTurn, Vec::new()),
                usage(30, 7, None),
            ),
        ],
    );
    let requests = provider.requests_handle();
    let mut session = Session::new(SessionId::new());
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(SteeringTool {
            spec: spec("echo"),
            control: control.clone(),
        }))
        .unwrap();
    let agent = agent(provider, registry, 10);

    agent
        .run_task(&mut session, "run the tool", &sink, &control)
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "both turns spent");
    let second = &requests[1];
    assert!(
        second.messages.iter().any(|message| matches!(
            message,
            Message::User { text } if text.contains("prefer the second option")
        )),
        "the steered text must reach the second turn's compile"
    );
    // user, assistant(call), tool result, steered user, final assistant.
    assert_eq!(session.transcript.len(), 5);

    let kinds = drain(&mut receiver);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(
                kind,
                EventKind::SteeringApplied {
                    turn: 2,
                    messages: 1
                }
            ))
            .count(),
        1,
        "events: {kinds:?}"
    );
}
