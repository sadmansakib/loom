//! The read-only scout (roadmap item 19, iteration 3).
//!
//! One delegation level, bounded hard: a cheap model answers a capsule
//! question over the workspace with read-only tools and returns a
//! ≤1 KB finding. The parent's context pays only the capsule and the
//! answer — the exploration the scout performed never enters it.
//!
//! The runtime ships the seam and the bounds; the delegation economics
//! (when answering locally would cost more parent context than the
//! child spend) are measured offline by the eval, per the roadmap.
//! Recursion is structurally impossible: the child registry is the
//! read-only subset of the parent's, and `scout` itself is not
//! read-only-exempt — it is simply never registered for the child.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use loom_kernel::agent::{Agent, TaskControl};
use loom_kernel::event::{EventKind, EventSink};
use loom_kernel::id::{SessionId, TaskId};
use loom_kernel::session::Session;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use loom_kernel::transcript::Item;
use serde_json::{Value, json};

/// Hard answer bound: the capsule contract promises the parent ≤1 KB.
pub const ANSWER_BUDGET_BYTES: usize = 1_000;

/// Configuration for the scout child agent (roadmap item 19).
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    /// Provider adapter for the child — mid-task model selection applies
    /// to the child only; the parent's model stays fixed.
    pub provider: String,
    /// The cheap model that answers the capsule.
    pub model: String,
    /// Child turn ceiling; delegation beyond this is a design smell, not
    /// a budget. Defaults to 8 via [`ScoutConfig::defaults`].
    pub max_turns: u32,
    /// Wall-clock budget for one capsule. Defaults to 120 s.
    pub timeout_secs: u64,
}

impl ScoutConfig {
    /// Fills the knob defaults the roadmap set: ~8 child turns, 120 s.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            max_turns: 8,
            timeout_secs: 120,
        }
    }
}

/// The `scout` tool: one bounded read-only delegation (item 19).
///
/// Wraps a reusable [`Agent`] — one task per call, fresh session per
/// call — composed by the engine from a cheap provider. `is_read_only`
/// is `true`: a scout never mutates the workspace, so the parent's
/// completion gate and mutation ledger stay untouched by delegation.
pub struct ScoutTool {
    agent: Arc<Agent>,
    timeout: Duration,
}

impl ScoutTool {
    /// Wraps a composed child agent with the wall-clock budget.
    pub fn new(agent: Arc<Agent>, timeout_secs: u64) -> Self {
        Self {
            agent,
            timeout: Duration::from_secs(timeout_secs),
        }
    }
}

impl Tool for ScoutTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "scout".to_owned(),
            description: "Delegate one read-only question to a cheap scout model that \
                          explores the workspace (read/list/search only) and returns a \
                          concise answer with file:line pointers. Use it when locating \
                          something would take you several read/search turns. Provide a \
                          single focused question; the answer is bounded to ~1 KB."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "One focused question about the workspace."
                    },
                    "focus": {
                        "type": "string",
                        "description": "Optional hint narrowing the search (a path, a symbol, a file name)."
                    }
                },
                "required": ["question"]
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        // Cloned out so the returned future owns everything it touches:
        // the tool engine calls `call` under a higher-ranked bound, and
        // coercing a future that still borrows `self`/`ctx` through
        // `Box::pin` here trips rustc's FnOnce generality check. Boxing
        // inside the `'static` `run_scout` keeps this call site a plain
        // move. The token still cancels the child cooperatively; the
        // Arc clone is one refcount.
        run_scout(
            self.agent.clone(),
            self.timeout,
            ctx.cancellation.clone(),
            input,
        )
    }
}

fn run_scout(
    agent: Arc<Agent>,
    timeout: Duration,
    cancellation: tokio_util::sync::CancellationToken,
    input: Value,
) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
    // The child run goes through `tokio::spawn`: the tool engine calls
    // `call` under a higher-ranked bound, and a future that nests
    // `Agent::run_task` (→ `ToolBatch::execute`'s wave closures) trips
    // rustc's FnOnce-generality check when boxed directly. Behind a
    // JoinHandle the nested future is Send-checked once, concretely,
    // at the spawn. The token still cancels the child cooperatively.
    Box::pin(async move {
        let handle = tokio::spawn(run_scout_task(agent, timeout, cancellation, input));
        match handle.await {
            Ok(output) => output,
            Err(join) => Ok(ToolOutput::new(format!(
                "scout failed before answering ({join}); proceed without it"
            ))),
        }
    })
}

/// One bounded delegation: capsule → child run under wall-clock and
/// parent-cancellation races → the ≤1 KB answer.
async fn run_scout_task(
    agent: Arc<Agent>,
    timeout: Duration,
    cancellation: tokio_util::sync::CancellationToken,
    input: Value,
) -> Result<ToolOutput, ToolError> {
    let question = input["question"].as_str().unwrap_or_default().to_owned();
    if question.trim().is_empty() {
        return Ok(ToolOutput::new(
            "scout error: `question` must be a non-empty string".to_owned(),
        ));
    }
    let focus = input["focus"].as_str().map(str::to_owned);
    let prompt = capsule(&question, focus.as_deref());

    let mut session = Session::new(SessionId::new());
    let control = TaskControl::new();
    let run = agent.run_task(&mut session, &prompt, &QuietSink, &control);
    // The parent's token cancels the child through the same
    // cooperative seam every tool shares.
    let parent_cancelled = cancellation.cancelled();

    tokio::select! {
        biased;
        _ = tokio::time::sleep(timeout) => Ok(ToolOutput::new(format!(
            "scout timed out after {}s without an answer; proceed without it",
            timeout.as_secs()
        ))),
        _ = parent_cancelled => Ok(ToolOutput::new(
            "scout cancelled with the parent task".to_owned(),
        )),
        result = run => match result {
            Ok(_) => Ok(ToolOutput::new(final_answer(&session))),
            Err(error) => Ok(ToolOutput::new(format!("scout failed: {error}"))),
        },
    }
}

/// The capsule task the child receives: goal, scope, constraints, and
/// the output contract — everything a bounded delegation needs.
pub fn capsule(question: &str, focus: Option<&str>) -> String {
    let focus = focus
        .map(|hint| format!("Focus hint from the caller: {hint}\n"))
        .unwrap_or_default();
    format!(
        "You are a read-only scout. Answer ONE question about this \
         workspace so a parent agent can proceed without reading the \
         files itself.\n\n\
         {focus}\
         Question: {question}\n\n\
         Constraints: you may read, list, and search files only — no \
         edits, no shell commands. Explore efficiently; you have a \
         small turn budget.\n\n\
         Output contract: a concise answer (at most ~150 words) with \
         file:line pointers for every claim. No preamble, no plan \
         narration — just the answer."
    )
}

/// The scout's answer: the last committed assistant text, truncated to
/// the byte budget on a char boundary with an explicit marker so a
/// clipped answer is always legible as clipped.
pub fn final_answer(session: &Session) -> String {
    let text = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Assistant(message) => Some(message.content.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    truncate_answer(text)
}

/// Byte-bounds a scout answer on a char boundary.
fn truncate_answer(text: &str) -> String {
    if text.len() <= ANSWER_BUDGET_BYTES {
        return text.to_owned();
    }
    let mut end = ANSWER_BUDGET_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[…scout answer truncated at 1 KB]", &text[..end])
}

/// A no-op sink: the child's event stream belongs to the delegation,
/// not the parent's frontend.
struct QuietSink;

impl EventSink for QuietSink {
    fn emit(&self, _task: TaskId, _kind: EventKind) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use loom_kernel::context::{AgentConfig, DefaultContextCompiler};
    use loom_kernel::model::{
        ModelCapabilities, ModelEvent, ModelProvider, ModelRequest, ModelResponse, StopReason,
        Usage,
    };
    use loom_kernel::transcript::AssistantMessage;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A provider that answers once with fixed text and records every
    /// request it saw into a shared log, so tests can assert the
    /// capsule reached the child.
    struct AnsweringProvider {
        answer: String,
        requests: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl ModelProvider for AnsweringProvider {
        fn name(&self) -> &str {
            "answering"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, request: &'a ModelRequest) -> loom_kernel::model::ModelStream<'a> {
            let seen = request
                .messages
                .iter()
                .filter_map(|m| match m {
                    loom_kernel::model::Message::User { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.requests.lock().unwrap().push(seen);
            let event = ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: self.answer.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                },
                usage: Usage::default(),
            });
            Box::pin(stream::iter(vec![Ok(event)]))
        }
    }

    /// A provider that never yields an event: forces the wall-clock and
    /// cancellation races to decide.
    struct HangingProvider;

    impl ModelProvider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, _request: &'a ModelRequest) -> loom_kernel::model::ModelStream<'a> {
            Box::pin(stream::pending())
        }
    }

    fn scout_tool(provider: impl ModelProvider + 'static, timeout_secs: u64) -> ScoutTool {
        let agent = Agent::new(
            Box::new(provider),
            Box::new(DefaultContextCompiler::default()),
            loom_kernel::tool::ToolRegistry::new(),
            AgentConfig::new(std::path::PathBuf::from("."), "scout test"),
            "cheap-model",
        )
        .with_loop_shaping(false);
        ScoutTool::new(std::sync::Arc::new(agent), timeout_secs)
    }

    fn tool_ctx(cancellation: &tokio_util::sync::CancellationToken) -> ToolContext {
        ToolContext::new(
            std::path::PathBuf::from("."),
            cancellation.clone(),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn capsule_carries_question_and_focus() {
        let prompt = capsule("where is parse_opts?", Some("crates/loom-kernel"));
        assert!(prompt.contains("where is parse_opts?"));
        assert!(prompt.contains("crates/loom-kernel"));
    }

    #[test]
    fn truncate_answer_bounds_long_answers() {
        let short = "src/lib.rs:12";
        assert_eq!(truncate_answer(short), short);
        let long = "x".repeat(5_000);
        let bounded = truncate_answer(&long);
        assert!(bounded.contains("truncated at 1 KB"));
        assert!(bounded.len() <= ANSWER_BUDGET_BYTES + 40);
        assert!(bounded.starts_with('x'));
    }

    #[tokio::test]
    async fn scout_returns_the_child_answer_and_the_capsule_reaches_it() {
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let provider = AnsweringProvider {
            answer: "parse_opts lives in src/lib.rs:12".to_owned(),
            requests: requests.clone(),
        };
        let tool = scout_tool(provider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "where is parse_opts?"}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("src/lib.rs:12"));
        let log = requests.lock().unwrap();
        assert!(
            log.iter().any(|r| r.contains("where is parse_opts?")),
            "the capsule must reach the child model"
        );
    }

    #[tokio::test]
    async fn scout_times_out_and_says_so() {
        let tool = scout_tool(HangingProvider, 1);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "anything"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("timed out"), "{}", output);
    }

    #[tokio::test]
    async fn scout_stops_when_the_parent_cancels() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        token.cancel();
        let output = tool
            .call(serde_json::json!({"question": "anything"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("cancel"), "{}", output);
    }

    #[tokio::test]
    async fn scout_rejects_an_empty_question() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "  "}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("non-empty"), "{}", output);
    }
}
