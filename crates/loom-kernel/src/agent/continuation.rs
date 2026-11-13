//! Long-run continuation paths (§5.1): the context handoff — a
//! one-shot compaction summary when the projection overflows (roadmap
//! item 12) — and the bounded `max_tokens` continuation reminders.

use super::Agent;
use super::TaskError;
use super::summary::Accounting;
use crate::context::PruneCheckpoint;
use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::model::{Message, ModelRequest, StopReason};
use crate::session::Session;
use crate::transcript::{Item, ToolStatus};
use tokio_util::sync::CancellationToken;

impl Agent {
    /// The LLM handoff (roadmap item 12's last resort): builds a
    /// deterministic digest of the task and recent history, spends one
    /// model call asking for a goal-directed summary, and freezes it
    /// into a pruning checkpoint whose projection is brief + handoff +
    /// tail. The call bills like any other (usage accounted, events
    /// emitted, retries honored). `None` when the reply is unusable —
    /// wrong stop reason, tool calls, or empty text — in which case the
    /// caller surfaces the original budget overrun.
    pub(super) async fn attempt_handoff(
        &self,
        session: &Session,
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
        turn: u32,
    ) -> Result<Option<PruneCheckpoint>, TaskError> {
        let digest = handoff_digest(session);
        let request = ModelRequest {
            model: self.model.clone(),
            messages: vec![
                Message::System {
                    text: HANDOFF_PROMPT.to_owned(),
                },
                Message::User { text: digest },
            ],
            // No tools: the summary must be plain text, and an
            // EndTurn-with-calls reply is rejected below anyway.
            tools: Vec::new(),
            max_tokens: Some(HANDOFF_MAX_OUTPUT_TOKENS),
            temperature: None,
            stable_prefix_messages: None,
            // The handoff is a one-shot summary call — no history to
            // clear, so no context-editing hint rides it, and no
            // schedule applies (compression, not reasoning work).
            context_editing: None,
            reasoning_effort: None,
            session_id: Some(session.id.to_string()),
        };
        let response = self
            .stream_with_retry(&request, task_id, sink, cancellation, accounting)
            .await?;
        accounting.usage = accounting.usage + response.usage;
        let summary = response.message;
        if summary.stop_reason != StopReason::EndTurn
            || !summary.tool_calls.is_empty()
            || summary.content.trim().is_empty()
        {
            return Ok(None);
        }
        let chars = summary.content.chars().count();
        sink.emit(task_id, EventKind::ContextHandoff { turn, chars });
        Ok(Some(PruneCheckpoint {
            upto_item: session.transcript.len(),
            stubs: Vec::new(),
            elided_args: Vec::new(),
            elided_users: Vec::new(),
            elided_reasoning: Vec::new(),
            elided_assistant_text: Vec::new(),
            handoff: Some(summary.content),
        }))
    }
}

/// Ceiling on continue-reminders per task after `max_tokens` truncation:
/// enough for a reasoning model's long final answer, but bounded so a
/// model that never terminates still surfaces the failure.
pub(super) const MAX_TOKENS_CONTINUATIONS: u8 = 4;

/// Injected user item when generation hits the output limit: the field's
/// standard resume protocol — the model already sees its committed
/// partial reply as the previous assistant turn.
pub(super) const MAX_TOKENS_CONTINUATION_PROMPT: &str = "Your reply hit the output token limit and was cut off. Continue \
     exactly where you left off — do not repeat any earlier content.";

/// Whether a result is a green command run: successful, command-shaped
/// (`$ …`), and reporting exit status 0.
/// System prompt for the one-shot LLM handoff (roadmap item 12's last
/// resort). Goal-directed by design: the summary names the objective,
/// decisions, open problems, and relevant files, not a narrative.
pub(super) const HANDOFF_PROMPT: &str = "You are compacting an agent session that outgrew its context \
window. Write a handoff summary the same agent can continue from as if \
no history was lost. Include, in this order: (1) the task objective as \
currently understood, including any re-scoping; (2) decisions already \
made and why; (3) open problems and failed approaches, with the exact \
errors; (4) files and paths that matter, with their current state; (5) \
the single next step. Be terse and concrete — paths, commands, \
identifiers, not prose. Plain text only.";

/// Output ceiling for the handoff summary request.
pub(super) const HANDOFF_MAX_OUTPUT_TOKENS: u32 = 1024;

/// The deterministic digest the handoff request carries: the live task
/// brief, the session's opening brief when it differs, the trailing
/// assistant texts, the trailing failed tool results, and every path
/// the session touched. Fixed caps keep the digest small even when the
/// transcript is enormous — that is the point of the handoff.
pub(super) fn handoff_digest(session: &Session) -> String {
    let cap = |text: &str, max: usize| -> String {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= max {
            text.to_owned()
        } else {
            chars[..max].iter().collect()
        }
    };
    let mut digest = String::new();

    // The live task brief: the last user item that is real input
    // (run_task appends the task text; elision never takes the last
    // user item, so it is intact). Kernel-appended todo blocks
    // (roadmap item 8) are projection artifacts, not instructions —
    // skip them so the digest's brief is never the plan list itself.
    let last_user = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::User { text } if !text.starts_with(crate::session::TODO_BLOCK_HEADER) => {
                Some(text.clone())
            }
            Item::User { .. } | Item::Assistant(_) | Item::ToolResult(_) => None,
        })
        .unwrap_or_default();
    let brief = session.transcript.first().and_then(|item| match item {
        Item::User { text } => Some(text.clone()),
        Item::Assistant(_) | Item::ToolResult(_) => None,
    });
    digest.push_str("## Task\n");
    digest.push_str(&cap(&last_user, 2_000));
    digest.push('\n');
    if let Some(brief) = brief
        && brief != last_user
    {
        digest.push_str("\n## Session brief\n");
        digest.push_str(&cap(&brief, 1_000));
        digest.push('\n');
    }

    // Trailing assistant texts — the freshest reasoning survives.
    let texts: Vec<&str> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(assistant)
                if assistant.stop_reason != StopReason::ContentFilter
                    && !assistant.content.trim().is_empty() =>
            {
                Some(assistant.content.as_str())
            }
            Item::User { .. } | Item::Assistant(_) | Item::ToolResult(_) => None,
        })
        .collect();
    if !texts.is_empty() {
        digest.push_str("\n## Recent assistant messages (oldest first)\n");
        for text in texts.iter().rev().take(4).rev() {
            digest.push_str("- ");
            digest.push_str(&cap(text, 500));
            digest.push('\n');
        }
    }

    // Trailing failed tool results — the exact errors to not repeat.
    let failures: Vec<&str> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.status != ToolStatus::Success => {
                Some(result.content.as_str())
            }
            Item::User { .. } | Item::Assistant(_) | Item::ToolResult(_) => None,
        })
        .collect();
    if !failures.is_empty() {
        digest.push_str("\n## Recent failed tool results (oldest first)\n");
        for content in failures.iter().rev().take(3).rev() {
            digest.push_str("- ");
            digest.push_str(&cap(content, 300));
            digest.push('\n');
        }
    }

    // Every path the session touched — deduplicated, first-seen order.
    let mut touched: Vec<&str> = Vec::new();
    for item in &session.transcript {
        if let Item::ToolResult(result) = item {
            for path in result
                .effects
                .files_written
                .iter()
                .chain(&result.effects.files_deleted)
            {
                if !touched.contains(&path.as_str()) {
                    touched.push(path.as_str());
                }
            }
        }
    }
    if !touched.is_empty() {
        digest.push_str("\n## Files touched\n");
        for path in touched.iter().take(40) {
            digest.push_str("- ");
            digest.push_str(path);
            digest.push('\n');
        }
    }

    digest
}
