use crate::context::{ContextCensus, TokenEstimator, estimate_serialized};
use crate::model::Message;
use crate::model::ModelRequest;

/// Serialized byte length of a value, or 0 if serialization fails (it cannot
/// for the types in this crate; the fallback keeps the census panic-free).
pub(in crate::context) fn serialized_size<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_string(value)
        .map(|json| json.len() as u64)
        .unwrap_or(0)
}

/// Attributes the request's estimated context to its sources (ADR-016, V18).
///
/// `facts_tokens` is the estimated tokens of the repository facts
/// message, subtracted from the system-prompt attribution so the two system
/// messages in the prefix never double-count.
///
/// Per-source figures are the estimator's tokens over each serialized
/// message (or tool spec); `total` is the authoritative estimate: the
/// whole message list plus tool list, serialized once and estimated,
/// plus one token per message for framing (§8). The two are computed
/// from the same payload, so `total` is exactly the number the budget
/// check compares.
pub(in crate::context) fn census(
    request: &ModelRequest,
    facts_tokens: u32,
    estimator: &dyn TokenEstimator,
) -> ContextCensus {
    let mut system_total = 0u32;
    let mut user_items = 0u32;
    let mut assistant_history = 0u32;
    let mut reasoning_replay = 0u32;
    let mut tool_results = 0u32;
    // Serialize each message exactly once, then reuse those bytes for
    // attribution and compose the list payload from them for the
    // authoritative total. The previous implementation serialized each
    // message for attribution *and* serialized the whole list again.
    let serialized_messages: Vec<String> = request.messages.iter().map(serialize_compact).collect();
    for (message, serialized) in request.messages.iter().zip(&serialized_messages) {
        let message_tokens = estimator.estimate(serialized);
        match message {
            Message::System { .. } => {
                system_total = system_total.saturating_add(message_tokens);
            }
            Message::User { .. } => {
                user_items = user_items.saturating_add(message_tokens);
            }
            Message::Assistant(assistant) => {
                // Reasoning is attributed on its own; the rest of the
                // assistant message (text and tool-call arguments) stays in
                // `assistant_history`, so the two never double-count.
                let reasoning = assistant
                    .reasoning
                    .as_ref()
                    .map_or(0, |trace| estimate_serialized(estimator, trace));
                reasoning_replay = reasoning_replay.saturating_add(reasoning);
                assistant_history =
                    assistant_history.saturating_add(message_tokens.saturating_sub(reasoning));
            }
            Message::ToolResult(_) => {
                tool_results = tool_results.saturating_add(message_tokens);
            }
        }
    }
    let serialized_tools: Vec<String> = request.tools.iter().map(serialize_compact).collect();
    let tool_specs = serialized_tools
        .iter()
        .map(|json| estimator.estimate(json))
        .fold(0u32, u32::saturating_add);
    let messages = compose_json_array(&serialized_messages);
    let tools = compose_json_array(&serialized_tools);
    // Saturating on each half: a compiler that reports `u32::MAX`
    // estimated tokens will simply exceed any budget.
    let total = estimator
        .estimate(&messages)
        .saturating_add(estimator.estimate(&tools))
        .saturating_add(u32::try_from(request.messages.len()).unwrap_or(u32::MAX));

    ContextCensus {
        // Everything in a `System` message that is not the facts block is the
        // system prompt; the two are the only system messages projected.
        system_prompt: system_total.saturating_sub(facts_tokens),
        repo_facts: facts_tokens,
        user_items,
        assistant_history,
        reasoning_replay,
        tool_results,
        tool_specs,
        total,
    }
}

/// Compact JSON for one infallibly serializable schema value. This
/// replaces `estimate_serialized` where the byte string is reused for
/// both attribution and the aggregate payload.
pub(super) fn serialize_compact<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Composes the already-serialized array members into exactly the JSON
/// byte shape `serde_json::to_string(&Vec<T>)` emits: `[`, comma-joined
/// compact members, `]`. Stable ordering and whitespace are unchanged.
pub(super) fn compose_json_array(items: &[String]) -> String {
    let capacity = items.iter().map(String::len).sum::<usize>() + items.len() + 1;
    let mut out = String::with_capacity(capacity);
    out.push('[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(item);
    }
    out.push(']');
    out
}
