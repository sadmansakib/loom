//! The `scout` tool surface: its spec, builder knobs, and the
//! fail-soft capacity gate around one spawned child run.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use loom_kernel::agent::Agent;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use super::config::SCOUT_CONCURRENCY;
use super::run::run_scout_task;

/// The `scout` tool: one bounded read-only delegation (item 19).
///
/// Wraps a reusable [`Agent`] — one task per call, fresh session per
/// call — composed by the engine from a cheap provider, plus an
/// optional stronger [`Agent`] for cascade escalation and cross-checks
/// (docs/subagents-design.md). `is_read_only` is `true`: a scout never
/// mutates the workspace, so the parent's completion gate and mutation
/// ledger stay untouched by delegation.
pub struct ScoutTool {
    agent: Arc<Agent>,
    /// The strong model pair for escalation / cross-check runs.
    escalation: Option<(Arc<Agent>, String)>,
    /// Footer provenance label for the primary model.
    model_label: String,
    timeout: Duration,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
}

impl ScoutTool {
    /// Wraps a composed child agent with the wall-clock budget and the
    /// default concurrency cap ([`SCOUT_CONCURRENCY`] parallel scouts).
    pub fn new(agent: Arc<Agent>, timeout_secs: u64) -> Self {
        Self {
            agent,
            escalation: None,
            model_label: "scout".to_owned(),
            timeout: Duration::from_secs(timeout_secs),
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(SCOUT_CONCURRENCY)),
        }
    }

    /// Overrides the parallel-scout cap (tests; future config wiring).
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.permits = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        self
    }

    /// Sets the provenance footer label for the primary model.
    pub fn with_model_label(mut self, label: impl Into<String>) -> Self {
        self.model_label = label.into();
        self
    }

    /// Attaches the stronger escalation/cross-check agent with its
    /// provenance label.
    pub fn with_escalation(mut self, agent: Arc<Agent>, model_label: impl Into<String>) -> Self {
        self.escalation = Some((agent, model_label.into()));
        self
    }
}

impl Tool for ScoutTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "scout".to_owned(),
            description: "Delegate read-only questions to a cheap scout model that \
                          explores the workspace (read/list/search only) and returns \
                          concise answers with file:line pointers. Use it when locating \
                          something would take you several read/search turns. One \
                          focused `question` per call, or up to 3 `questions` answered \
                          in parallel (divide-and-conquer). Answers are bounded to \
                          ~1 KB each and carry a model/turns provenance footer. A \
                          weak answer (timeout, empty, no file:line pointers) is \
                          auto-escalated once to the stronger scout model when one \
                          is configured; `cross_check: true` asks both models the \
                          same question and reports their agreement."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "One focused question about the workspace."
                    },
                    "questions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 3,
                        "description": "Divide-and-conquer: up to 3 focused questions answered in parallel (alternative to `question`)."
                    },
                    "focus": {
                        "type": "string",
                        "description": "Optional hint narrowing the search (a path, a symbol, a file name)."
                    },
                    "cross_check": {
                        "type": "boolean",
                        "description": "Ask both the cheap and the strong scout model the same question and report where their file:line citations agree or differ. Ignored when no escalation model is configured."
                    }
                },
                "required": []
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
            self.escalation
                .as_ref()
                .map(|(agent, label)| (agent.clone(), label.clone())),
            self.model_label.clone(),
            self.timeout,
            ctx.cancellation.clone(),
            input,
            self.permits.clone(),
        )
    }
}

fn run_scout(
    agent: Arc<Agent>,
    escalation: Option<(Arc<Agent>, String)>,
    model_label: String,
    timeout: Duration,
    cancellation: tokio_util::sync::CancellationToken,
    input: Value,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
    // The child run goes through `tokio::spawn`: the tool engine calls
    // `call` under a higher-ranked bound, and a future that nests
    // `Agent::run_task` (→ `ToolBatch::execute`'s wave closures) trips
    // rustc's FnOnce-generality check when boxed directly. Behind a
    // JoinHandle the nested future is Send-checked once, concretely,
    // at the spawn. The token still cancels the child cooperatively.
    Box::pin(async move {
        // Fail-soft concurrency gate: never queue. A parent fanning out
        // scouts gets an immediate "at capacity" it can act on instead
        // of a pile of child spends waiting on permits.
        let permit = match permits.try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return Ok(ToolOutput::new(
                    "scout is at capacity (all parallel scout permits are \
                     held); retry after the running scouts answer, or \
                     proceed without it"
                        .to_owned(),
                ));
            }
        };
        let handle = tokio::spawn(run_scout_task(
            agent,
            escalation,
            model_label,
            timeout,
            cancellation,
            input,
            permit,
        ));
        match handle.await {
            Ok(output) => output,
            Err(join) => Ok(ToolOutput::new(format!(
                "scout failed before answering ({join}); proceed without it"
            ))),
        }
    })
}
