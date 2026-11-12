//! Request-side wire mechanics: adapter knobs and the
//! chat-completions body builder.

use loom_kernel::model::{Message, ModelRequest};
use loom_kernel::transcript::ToolResult;
use serde_json::{Value, json};

use super::REASONING_FIELD;

/// Provider-injected request knobs that differ between OpenAI-compatible
/// providers.
///
/// Provider-neutral core fields (`model`, `messages`, `stream`, `tools`,
/// `max_tokens`, `temperature`) are built by [`build_request_body`];
/// everything else a provider needs goes in [`WireOptions::extra_body`]
/// and is merged last, never overwriting a core field.
#[derive(Debug, Clone, Default)]
pub(crate) struct WireOptions {
    /// Extra top-level request fields (e.g. DeepSeek's `thinking` and
    /// `reasoning_effort`; a later GLM adapter adds `clear_thinking`).
    /// A core field already present wins, so an adapter cannot
    /// accidentally override the request shape.
    pub extra_body: serde_json::Map<String, Value>,
    /// Field name the replayed reasoning trace is written under on an
    /// assistant message. `None` keeps the [`REASONING_FIELD`] default;
    /// Groq passes [`REASONING_FIELD_PLAIN`] because its dialect names
    /// the trace `reasoning` and rejects the default key outright.
    pub reasoning_field: Option<&'static str>,
}

/// Builds the chat-completions request body for one [`ModelRequest`].
///
/// Shape (verified against the DeepSeek API reference, 2026-10-03):
/// `stream` is always `true` (the core's only model interface is the
/// stream), `stream_options.include_usage` is requested, `tool_choice`
/// is `"auto"` **only when tools are sent**, and provider-specific body
/// fields ([`WireOptions::extra_body`]) are merged in verbatim — without
/// overwriting a core field.
///
/// Reasoning replay: an assistant message carries its prior trace
/// whenever the context compiler replayed it — with `tools` in
/// the request, omitting the prior reasoning is an HTTP 400 on
/// DeepSeek, so replay here must stay aligned with compiler policy
/// (ADR-006). The key it is written under is dialect-specific
/// ([`WireOptions::reasoning_field`]). `provider_data` is never sent:
/// DeepSeek replays text only.
pub(crate) fn build_request_body(request: &ModelRequest, options: &WireOptions) -> Value {
    let reasoning_field = options.reasoning_field.unwrap_or(REASONING_FIELD);
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| match message {
            Message::System { text } => json!({"role": "system", "content": text}),
            Message::User { text } => json!({"role": "user", "content": text}),
            Message::Assistant(assistant) => {
                let mut value = json!({"role": "assistant", "content": assistant.content});
                if let Some(reasoning) = &assistant.reasoning {
                    value[reasoning_field] = json!(reasoning.text);
                }
                if !assistant.tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(
                        assistant
                            .tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id.as_str(),
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments.to_string(),
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                value
            }
            Message::ToolResult(result) => tool_result_message(result),
        })
        .collect();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        // Exactly one choice is wanted and the response is read from
        // `choices[0]` alone, so say so on the wire: a provider that
        // sent several would have all but the first silently dropped.
        "n": 1,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    // Provider-specific fields last, but a core field already present
    // wins: an adapter cannot reshape the request by accident.
    if let Some(map) = body.as_object_mut() {
        for (key, value) in &options.extra_body {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// Maps one transcript [`ToolResult`] to its wire form.
fn tool_result_message(result: &ToolResult) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.call_id.as_str(),
        "content": result.content,
    })
}
