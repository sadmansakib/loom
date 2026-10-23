//! Anthropic Messages API wire translation — pure, no I/O.
//!
//! The mirror of [`crate::openai_wire`] for the Anthropic Messages
//! protocol: request-body construction, SSE event assembly, stop-reason
//! mapping, and error classification. Everything here is unit-testable
//! without a key (roadmap item 14: the adapter pressures the
//! `ModelProvider` boundary offline).
//!
//! Wire shape (Messages API, `anthropic-version: 2023-06-01`):
//!
//! - the system prompt is a top-level `system` parameter, not a message;
//!   a `System` message that appears **after** the conversation has
//!   started is *not* hoisted there — it stays in place as an in-turn
//!   `system`-role message, the Fable 5.1+ steering form (e.g. a
//!   per-turn `output_config.effort` note) that applies to the next
//!   assistant turn without invalidating the cached prefix;
//! - conversation turns are `user`/`assistant` messages whose `content`
//!   is an array of typed blocks (`text`, `tool_use`, `tool_result`);
//! - tool results travel in `user`-role `tool_result` blocks keyed by
//!   `tool_use_id`;
//! - streaming terminates with a dispatched `message_stop` event (no
//!   OpenAI-style `[DONE]` sentinel);
//! - prompt caching is requested via `cache_control` markers on content
//!   blocks, placed by this adapter from the kernel's
//!   [`ModelRequest::stable_prefix_messages`] hint (the kernel itself
//!   stays provider-blind).
//!
//! Deviations from the OpenAI wire that matter here:
//!
//! - `max_tokens` is **required** — defaulted when the request omits it;
//! - `temperature` is **never sent**: Claude 4.7 and later (the whole
//!   5.5 family, Fable 5.1 included) reject a non-default value with a
//!   400, and adaptive thinking makes it moot on the current lineup.
//!   Loom has no surface that sets one; see [`build_request_body`];
//! - assistant `reasoning` traces are not forwardable (Anthropic has no
//!   replay field for foreign reasoning), so they are dropped on
//!   translation — only reasoning *received* from Anthropic is replayed,
//!   via its own blocks in later requests' assistant messages;
//! - failures can arrive as `error` events *inside* an otherwise-200
//!   SSE stream, not only as HTTP statuses.
//!
//! Consequences of the 5.5-generation changes that are *already*
//! satisfied here, and must stay satisfied:
//!
//! - **preserved thinking** — thinking blocks are bound to the model and
//!   the conversation that produced them, and replaying hand-edited ones
//!   is an API error. Loom never re-emits thinking blocks (reasoning is
//!   replayed as plain text, if at all), so it cannot violate this;
//! - **thinking cannot be disabled** on Opus 5.5 / Fable 5.1 — nothing
//!   to do, since the adapter never sends a `thinking` parameter and
//!   the table reports `reasoning: true` for every model;
//! - **forced tool use is an error** — the adapter never sends
//!   `tool_choice`, so `tool_choice: any` is simply unavailable.

use loom_kernel::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelRequest, ModelResponse, StopReason, Usage,
};
use loom_kernel::transcript::{AssistantMessage, ToolStatus};
use serde_json::{Value, json};

/// Header value for the required `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` fallback when the request carries none. Anthropic
/// requires the parameter; the kernel's compiler normally derives it
/// from capabilities, so this covers direct adapter use only.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8_192;

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
    body
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

/// Classifies a non-success HTTP status from the Messages API.
///
/// Anthropic error bodies are `{"type": "error", "error": {"type",
/// "message"}}`; the type names are matched where they change the
/// retry decision.
pub(crate) fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
    let kind = match status.as_u16() {
        400 => {
            // Anthropic reports context overflows as 400 with a
            // `context_length_exceeded`-family error type.
            if body.contains("context_length_exceeded") {
                ModelErrorKind::ContextOverflow
            } else {
                ModelErrorKind::InvalidRequest
            }
        }
        401 | 403 => ModelErrorKind::Auth,
        404 => ModelErrorKind::InvalidRequest,
        413 => ModelErrorKind::ContextOverflow,
        429 => ModelErrorKind::RateLimited,
        529 => ModelErrorKind::Overloaded,
        500..=599 => ModelErrorKind::Server,
        _ => ModelErrorKind::Server,
    };
    let message = extract_error_message(body).map_or_else(
        || format!("HTTP {status} from the Messages API"),
        |detail| format!("HTTP {status}: {detail}"),
    );
    ModelError::new(kind, message)
}

/// Maps an `error` event carried inside the stream to its kind.
pub(crate) fn classify_stream_error(body: &str) -> ModelError {
    let kind = match extract_error_type(body).as_deref() {
        Some("overloaded_error") => ModelErrorKind::Overloaded,
        Some("rate_limit_error") => ModelErrorKind::RateLimited,
        Some("api_error") => ModelErrorKind::Server,
        Some("invalid_request_error") => ModelErrorKind::InvalidRequest,
        Some("authentication_error") => ModelErrorKind::Auth,
        Some("permission_error") => ModelErrorKind::Auth,
        Some("not_found_error") => ModelErrorKind::InvalidRequest,
        Some("request_too_large") => ModelErrorKind::ContextOverflow,
        _ => ModelErrorKind::Server,
    };
    ModelError::new(
        kind,
        extract_error_message(body).map_or_else(
            || "error event inside the stream".to_owned(),
            |detail| format!("error event: {detail}"),
        ),
    )
}

/// Pulls `error.message` out of an Anthropic error body, falling back
/// to a bounded excerpt for non-JSON bodies.
fn extract_error_message(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok();
    if let Some(message) = parsed.as_ref().and_then(|value| {
        value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    }) {
        return Some(message.to_owned());
    }
    (!body.trim().is_empty()).then(|| crate::http::body_excerpt(body))
}

/// Pulls `error.type` out of an Anthropic error body.
fn extract_error_type(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .get("type")?
        .as_str()
        .map(str::to_owned)
}

/// Assembles a Messages API SSE stream into [`ModelEvent`]s.
///
/// Feed each dispatched SSE event's `data` JSON to [`Self::ingest`];
/// the final [`ModelEvent::Completed`] comes from [`Self::finalize`],
/// called when `message_stop` has been seen.
#[derive(Debug, Default)]
pub(crate) struct AnthropicAssembler {
    /// Accumulated text of the response, in block order.
    text: String,
    /// Blocks under assembly: text spans and tool calls.
    blocks: Vec<Block>,
    /// The block currently receiving deltas (SSE guarantees one open
    /// content block at a time).
    current: Option<Block>,
    /// Reasoning text, if the model emitted thinking deltas.
    reasoning: Option<String>,
    /// Stop reason from `message_delta`, if it arrived.
    stop_reason: Option<StopReason>,
    /// Usage from `message_start` (input side).
    input_usage: u64,
    cached_input: Option<u64>,
    /// Cache-write tokens from `message_start`, if reported (billed
    /// above the base input rate on Anthropic).
    cache_creation: Option<u64>,
    /// Thinking tokens from `message_delta` usage details, if reported
    /// (already counted in the output total — an attribution split).
    thinking: Option<u64>,
    /// Usage from `message_delta` (output side, cumulative).
    output_usage: Option<u64>,
}

/// One content block under assembly.
#[derive(Debug)]
enum Block {
    /// A text block: assembled span.
    Text,
    /// A tool call: id, name, and the raw accumulated argument JSON.
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

impl AnthropicAssembler {
    /// Ingests one SSE `data` payload; returns display deltas to emit
    /// and whether this event was the terminal `message_stop`.
    pub(crate) fn ingest(&mut self, data: &str) -> Result<(Vec<ModelEvent>, bool), ModelError> {
        let event: Value = serde_json::from_str(data).map_err(|error| {
            ModelError::new(
                ModelErrorKind::Protocol,
                format!("malformed Messages API event: {error}"),
            )
        })?;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message_start" => {
                self.ingest_message_start(&event);
                Ok((Vec::new(), false))
            }
            "content_block_start" => {
                // Close any open block (a stop event should have done
                // it; tolerate its absence rather than lose content).
                self.close_current();
                let block = &event["content_block"];
                self.current = Some(match block["type"].as_str() {
                    Some("tool_use") => Block::Tool {
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                        json: String::new(),
                    },
                    _ => Block::Text,
                });
                Ok((Vec::new(), false))
            }
            "content_block_delta" => Ok((self.ingest_block_delta(&event["delta"]), false)),
            "content_block_stop" => {
                self.close_current();
                Ok((Vec::new(), false))
            }
            "message_delta" => {
                self.ingest_message_delta(&event);
                Ok((Vec::new(), false))
            }
            "message_stop" => Ok((Vec::new(), true)),
            "ping" => Ok((Vec::new(), false)),
            "error" => Err(classify_stream_error(data)),
            other => Err(ModelError::new(
                ModelErrorKind::Protocol,
                format!("unknown Messages API event type {other:?}"),
            )),
        }
    }

    /// `message_start`: the input-side usage snapshot, including the
    /// cached split (Anthropic reports the cached subset separately, so
    /// the billable miss count is the difference).
    fn ingest_message_start(&mut self, event: &Value) {
        let usage = &event["message"]["usage"];
        self.input_usage = usage["input_tokens"].as_u64().unwrap_or(0);
        self.cached_input = usage["cache_read_input_tokens"].as_u64();
        self.cache_creation = usage["cache_creation_input_tokens"].as_u64();
        if let Some(cached) = self.cached_input {
            self.input_usage = self.input_usage.saturating_sub(cached);
        }
    }

    /// `content_block_delta`: appends the fragment to whichever buffer
    /// the open block owns and returns the display delta it produced
    /// (text and thinking fragments surface; JSON fragments accumulate
    /// silently).
    fn ingest_block_delta(&mut self, delta: &Value) -> Vec<ModelEvent> {
        match delta["type"].as_str() {
            Some("text_delta") => {
                let text = delta["text"].as_str().unwrap_or_default();
                self.text.push_str(text);
                vec![ModelEvent::TextDelta(text.to_owned())]
            }
            Some("input_json_delta") => {
                if let Some(Block::Tool { json, .. }) = self.current.as_mut() {
                    json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                }
                Vec::new()
            }
            Some("thinking_delta") => {
                let text = delta["thinking"].as_str().unwrap_or_default();
                self.reasoning
                    .get_or_insert_with(String::new)
                    .push_str(text);
                vec![ModelEvent::ReasoningDelta(text.to_owned())]
            }
            _ => Vec::new(),
        }
    }

    /// `message_delta`: the output-side usage snapshot and the stop
    /// reason, either of which the event may omit.
    fn ingest_message_delta(&mut self, event: &Value) {
        if let Some(reason) = event["delta"]["stop_reason"].as_str() {
            self.stop_reason = Some(map_stop_reason(reason));
        }
        if let Some(output) = event["usage"]["output_tokens"].as_u64() {
            self.output_usage = Some(output);
        }
        if let Some(thinking) = event["usage"]["output_tokens_details"]["thinking_tokens"].as_u64()
        {
            self.thinking = Some(thinking);
        }
    }

    /// Seals the open block, if any, into the finished list.
    fn close_current(&mut self) {
        if let Some(block) = self.current.take() {
            self.blocks.push(block);
        }
    }

    /// Builds the terminal response. Structural normalization mirrors
    /// the OpenAI wire's contract: `ToolCalls` without calls and calls
    /// with malformed argument JSON are salvaged (the engine refuses to
    /// dispatch them and reports the parse failure back to the model); a
    /// `MaxTokens` response discards partially-assembled calls.
    pub(crate) fn finalize(mut self) -> Result<ModelResponse, ModelError> {
        self.close_current();
        let wire_reason = self.stop_reason.unwrap_or(StopReason::EndTurn);
        // Truncation discards partial calls before parsing them: a
        // max_tokens stream can cut mid-JSON (mirrors the OpenAI
        // `length` + calls rule).
        if wire_reason == StopReason::MaxTokens {
            return Ok(ModelResponse {
                message: AssistantMessage {
                    content: self.text,
                    reasoning: self
                        .reasoning
                        .map(|text| loom_kernel::transcript::ReasoningTrace {
                            text,
                            provider_data: None,
                        }),
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::MaxTokens,
                },
                usage: Usage {
                    input_tokens: self.input_usage,
                    output_tokens: self.output_usage.unwrap_or(0),
                    cached_input_tokens: self.cached_input,
                    thinking_tokens: self.thinking,
                    cache_creation_input_tokens: self.cache_creation,
                    cost_usd: None,
                },
            });
        }
        let mut tool_calls = Vec::new();
        for block in &self.blocks {
            if let Block::Tool { id, name, json } = block {
                // Salvage instead of failing: unparseable or non-object
                // arguments become a placeholder object under the
                // reserved key (the engine refuses to dispatch it and
                // feeds the excerpt back so the model can reissue the
                // call) — one malformed block no longer kills the turn.
                // The expect is a proven invariant: an empty JSON
                // object is always valid ToolArguments.
                #[allow(clippy::expect_used)]
                let arguments = if json.trim().is_empty() {
                    loom_kernel::transcript::ToolArguments::try_from(json!({}))
                        .expect("the empty object is always valid ToolArguments")
                } else {
                    let parsed = serde_json::from_str::<Value>(json)
                        .map_err(|_| ())
                        .and_then(|value| {
                            loom_kernel::transcript::ToolArguments::try_from(value).map_err(|_| ())
                        });
                    match parsed {
                        Ok(arguments) => arguments,
                        Err(_) => loom_kernel::transcript::ToolArguments::malformed(json),
                    }
                };
                let id =
                    loom_kernel::transcript::ToolCallId::try_from(id.clone()).map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::Protocol,
                            format!("empty tool_use id for {name:?}"),
                        )
                    })?;
                tool_calls.push(loom_kernel::transcript::ToolCall {
                    id,
                    name: name.clone(),
                    arguments,
                });
            }
        }

        // Normalize: any response that produced calls is a ToolCalls
        // response regardless of the reported reason (mirrors the
        // OpenAI `stop` + calls rule).
        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolCalls
        } else if wire_reason == StopReason::ToolCalls {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "stop reason tool_use with zero tool calls",
            ));
        } else {
            wire_reason
        };

        let reasoning = self
            .reasoning
            .map(|text| loom_kernel::transcript::ReasoningTrace {
                text,
                provider_data: None,
            });
        Ok(ModelResponse {
            message: AssistantMessage {
                content: self.text,
                reasoning,
                tool_calls,
                stop_reason,
            },
            usage: Usage {
                input_tokens: self.input_usage,
                output_tokens: self.output_usage.unwrap_or(0),
                cached_input_tokens: self.cached_input,
                thinking_tokens: self.thinking,
                cache_creation_input_tokens: self.cache_creation,
                cost_usd: None,
            },
        })
    }
}

/// Maps a Messages API stop reason to the neutral one.
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolCalls,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        other => {
            tracing::debug!(target: "model", stop_reason_raw = other, "unmapped stop reason");
            StopReason::EndTurn
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::model::Message;
    use loom_kernel::transcript::{
        ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
    };

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            model: "claude-sonnet-4-5".into(),
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            stable_prefix_messages: None,
        }
    }

    fn assistant_call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).unwrap(),
            name: "read_file".into(),
            arguments: ToolArguments::try_from(json!({"path": "x.rs"})).unwrap(),
        }
    }

    #[test]
    fn system_prompt_moves_to_the_top_level_parameter() {
        let body = build_request_body(&request(vec![
            Message::System { text: "sys".into() },
            Message::User { text: "hi".into() },
        ]));
        assert_eq!(body["system"], json!("sys"));
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn max_tokens_is_required_so_it_defaults() {
        let body = build_request_body(&request(vec![Message::User { text: "hi".into() }]));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn temperature_is_never_forwarded() {
        // Claude 4.7+ answer a non-default temperature with a 400, so
        // the adapter drops the field even when the caller set one.
        let mut req = request(vec![Message::User { text: "hi".into() }]);
        req.temperature = Some(0.2);
        let body = build_request_body(&req);
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn assistant_turns_translate_to_blocks_and_drop_reasoning() {
        let body = build_request_body(&request(vec![
            Message::System { text: "sys".into() },
            Message::Assistant(AssistantMessage {
                content: "reading".into(),
                reasoning: Some(ReasoningTrace {
                    text: "plan".into(),
                    provider_data: None,
                }),
                tool_calls: vec![assistant_call("toolu_1")],
                stop_reason: StopReason::ToolCalls,
            }),
            Message::ToolResult(ToolResult::new(
                ToolCallId::try_from("toolu_1".to_owned()).unwrap(),
                ToolStatus::Success,
                "file body",
                false,
            )),
        ]));
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "toolu_1");
        assert_eq!(assistant["content"][1]["input"], json!({"path": "x.rs"}));
        let result = &body["messages"][1];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(result["content"][0]["is_error"], json!(false));
        assert!(assistant.get("reasoning").is_none());
    }

    #[test]
    fn failed_tool_results_are_flagged() {
        let body = build_request_body(&request(vec![
            Message::Assistant(AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![assistant_call("toolu_1")],
                stop_reason: StopReason::ToolCalls,
            }),
            Message::ToolResult(ToolResult::new(
                ToolCallId::try_from("toolu_1".to_owned()).unwrap(),
                ToolStatus::Error(loom_kernel::transcript::ToolFailure::Execution),
                "boom",
                false,
            )),
        ]));
        assert_eq!(body["messages"][1]["content"][0]["is_error"], json!(true));
    }

    #[test]
    fn tools_become_input_schema_entries() {
        let mut req = request(vec![Message::User { text: "hi".into() }]);
        req.tools = vec![loom_kernel::tool::ToolSpec {
            name: "read_file".into(),
            description: "reads".into(),
            parameters: json!({"type": "object"}),
        }];
        let body = build_request_body(&req);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
    }

    #[test]
    fn cache_control_lands_on_the_stable_prefix_boundary() {
        // prefix = 2 covers system + first user turn → marker on turn 0.
        let mut req = request(vec![
            Message::System { text: "sys".into() },
            Message::User {
                text: "task".into(),
            },
            Message::Assistant(AssistantMessage {
                content: "working".into(),
                reasoning: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
            }),
        ]);
        req.stable_prefix_messages = Some(2);
        let body = build_request_body(&req);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        // prefix = 3 would mark the assistant turn instead.
        req.stable_prefix_messages = Some(3);
        let body = build_request_body(&req);
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn cache_control_on_the_system_when_the_prefix_covers_only_it() {
        let mut req = request(vec![
            Message::System { text: "sys".into() },
            Message::User {
                text: "task".into(),
            },
        ]);
        req.stable_prefix_messages = Some(1);
        let body = build_request_body(&req);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn no_cache_control_without_a_hint() {
        let body = build_request_body(&request(vec![
            Message::System { text: "sys".into() },
            Message::User { text: "hi".into() },
        ]));
        assert!(body["system"].is_string());
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn in_turn_system_messages_stay_in_place() {
        // The Fable 5.1+ steering form: a System message after the
        // conversation has started must NOT fold into the top-level
        // `system` parameter — hoisting it would rewrite the cached
        // prefix and widen its effect to the whole request.
        let body = build_request_body(&request(vec![
            Message::System {
                text: "global prompt".into(),
            },
            Message::User {
                text: "task".into(),
            },
            Message::System {
                text: "output_config: effort high for this turn".into(),
            },
            Message::User { text: "go".into() },
        ]));
        // Only the leading system message reaches the top level.
        assert_eq!(body["system"], json!("global prompt"));
        // The in-turn system message keeps its position and role.
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(body["messages"][1]["role"], json!("system"));
        assert_eq!(
            body["messages"][1]["content"],
            json!([{
                "type": "text",
                "text": "output_config: effort high for this turn"
            }])
        );
        assert_eq!(body["messages"][2]["role"], json!("user"));
    }

    #[test]
    fn a_stable_prefix_marks_turns_before_an_in_turn_system_message() {
        // Breakpoint indexing counts an in-turn system message like any
        // other turn: prefix = 2 (leading system + first user) marks
        // the user turn, never the steering message that follows it.
        let mut req = request(vec![
            Message::System { text: "sys".into() },
            Message::User {
                text: "task".into(),
            },
            Message::System {
                text: "steer".into(),
            },
            Message::User { text: "go".into() },
        ]);
        req.stable_prefix_messages = Some(2);
        let body = build_request_body(&req);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        for turn in 1..3 {
            assert!(
                body["messages"][turn]["content"][0]
                    .get("cache_control")
                    .is_none()
            );
        }
    }

    #[test]
    fn thinking_and_cache_creation_usage_are_accounted() {
        let mut assembler = AnthropicAssembler::default();
        for data in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50,"output_tokens_details":{"thinking_tokens":30}}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler.ingest(data).unwrap();
        }
        let response = assembler.finalize().unwrap();
        assert_eq!(
            response.message.reasoning.as_ref().map(|r| r.text.as_str()),
            Some("ponder")
        );
        assert_eq!(response.usage.input_tokens, 60);
        assert_eq!(response.usage.cached_input_tokens, Some(40));
        assert_eq!(response.usage.cache_creation_input_tokens, Some(10));
        // Thinking tokens are an attribution split of the output total.
        assert_eq!(response.usage.thinking_tokens, Some(30));
        assert_eq!(response.usage.output_tokens, 50);
    }

    #[test]
    fn statuses_map_to_stable_kinds() {
        assert_eq!(
            classify_status(reqwest::StatusCode::UNAUTHORIZED, "").kind,
            ModelErrorKind::Auth
        );
        assert_eq!(
            classify_status(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            )
            .kind,
            ModelErrorKind::RateLimited
        );
        assert_eq!(
            classify_status(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: context_length_exceeded"}}"#
            )
            .kind,
            ModelErrorKind::ContextOverflow
        );
        assert_eq!(
            classify_status(
                reqwest::StatusCode::from_u16(529).unwrap(),
                r#"{"error":{"type":"overloaded_error","message":"overloaded"}}"#
            )
            .kind,
            ModelErrorKind::Overloaded
        );
    }

    #[test]
    fn a_full_stream_assembles_into_a_response() {
        let mut assembler = AnthropicAssembler::default();
        let events: Vec<&str> = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":40}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Rea"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ding."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":57}}"#,
            r#"{"type":"message_stop"}"#,
        ]
        .into();
        let mut deltas = 0;
        for data in events {
            for event in assembler.ingest(data).unwrap().0 {
                assert!(matches!(
                    event,
                    ModelEvent::TextDelta(_) | ModelEvent::ReasoningDelta(_)
                ));
                deltas += 1;
            }
        }
        assert_eq!(deltas, 2);
        let response = assembler.finalize().unwrap();
        assert_eq!(response.message.content, "Reading.");
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].name, "read_file");
        assert_eq!(response.usage.input_tokens, 60);
        assert_eq!(response.usage.cached_input_tokens, Some(40));
        assert_eq!(response.usage.output_tokens, 57);
        assert_eq!(response.usage.cost_usd, None);
    }

    #[test]
    fn thinking_deltas_become_a_reasoning_trace() {
        let mut assembler = AnthropicAssembler::default();
        for data in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler.ingest(data).unwrap();
        }
        let response = assembler.finalize().unwrap();
        assert_eq!(
            response.message.reasoning.as_ref().map(|r| r.text.as_str()),
            Some("pondering")
        );
        assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn tool_use_with_zero_calls_is_a_protocol_error() {
        let mut assembler = AnthropicAssembler::default();
        for data in [
            r#"{"type":"message_start","message":{"usage":{}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler.ingest(data).unwrap();
        }
        assert_eq!(
            assembler.finalize().unwrap_err().kind,
            ModelErrorKind::Protocol
        );
    }

    #[test]
    fn max_tokens_discards_partial_calls() {
        let mut assembler = AnthropicAssembler::default();
        for data in [
            r#"{"type":"message_start","message":{"usage":{}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler.ingest(data).unwrap();
        }
        let response = assembler.finalize().unwrap();
        assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
        assert!(response.message.tool_calls.is_empty());
    }

    #[test]
    fn malformed_argument_json_is_salvaged_not_fatal() {
        let mut assembler = AnthropicAssembler::default();
        for data in [
            r#"{"type":"message_start","message":{"usage":{}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not json"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler.ingest(data).unwrap();
        }
        let response = assembler.finalize().unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.message.tool_calls.len(), 1);
        let call = &response.message.tool_calls[0];
        assert_eq!(call.id.as_str(), "toolu_1");
        assert_eq!(call.name, "read_file");
        assert!(call.arguments_are_malformed());
    }

    #[test]
    fn error_events_inside_the_stream_fail_the_request() {
        let mut assembler = AnthropicAssembler::default();
        let error = assembler
            .ingest(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
            )
            .unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Overloaded);
    }
}
