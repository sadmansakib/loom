//! The read-only scout (roadmap item 19, iteration 3).
//!
//! One delegation level, bounded hard: a cheap model answers a capsule
//! question over the workspace with read-only tools and returns a
//! ≤1 KB finding. The parent's context pays only the capsule and the
//! answer — the exploration the scout performed never enters it.
//!
//! Multi-model collaboration is layered on that spine
//! (docs/subagents-design.md): a deterministic weak-answer gate
//! escalates once to a stronger model (FrugalGPT-shaped cascade, code
//! heuristics instead of a learned scorer); `cross_check` runs both
//! models and reports citation agreement; `questions` fans out up to
//! three capsules in parallel. Every answer carries a model/turns
//! provenance footer — delegation spend stays visible. Depth stays at
//! one level: nested subagents compound lossy compression (Cognition
//! 2025; Anthropic's research system keeps one level too).
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

/// Parallel scouts one parent may run at once. Over the cap the call
/// fails soft ("at capacity") rather than queueing — a parent that
/// fires a fan of scouts should not silently stack a backlog of child
/// spends behind a semaphore.
pub const SCOUT_CONCURRENCY: usize = 3;

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
    /// Optional stronger model for cascade escalation and cross-checks
    /// (docs/subagents-design.md §3). `None` keeps the single-model
    /// scout.
    pub escalation: Option<ScoutEscalation>,
}

/// The escalation target: a (provider, model) pair for the strong
/// second opinion. One escalation per capsule, hard-bounded — no
/// chains, no judge models.
#[derive(Debug, Clone)]
pub struct ScoutEscalation {
    /// Provider id for the strong model (resolved like the scout's own).
    pub provider: String,
    /// Strong model id; also the provenance label in escalated output.
    pub model: String,
}

impl ScoutConfig {
    /// Fills the knob defaults the roadmap set: ~8 child turns, 120 s.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            max_turns: 8,
            timeout_secs: 120,
            escalation: None,
        }
    }

    /// Sets the cascade/cross-check escalation target (builder).
    pub fn with_escalation(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.escalation = Some(ScoutEscalation {
            provider: provider.into(),
            model: model.into(),
        });
        self
    }
}

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

/// How one child run ended. Drives the deterministic weak-answer gate.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Answered,
    TimedOut,
    Failed(String),
    Cancelled,
}

/// One completed child run: the (already budget-truncated) answer and
/// how it ended.
struct ScoutOutcome {
    answer: String,
    status: Status,
    /// Assistant turns in the child transcript — deterministic spend
    /// provenance for the footer.
    turns: usize,
}

/// One bounded delegation: capsule → child run under wall-clock and
/// parent-cancellation races → the ≤1 KB answer, with cascade
/// escalation, cross-check, and fan-out layered on top
/// (docs/subagents-design.md).
async fn run_scout_task(
    agent: Arc<Agent>,
    escalation: Option<(Arc<Agent>, String)>,
    model_label: String,
    timeout: Duration,
    cancellation: tokio_util::sync::CancellationToken,
    input: Value,
    // Held for the child's whole run; released on drop (timeout,
    // cancellation, or answer).
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<ToolOutput, ToolError> {
    // Input contract: `questions` (fan-out) wins when present and
    // non-empty; otherwise `question`. Everything else is a soft error.
    let fan: Option<Vec<String>> = input["questions"].as_array().map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    });
    let questions: Vec<String> = if let Some(fan) = fan {
        if fan.len() > SCOUT_CONCURRENCY {
            return Ok(ToolOutput::new(format!(
                "scout error: `questions` accepts at most {SCOUT_CONCURRENCY} items \
                 per call (divide-and-conquer, not a queue); got {}",
                fan.len()
            )));
        }
        if fan.is_empty() || fan.iter().any(|q| q.trim().is_empty()) {
            return Ok(ToolOutput::new(
                "scout error: every entry in `questions` must be a non-empty string".to_owned(),
            ));
        }
        fan
    } else if let Some(question) = input["question"].as_str() {
        if question.trim().is_empty() {
            return Ok(ToolOutput::new(
                "scout error: `question` must be a non-empty string".to_owned(),
            ));
        }
        vec![question.to_owned()]
    } else {
        return Ok(ToolOutput::new(
            "scout error: provide `question` (one focused question) or `questions` \
             (up to 3, answered in parallel)"
                .to_owned(),
        ));
    };
    let focus = input["focus"].as_str().map(str::to_owned);
    let cross_check = input["cross_check"].as_bool().unwrap_or(false);

    // Fan-out: every question's primary run starts concurrently; each
    // may then escalate or cross-check sequentially (bounded: one
    // strong-model run per question).
    let env = ScoutEnv {
        escalation: escalation.as_ref(),
        model_label: &model_label,
        focus: focus.as_deref(),
        cross_check,
        timeout,
        cancellation: &cancellation,
    };
    let mut jobs = Vec::with_capacity(questions.len());
    for question in questions {
        jobs.push(answer_one(&agent, &env, question));
    }
    let sections = futures::future::join_all(jobs).await;
    let total = sections.len();
    let mut out = String::new();
    for (index, section) in sections.into_iter().enumerate() {
        if total > 1 {
            out.push_str(&format!("[scout {}/{}]\n", index + 1, total));
        }
        out.push_str(&section);
        if index + 1 < total {
            out.push_str("\n\n");
        }
    }
    Ok(ToolOutput::new(out))
}

/// Everything every parallel capsule needs besides its question —
/// bundled so `answer_one` stays under the argument budget.
struct ScoutEnv<'a> {
    escalation: Option<&'a (Arc<Agent>, String)>,
    model_label: &'a str,
    focus: Option<&'a str>,
    cross_check: bool,
    timeout: Duration,
    cancellation: &'a tokio_util::sync::CancellationToken,
}

/// Answers one question with the cheap model, then applies the
/// deterministic weak-answer gate: a weak primary (timeout, failure,
/// empty, pointerless, or budget-truncated answer) escalates once to
/// the strong model; `cross_check` runs both models unconditionally
/// and reports their citation agreement. The parent model remains the
/// synthesizer — no judge model in the runtime.
async fn answer_one(agent: &Arc<Agent>, env: &ScoutEnv<'_>, question: String) -> String {
    let prompt = capsule(&question, env.focus);
    let primary = ask(agent, &prompt, env.timeout, env.cancellation).await;
    if primary.status == Status::Cancelled {
        return "scout cancelled with the parent task".to_owned();
    }

    if let Some((strong_agent, strong_label)) = env.escalation {
        if let Some(reason) = weak_reason(&primary) {
            let strong = ask(strong_agent, &prompt, env.timeout, env.cancellation).await;
            if strong.status == Status::Cancelled {
                return "scout cancelled with the parent task".to_owned();
            }
            return format!(
                "{}\n\n[escalated to {strong_label}: primary answer was {reason}]\n\n{}",
                render(&primary, env.model_label),
                render(&strong, strong_label)
            );
        }
        if env.cross_check {
            let strong = ask(strong_agent, &prompt, env.timeout, env.cancellation).await;
            if strong.status == Status::Cancelled {
                return "scout cancelled with the parent task".to_owned();
            }
            return format!(
                "[cross-check: {} vs {strong_label}]\n{}\n\n{}\n\n{}",
                env.model_label,
                render(&primary, env.model_label),
                render(&strong, strong_label),
                agreement_note(&primary.answer, &strong.answer)
            );
        }
    } else if env.cross_check {
        // Deterministic honesty: the flag is a no-op without a strong
        // model; say so instead of silently degrading.
        return format!(
            "{}\n\n[cross-check requested but no escalation model is configured]",
            render(&primary, env.model_label)
        );
    }
    render(&primary, env.model_label)
}

/// One child run raced against the wall-clock budget and the parent's
/// cancellation token. Every outcome stays fail-soft.
async fn ask(
    agent: &Arc<Agent>,
    prompt: &str,
    timeout: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> ScoutOutcome {
    let mut session = Session::new(SessionId::new());
    let control = TaskControl::new();
    let run = agent.run_task(&mut session, prompt, &QuietSink, &control);
    // The parent's token cancels the child through the same
    // cooperative seam every tool shares.
    let parent_cancelled = cancellation.cancelled();

    tokio::select! {
        biased;
        _ = tokio::time::sleep(timeout) => ScoutOutcome {
            answer: String::new(),
            status: Status::TimedOut,
            turns: turn_count(&session),
        },
        _ = parent_cancelled => ScoutOutcome {
            answer: String::new(),
            status: Status::Cancelled,
            turns: turn_count(&session),
        },
        result = run => match result {
            Ok(_) => ScoutOutcome {
                answer: final_answer(&session),
                status: Status::Answered,
                turns: turn_count(&session),
            },
            Err(error) => ScoutOutcome {
                answer: String::new(),
                status: Status::Failed(error.to_string()),
                turns: turn_count(&session),
            },
        },
    }
}

/// The deterministic weak-answer gate (FrugalGPT's quality check with
/// code heuristics instead of a learned scorer — conservative recall
/// by design, per docs/subagents-design.md §3): timeouts, failures,
/// empty answers, answers that hit the truncation budget, and answers
/// that violate the capsule's file:line contract all escalate once.
fn weak_reason(outcome: &ScoutOutcome) -> Option<&'static str> {
    match &outcome.status {
        Status::TimedOut => Some("timed out"),
        Status::Failed(_) => Some("a failed run"),
        Status::Cancelled => None, // parent intent; a re-run would cancel too
        Status::Answered => {
            if outcome.answer.trim().is_empty() {
                Some("an empty answer")
            } else if outcome.answer.contains("[…scout answer truncated at 1 KB]") {
                Some("truncated at the answer budget")
            } else if !has_file_line_pointer(&outcome.answer) {
                Some("missing the file:line pointers the capsule contract requires")
            } else {
                None
            }
        }
    }
}

/// One whitespace token, stripped of common surrounding punctuation
/// (including trailing periods), judged as a `path:line` citation.
fn pointer_token(token: &str) -> Option<&str> {
    let mut token = token.trim_matches(|c: char| {
        matches!(
            c,
            ',' | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '!'
                | '?'
                | '"'
                | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
        )
    });
    // Trailing periods are sentence punctuation; interior dots stay
    // (file extensions).
    while token.ends_with('.') {
        token = &token[..token.len() - 1];
    }
    match token.rsplit_once(':') {
        Some((path, line)) => {
            // The path half must look like a name, not a bare number —
            // keeps "12:30" (a time) and "3:16" (a verse) out.
            (!path.is_empty()
                && !path.chars().all(|c| c.is_ascii_digit())
                && !line.is_empty()
                && line.chars().all(|c| c.is_ascii_digit()))
            .then_some(token)
        }
        None => None,
    }
}

/// True when the text cites at least one `file:line`-shaped pointer —
/// a colon followed by ASCII digits inside a token. Deliberately
/// cheap: the capsule demands pointers for every claim, so zero
/// matches is a contract violation, not a style note.
fn has_file_line_pointer(text: &str) -> bool {
    text.split_whitespace()
        .any(|token| pointer_token(token).is_some())
}

/// All `file:line` pointers an answer cites (deterministic extraction
/// for the cross-check agreement note).
fn cited_pointers(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(pointer_token)
        .map(str::to_owned)
        .collect()
}

/// The deterministic cross-check note: which citations both models
/// make, and which only one does. The parent synthesizes; this is the
/// evidence, not a verdict.
fn agreement_note(primary: &str, strong: &str) -> String {
    use std::collections::HashSet;
    let cap = 6;
    let a: HashSet<String> = cited_pointers(primary).into_iter().collect();
    let b: HashSet<String> = cited_pointers(strong).into_iter().collect();
    let both: Vec<&str> = a.intersection(&b).take(cap).map(String::as_str).collect();
    let only_a: Vec<&str> = a.difference(&b).take(cap).map(String::as_str).collect();
    let only_b: Vec<&str> = b.difference(&a).take(cap).map(String::as_str).collect();
    if a.is_empty() && b.is_empty() {
        return "[agreement note: neither answer cites file:line pointers]".to_owned();
    }
    let mut note = String::from("[agreement note: ");
    if both.is_empty() {
        note.push_str("no shared citations");
    } else {
        note.push_str(&format!("both cite {}", both.join(", ")));
    }
    if !only_a.is_empty() {
        note.push_str(&format!(
            "; only the primary model cites {}",
            only_a.join(", ")
        ));
    }
    if !only_b.is_empty() {
        note.push_str(&format!(
            "; only the strong model cites {}",
            only_b.join(", ")
        ));
    }
    format!("{note}]")
}

/// Formats one outcome with its provenance footer. Failed and timed-out
/// runs state so plainly — an absent answer must never read as "no
/// result found".
fn render(outcome: &ScoutOutcome, model_label: &str) -> String {
    match &outcome.status {
        Status::Answered => format!(
            "{}\n(model: {model_label}, {} turn{})",
            outcome.answer,
            outcome.turns,
            if outcome.turns == 1 { "" } else { "s" }
        ),
        Status::TimedOut => format!(
            "no answer: the run timed out (model: {model_label}, {} turn{})",
            outcome.turns,
            if outcome.turns == 1 { "" } else { "s" }
        ),
        Status::Failed(error) => {
            format!("no answer: the run failed — {error} (model: {model_label})")
        }
        Status::Cancelled => "scout cancelled with the parent task".to_owned(),
    }
}

/// Assistant turns committed in a child transcript — the deterministic
/// spend signal for the footer (feeds the parked delegation-economics
/// trigger, roadmap item 19).
fn turn_count(session: &Session) -> usize {
    session
        .transcript
        .iter()
        .filter(|item| matches!(item, Item::Assistant(_)))
        .count()
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

    /// A scout with a strong escalation agent wired (`with_escalation`).
    fn escalation_tool(
        primary: impl ModelProvider + 'static,
        strong: impl ModelProvider + 'static,
        timeout_secs: u64,
    ) -> ScoutTool {
        let make = |provider: Box<dyn ModelProvider>| {
            Agent::new(
                provider,
                Box::new(DefaultContextCompiler::default()),
                loom_kernel::tool::ToolRegistry::new(),
                AgentConfig::new(std::path::PathBuf::from("."), "scout test"),
                "model",
            )
            .with_loop_shaping(false)
        };
        ScoutTool::new(std::sync::Arc::new(make(Box::new(primary))), timeout_secs)
            .with_model_label("cheap-model")
            .with_escalation(std::sync::Arc::new(make(Box::new(strong))), "strong-model")
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

    #[tokio::test]
    async fn scout_fails_soft_beyond_the_concurrency_cap() {
        // One permit: the in-flight scout holds it until its 2 s timeout.
        let tool = std::sync::Arc::new(scout_tool(HangingProvider, 2).with_concurrency(1));
        let first_tool = tool.clone();
        let first = tokio::spawn(async move {
            let token = tokio_util::sync::CancellationToken::new();
            let ctx = tool_ctx(&token);
            first_tool
                .call(serde_json::json!({"question": "slow"}), &ctx)
                .await
                .expect("scout call")
                .content
        });
        // Let the first scout acquire its permit before the second asks.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let second = tool
            .call(serde_json::json!({"question": "blocked"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(second.contains("at capacity"), "{}", second);
        let first = first.await.expect("join");
        assert!(first.contains("timed out"), "{}", first);
    }

    #[test]
    fn pointer_detection_and_extraction() {
        assert!(has_file_line_pointer("see src/lib.rs:12 for it"));
        assert!(has_file_line_pointer("engine.rs:40 says so."));
        assert!(!has_file_line_pointer("no pointers here"));
        assert!(!has_file_line_pointer("time 12:30 was odd")); // colon not path-shaped
        let pointers = cited_pointers("a.rs:1 and b/c.rs:22, plus d.rs:333.");
        assert_eq!(pointers, vec!["a.rs:1", "b/c.rs:22", "d.rs:333"]);
    }

    #[test]
    fn agreement_note_reports_shared_and_divergent_citations() {
        let note = agreement_note(
            "core in src/lib.rs:12 and extra.rs:9",
            "core in src/lib.rs:12, other.rs:4",
        );
        assert!(note.contains("both cite src/lib.rs:12"), "{note}");
        assert!(
            note.contains("only the primary model cites extra.rs:9"),
            "{note}"
        );
        assert!(
            note.contains("only the strong model cites other.rs:4"),
            "{note}"
        );
    }

    #[test]
    fn weak_reason_flags_the_contract_violations() {
        let answered = |answer: &str| ScoutOutcome {
            answer: answer.to_owned(),
            status: Status::Answered,
            turns: 1,
        };
        assert!(weak_reason(&answered("it is in src/lib.rs:12")).is_none());
        assert_eq!(
            weak_reason(&answered("no idea, sorry")),
            Some("missing the file:line pointers the capsule contract requires")
        );
        assert_eq!(weak_reason(&answered("")), Some("an empty answer"));
        let truncated = ScoutOutcome {
            answer: format!("{}\n[…scout answer truncated at 1 KB]", "x".repeat(1_100)),
            status: Status::Answered,
            turns: 1,
        };
        assert_eq!(
            weak_reason(&truncated),
            Some("truncated at the answer budget")
        );
        let timed_out = ScoutOutcome {
            answer: String::new(),
            status: Status::TimedOut,
            turns: 0,
        };
        assert_eq!(weak_reason(&timed_out), Some("timed out"));
    }

    #[tokio::test]
    async fn weak_answers_escalate_to_the_strong_model() {
        let primary_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let strong_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "i could not find it".to_owned(), // no pointers → weak
                requests: primary_requests.clone(),
            },
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: strong_requests.clone(),
            },
            60,
        );
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
        assert!(output.contains("escalated to strong-model"), "{}", output);
        assert!(
            output.contains("missing the file:line pointers"),
            "{}",
            output
        );
        assert!(output.contains("src/lib.rs:12"), "{}", output);
        // Both models saw the same capsule.
        assert!(
            strong_requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("where is parse_opts?"))
        );
    }

    #[tokio::test]
    async fn strong_answers_do_not_escalate() {
        let strong_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            AnsweringProvider {
                answer: "should never run".to_owned(),
                requests: strong_requests.clone(),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "where?"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(!output.contains("escalated"), "{}", output);
        assert!(strong_requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cross_check_runs_both_models_and_reports_agreement() {
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "entry at src/main.rs:10".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            AnsweringProvider {
                answer: "entry at src/main.rs:10, config in src/config.rs:5".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "entry point?", "cross_check": true}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("cross-check: cheap-model vs strong-model"),
            "{}",
            output
        );
        assert!(output.contains("both cite src/main.rs:10"), "{}", output);
        assert!(
            output.contains("only the strong model cites src/config.rs:5"),
            "{}",
            output
        );
    }

    #[tokio::test]
    async fn cross_check_without_an_escalation_model_says_so() {
        let tool = scout_tool(
            AnsweringProvider {
                answer: "entry at src/main.rs:10".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "entry?", "cross_check": true}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("no escalation model is configured"),
            "{}",
            output
        );
    }

    #[tokio::test]
    async fn questions_fan_out_in_parallel_with_labels() {
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = scout_tool(
            AnsweringProvider {
                answer: "found it at src/lib.rs:12".to_owned(),
                requests: requests.clone(),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"questions": ["where is parse_opts?", "where is main?"]}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("[scout 1/2]"), "{}", output);
        assert!(output.contains("[scout 2/2]"), "{}", output);
        assert_eq!(output.matches("found it at src/lib.rs:12").count(), 2);
        // Both capsules reached the child.
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("where is main?"))
        );
    }

    #[tokio::test]
    async fn questions_over_the_cap_fail_soft() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"questions": ["a?", "b?", "c?", "d?"]}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("at most 3"), "{}", output);
    }

    #[tokio::test]
    async fn missing_question_input_fails_soft() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"focus": "src"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("`question`"), "{}", output);
    }

    #[tokio::test]
    async fn answers_carry_a_provenance_footer() {
        let tool = scout_tool(
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        )
        .with_model_label("cheap-model");
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "where?"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("(model: cheap-model, 1 turn)"),
            "{}",
            output
        );
    }
}
