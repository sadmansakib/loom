use crate::model::Message;
use crate::session::Session;
use crate::transcript::{Item, ToolArguments};

use super::config::PruneConfig;
use super::facts::{TranscriptFacts, turn_of};
use super::strategies::DecisionSet;
use super::types::{ArgElision, StubReason};
use crate::context::{TokenEstimator, estimate_serialized};

/// The constant signature an elided assistant text becomes: its first
/// [`ASSISTANT_TEXT_KEEP_CHARS`] characters (never splitting a UTF-8
/// boundary) plus a `[pruned N chars]` marker naming the full length.
///
/// A constant, not a config knob: frozen elisions re-apply this to the
/// immutable transcript text, so byte-stability across config changes
/// requires the replacement to depend only on the text itself.
pub(super) fn elide_text(text: &str) -> String {
    // Idempotent: an already-elided signature (200 leading characters
    // plus a short marker) is shorter than the keep window plus marker
    // margin, so re-applying an elision — the rescue arm bakes one into
    // its view and the frozen re-apply then runs over that projection —
    // leaves it byte-identical instead of nesting markers.
    if text.chars().count() <= ASSISTANT_TEXT_KEEP_CHARS + 64 {
        return text.to_owned();
    }
    // Byte offset just past the ASSISTANT_TEXT_KEEP_CHARS-th character:
    // the keep window counts characters, not bytes.
    let mut boundary = 0;
    for (seen, (offset, ch)) in text.char_indices().enumerate() {
        if seen == ASSISTANT_TEXT_KEEP_CHARS {
            boundary = offset;
            break;
        }
        boundary = offset + ch.len_utf8();
    }
    let mut signature = String::from(&text[..boundary]);
    signature.push_str(&format!(" [pruned {} chars]", text.chars().count()));
    signature
}

/// Leading characters an elided assistant text keeps.
const ASSISTANT_TEXT_KEEP_CHARS: usize = 200;

/// Pass 6 — argument elision: tool calls older than `elide_turns`
/// assistant turns have their arguments reduced to a signature — every
/// string longer than `elide_arg_chars` becomes `[pruned N chars]`,
/// while names, ids, paths and flags survive. The elided arguments are
/// frozen verbatim, so re-application is byte-stable by construction.
/// Mutating calls included: their arguments (edit payloads) are the
/// bucket result stubbing never touches. The protected tail always
/// wins.
pub(super) fn elide_aged_calls(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_turns else {
        return;
    };
    for call in &facts.calls {
        if call.index >= protected_from
            || total_turns.saturating_sub(call.turn) <= max_age
            || pass.elided_arg_ids.contains(call.call_id)
        {
            continue;
        }
        let Some(elided) = elide_arguments(call.arguments, config.elide_arg_chars) else {
            continue;
        };
        let reclaim = estimate_serialized(estimator, &call.arguments)
            .saturating_sub(estimate_serialized(estimator, &elided));
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_arg_ids.insert(call.call_id.to_owned());
        pass.elided_args.push(ArgElision {
            call_id: call.call_id.to_owned(),
            arguments: elided,
        });
    }
}

/// Pass 7 — user-item elision: user items *superseded by a later
/// instruction* (the last user item — the live task's brief under
/// `run_task`/`continue_session` follow-ups — is never elided, nor is
/// the session's first brief) older than `elide_turns` assistant turns
/// become a one-line stub. Completion-gate refusals and follow-up
/// prompts age out like any other history instead of stacking verbatim
/// forever. The protected tail always wins.
///
/// Task-poison guard: eliding only superseded items means the compiler
/// can never strip the instructions the *current* task runs on — every
/// live brief is either ordinal 0 or the last user item, and both are
/// exempt by construction.
pub(super) fn elide_aged_users(
    facts: &TranscriptFacts,
    transcript: &[Item],
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_turns else {
        return;
    };
    let Some(&last_user_index) = facts.user_at.last() else {
        return;
    };
    let stub = Message::User {
        text: StubReason::ElidedUser.stub_text().to_owned(),
    };
    let stub_tokens = estimate_serialized(estimator, &stub);
    for (ordinal, &index) in facts.user_at.iter().enumerate() {
        if ordinal == 0
            || index >= last_user_index
            || pass.elided_users.contains(&ordinal)
            || index >= protected_from
            || total_turns.saturating_sub(turn_of(facts, index)) <= max_age
        {
            continue;
        }
        let Item::User { text } = &transcript[index] else {
            continue;
        };
        let full = Message::User { text: text.clone() };
        let reclaim = estimate_serialized(estimator, &full).saturating_sub(stub_tokens);
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_users.push(ordinal);
    }
}

/// Pass 8 — reasoning elision: assistant reasoning traces older than
/// `elide_reasoning` turns are dropped from the projection entirely
/// (`reasoning = None` — no stub text; a reasoning slot is either the
/// provider's verbatim trace or absent, never paraphrased).
///
/// Provider-gated: the default is `None`, and only the engine's
/// provider gate enables it — omitting replayed reasoning is a hard
/// error on DeepSeek's tool-bearing wire but costs GLM only an
/// episodic cache miss, and Anthropic never forwards reasoning on
/// replay. The protected tail always wins. Ordinals are among
/// non-`ContentFilter` assistants, matching what the projection (and
/// thus the request) contains.
pub(super) fn elide_aged_reasoning(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_reasoning else {
        return;
    };
    for trace in &facts.reasoning_at {
        if trace.index >= protected_from
            || total_turns.saturating_sub(trace.turn) <= max_age
            || trace.tokens == 0
            || pass.elided_reasoning.contains(&trace.ordinal)
        {
            continue;
        }
        pass.reclaim_tokens += u64::from(trace.tokens);
        pass.elided_reasoning.push(trace.ordinal);
    }
}

/// Pass 9 — assistant-text elision (last resort, deterministic): texts
/// older than `elide_assistant_text` assistant turns and longer than
/// `elide_assistant_chars` are reduced to their leading signature —
/// first 200 characters plus a `[pruned N chars]` marker — *only when
/// the window already triggered or the budget-rescue pass is running*.
/// A routine aging batch never rewrites prose the model may still
/// anchor on; this pass exists so the rescue arm has one more
/// deterministic lever before the engine's LLM handoff (roadmap item
/// 12). The protected tail always wins. Ordinals are among
/// non-`ContentFilter` assistants, matching what the projection
/// contains.
pub(super) fn elide_aged_assistant_text(
    facts: &TranscriptFacts,
    session: &Session,
    protected_from: usize,
    config: PruneConfig,
    window_triggered: bool,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let total_turns = facts.assistant_at.len();
    let Some(max_age) = config.elide_assistant_text else {
        return;
    };
    if !window_triggered {
        return;
    }
    for text in &facts.assistant_text_at {
        if text.index >= protected_from
            || text.chars <= config.elide_assistant_chars
            || total_turns.saturating_sub(text.turn) <= max_age
            || pass.elided_assistant_text.contains(&text.ordinal)
        {
            continue;
        }
        let Item::Assistant(assistant) = &session.transcript[text.index] else {
            continue;
        };
        let full = estimate_serialized(estimator, &assistant.content);
        let reclaim = full.saturating_sub(text.signature_tokens);
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_assistant_text.push(text.ordinal);
    }
}

/// Reduces `args` to its signature: every string longer than
/// `min_chars` (recursively, in objects and arrays alike) is replaced
/// with `[pruned N chars]`; shorter strings and all non-string scalars
/// survive. Returns `None` when nothing changed — the caller treats
/// that as "nothing to elide" and skips the call entirely.
fn elide_arguments(args: &ToolArguments, min_chars: usize) -> Option<ToolArguments> {
    let mut value = serde_json::Value::from(args.clone());
    if !elide_strings(&mut value, min_chars) {
        return None;
    }
    ToolArguments::try_from(value).ok()
}

/// Rewrites long strings inside `value` in place; reports whether
/// anything changed. Deterministic: the replacement names the elided
/// length, so two runs over the same arguments agree byte for byte.
fn elide_strings(value: &mut serde_json::Value, min_chars: usize) -> bool {
    match value {
        serde_json::Value::String(text) => {
            if text.len() > min_chars {
                *text = format!("[pruned {} chars]", text.len());
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .map(|item| elide_strings(item, min_chars))
            .fold(false, |changed, item_changed| changed | item_changed),
        serde_json::Value::Object(fields) => fields
            .iter_mut()
            .map(|(_, item)| elide_strings(item, min_chars))
            .fold(false, |changed, item_changed| changed | item_changed),
        _ => false,
    }
}
