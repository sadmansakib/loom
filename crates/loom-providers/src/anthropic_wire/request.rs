//! Request-side wire mechanics: the Messages API body builder.

use loom_kernel::model::{ContextEditingHint, Message, ModelRequest, ReasoningEffort};
use loom_kernel::transcript::{AssistantMessage, ToolStatus};
use serde_json::{Value, json};

use super::DEFAULT_MAX_TOKENS;

/// Builds the Messages API request body for `request`.
///
/// Deterministic for a given `ModelRequest` (stable key order via
/// `json!`/`serde_json` object maps). `cache_control` is placed on the
/// last content block of the stable-prefix boundary indicated by
/// [`ModelRequest::stable_prefix_messages`]: the canonical
/// incremental-prompting breakpoint. When the prefix covers only the
/// system prompt, the marker lands on the system parameter instead.
///
/// `request.temperature` is deliberately ignored: Claude 4.7 and later
/// answer a non-default value with a 400 (`temperature`, `top_p`, and
/// `top_k` are unsupported), and no Loom surface sets one. Sending the
/// provider default and staying compatible beats honoring a field the
/// API rejects.
pub(crate) fn build_request_body(request: &ModelRequest) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<Value> = Vec::new();
    // Only the leading contiguous System messages fold into the
    // top-level `system` parameter. A System message that appears after
    // the conversation has started stays in place as an in-turn
    // system-role message: hoisting it would move its effect from "the
    // next assistant turn" to "the whole request" and rewrite the
    // cached prefix — exactly what the per-turn steering form exists
    // to avoid.
    let mut still_leading = true;
    let mut system_count = 0usize;

    for message in &request.messages {
        match message {
            Message::System { text } => {
                if still_leading {
                    system_parts.push(text.clone());
                    system_count += 1;
                } else {
                    turns.push(json!({
                        "role": "system",
                        "content": [{"type": "text", "text": text}],
                    }));
                }
            }
            Message::User { text } => {
                still_leading = false;
                turns.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": text}],
                }));
            }
            Message::Assistant(inner) => {
                still_leading = false;
                turns.push(assistant_message(inner));
            }
            Message::ToolResult(result) => {
                still_leading = false;
                let is_error = !matches!(result.status, ToolStatus::Success);
                turns.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": result.call_id.as_str(),
                        "content": result.content,
                        "is_error": is_error,
                    }],
                }));
            }
        }
    }

    // Place the cache breakpoint. `stable_prefix_messages` counts all
    // leading messages, system included: a boundary inside the system
    // prompt marks the system parameter; a boundary past it marks the
    // last content block of the (n - system - 1)-th translated turn.
    let mut mark_system = false;
    if let Some(prefix) = request.stable_prefix_messages.filter(|&n| n >= 1) {
        if prefix <= system_count {
            mark_system = !system_parts.is_empty();
        } else if let Some(block) = turns
            .get_mut(prefix - system_count - 1)
            .and_then(|turn| turn.get_mut("content"))
            .and_then(|content| content.as_array_mut())
            .and_then(|blocks| blocks.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }

    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
        "messages": turns,
    });
    if !system_parts.is_empty() {
        if mark_system {
            body["system"] = json!([{
                "type": "text",
                "text": system_parts.join("\n\n"),
                "cache_control": {"type": "ephemeral"},
            }]);
        } else {
            body["system"] = json!(system_parts.join("\n\n"));
        }
    }
    // `request.temperature` is intentionally dropped: Claude 4.7 and
    // later answer a non-default `temperature`/`top_p`/`top_k` with a
    // 400, and no Loom surface sets one.
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect(),
        );
    }
    // Server-side context editing (roadmap item 12): the
    // provider-agnostic hint becomes Anthropic's
    // `clear_tool_uses_20250919` strategy. `trigger` and `keep` are
    // always explicit — replaying the request bytes must reconstruct
    // behavior without unstated provider defaults. The matching beta
    // header is attached in `anthropic.rs` by detecting this key, so
    // the header cannot drift from the body.
    if let Some(hint) = &request.context_editing {
        apply_context_editing(&mut body, hint);
    }
    // Per-turn reasoning effort (roadmap item 9's Anthropic tail):
    // the Messages API has no request field for a level — thinking is
    // adaptive and the adapter never sends a `thinking` parameter
    // (module notes) — so the level rides the same in-turn
    // system-message channel steering uses. It is appended after the
    // cache breakpoint is placed: the stable prefix, its breakpoint,
    // and the translated turns' indices are untouched, and the
    // message lands at the tail, where the next assistant turn reads
    // it. Absent effort (the kernel default) sends nothing — the
    // request bytes are unchanged for models and schedules that never
    // set a level.
    if let Some(effort) = request.reasoning_effort {
        apply_reasoning_effort(&mut body, effort);
    }
    body
}

/// Builds the `context_management` parameter from the provider-agnostic
/// hint (see [`build_request_body`]).
fn apply_context_editing(body: &mut Value, hint: &ContextEditingHint) {
    let mut edit = json!({
        "type": "clear_tool_uses_20250919",
        "trigger": {"type": "input_tokens", "value": hint.trigger_tokens},
        "keep": {"type": "tool_uses", "value": hint.keep_tool_uses},
    });
    if let Some(clear_at_least) = hint.clear_at_least_tokens {
        edit["clear_at_least"] = json!({
            "type": "input_tokens",
            "value": clear_at_least,
        });
    }
    if !hint.exclude_tools.is_empty() {
        edit["exclude_tools"] = json!(hint.exclude_tools);
    }
    body["context_management"] = json!({"edits": [edit]});
}

/// Appends the in-turn system message that carries a per-turn reasoning
/// effort (see [`build_request_body`]).
fn apply_reasoning_effort(body: &mut Value, effort: ReasoningEffort) {
    if let Some(messages) = body["messages"].as_array_mut() {
        messages.push(json!({
            "role": "system",
            "content": [{
                "type": "text",
                "text": format!("Reasoning effort for this turn: {}.", effort.as_str()),
            }],
        }));
    }
}

/// Translates one assistant turn into a Messages API message value.
///
/// Reasoning is deliberately not forwarded: Anthropic has no field for
/// foreign reasoning traces, and only reasoning it produced itself is
/// replayable (via its own assistant blocks).
fn assistant_message(inner: &AssistantMessage) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    if !inner.content.is_empty() {
        blocks.push(json!({"type": "text", "text": inner.content}));
    }
    for call in &inner.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id.as_str(),
            "name": call.name,
            "input": *call.arguments.clone(),
        }));
    }
    json!({"role": "assistant", "content": blocks})
}
