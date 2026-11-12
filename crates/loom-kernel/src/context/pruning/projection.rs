use std::collections::HashMap;

use crate::model::{Message, StopReason};
use crate::session::Session;
use crate::transcript::{Item, ToolArguments};

use super::elision::elide_text;
use super::strategies::DecisionSet;
use super::types::{ArgElision, StubDecision, StubReason};

/// Builds the handoff projection view: the session's opening brief, the
/// LLM-written handoff as a user item, then the transcript tail from
/// `upto_item` on — everything before the tail was summarized away. The
/// durable transcript is never mutated; only this clone's projection
/// changes, mirroring [`stubbed_session`].
pub(super) fn handoff_session(session: &Session, text: &str, upto_item: usize) -> Session {
    let upto = upto_item.min(session.transcript.len());
    let mut transcript = Vec::with_capacity(2 + session.transcript.len() - upto);
    // The brief is the session's first user item (ordinal 0, never
    // elided); when the transcript opens on something else the handoff
    // alone leads.
    if matches!(session.transcript.first(), Some(Item::User { .. })) {
        transcript.push(session.transcript[0].clone());
    }
    transcript.push(Item::User {
        text: text.to_owned(),
    });
    transcript.extend(session.transcript[upto..].iter().cloned());
    let mut view = session.clone();
    view.transcript = transcript;
    view
}

/// Rewrites projected `ToolResult` messages whose `call_id` is stubbed,
/// leaving every other message (and the call/result pairing) untouched.
///
/// Returns the call ids whose bodies were actually rewritten, so the
/// caller can report them as
/// [`CompiledContext::stubbed_calls`](crate::context::CompiledContext::stubbed_calls): a stub
/// body is not content the model can still be pointed at.
pub(super) fn apply_stubs(
    request: &mut crate::model::ModelRequest,
    stubs: &[StubDecision],
) -> Vec<String> {
    let by_call: HashMap<&str, StubReason> = stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let mut rewritten = Vec::new();
    for message in &mut request.messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        let Some(reason) = by_call.get(result.call_id.as_str()) else {
            continue;
        };
        result.content = reason.stub_text().to_owned();
        result.truncated = false;
        rewritten.push(result.call_id.as_str().to_owned());
    }
    rewritten
}

/// Clones `session` with the stubbed results rewritten in place, as the
/// private view the budget-exceeded rescue recompiles. The durable
/// transcript is never mutated — only this copy's projection changes,
/// mirroring [`apply_stubs`] and [`apply_elisions`] item for item.
pub(super) fn stubbed_session(session: &Session, decisions: &DecisionSet) -> Session {
    let by_call: HashMap<&str, StubReason> = decisions
        .stubbed
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let elided_calls: HashMap<&str, &ToolArguments> = decisions
        .elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let mut stubbed = session.clone();
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for item in &mut stubbed.transcript {
        match item {
            Item::ToolResult(result) => {
                let Some(reason) = by_call.get(result.call_id.as_str()) else {
                    continue;
                };
                result.content = reason.stub_text().to_owned();
                result.truncated = false;
            }
            Item::Assistant(assistant) => {
                for call in &mut assistant.tool_calls {
                    if let Some(args) = elided_calls.get(call.id.as_str()) {
                        call.arguments = (*args).clone();
                    }
                }
                if assistant.stop_reason != StopReason::ContentFilter {
                    if decisions.elided_reasoning.contains(&assistant_ordinal) {
                        assistant.reasoning = None;
                    }
                    if decisions.elided_assistant_text.contains(&assistant_ordinal) {
                        assistant.content = elide_text(&assistant.content);
                    }
                    assistant_ordinal += 1;
                }
            }
            Item::User { text } => {
                if decisions.elided_users.contains(&user_ordinal) {
                    *text = StubReason::ElidedUser.stub_text().to_owned();
                }
                user_ordinal += 1;
            }
        }
    }
    stubbed
}

/// Rewrites projected assistant tool-call arguments, user messages, and
/// assistant reasoning per the frozen elisions: calls keep their
/// signature (the elided arguments are stored verbatim in the
/// checkpoint), user items past the brief become a one-line stub, and
/// elided reasoning traces are dropped outright. User items are matched
/// by ordinal (0-based, transcript order) and assistants by their
/// ordinal among *non-`ContentFilter`* assistant items — the projection
/// filters those turns, so transcript indexes would not line up with
/// request messages (the request contains only the non-filtered
/// assistants, so its own `Message::Assistant` count matches the
/// ordinals exactly).
pub(super) fn apply_elisions(
    request: &mut crate::model::ModelRequest,
    elided_args: &[ArgElision],
    elided_users: &[usize],
    elided_reasoning: &[usize],
    elided_assistant_text: &[usize],
) {
    if elided_args.is_empty()
        && elided_users.is_empty()
        && elided_reasoning.is_empty()
        && elided_assistant_text.is_empty()
    {
        return;
    }
    let by_call: HashMap<&str, &ToolArguments> = elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for message in &mut request.messages {
        match message {
            Message::Assistant(assistant) => {
                for call in &mut assistant.tool_calls {
                    if let Some(args) = by_call.get(call.id.as_str()) {
                        call.arguments = (*args).clone();
                    }
                }
                if elided_reasoning.contains(&assistant_ordinal) {
                    assistant.reasoning = None;
                }
                if elided_assistant_text.contains(&assistant_ordinal) {
                    assistant.content = elide_text(&assistant.content);
                }
                assistant_ordinal += 1;
            }
            Message::User { text } => {
                if elided_users.contains(&user_ordinal) {
                    *text = StubReason::ElidedUser.stub_text().to_owned();
                }
                user_ordinal += 1;
            }
            _ => {}
        }
    }
}
