//! The delegation runtime: one capsule → a child run raced against the
//! wall-clock budget and the parent's cancellation token, with cascade
//! escalation, cross-check, and fan-out layered on top
//! (docs/subagents-design.md).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use loom_kernel::agent::{Agent, TaskControl};
use loom_kernel::event::{EventKind, EventSink};
use loom_kernel::id::{SessionId, TaskId};
use loom_kernel::session::Session;
use loom_kernel::tool::{ToolError, ToolOutput};
use loom_kernel::transcript::Item;
use serde_json::Value;

use super::capsule::{capsule, final_answer};
use super::config::SCOUT_CONCURRENCY;
use super::gate::{agreement_note, weak_reason};

/// How one child run ended. Drives the deterministic weak-answer gate.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Status {
    Answered,
    TimedOut,
    Failed(String),
    Cancelled,
}

/// One completed child run: the (already budget-truncated) answer and
/// how it ended.
pub(super) struct ScoutOutcome {
    pub(super) answer: String,
    pub(super) status: Status,
    /// Assistant turns in the child transcript — deterministic spend
    /// provenance for the footer.
    pub(super) turns: usize,
}

/// One bounded delegation: capsule → child run under wall-clock and
/// parent-cancellation races → the ≤1 KB answer, with cascade
/// escalation, cross-check, and fan-out layered on top
/// (docs/subagents-design.md).
pub(super) async fn run_scout_task(
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
            let _ = writeln!(out, "[scout {}/{}]", index + 1, total);
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
        () = tokio::time::sleep(timeout) => ScoutOutcome {
            answer: String::new(),
            status: Status::TimedOut,
            turns: turn_count(&session),
        },
        () = parent_cancelled => ScoutOutcome {
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

/// A no-op sink: the child's event stream belongs to the delegation,
/// not the parent's frontend.
struct QuietSink;

impl EventSink for QuietSink {
    fn emit(&self, _task: TaskId, _kind: EventKind) {}
}
