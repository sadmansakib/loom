//! Shared OpenAI chat-completions wire mapping.
//!
//! Everything here is provider-neutral *wire mechanics*: translating a
//! [`ModelRequest`] into an OpenAI chat-completions JSON body, parsing
//! streamed chunks back, accumulating tool-call fragments by `index`,
//! and normalizing stop signals per architecture §5.1. Provider-specific
//! policy — error classification, in-stream failure reasons, thinking
//! flags, capability tables — is injected by the adapter (e.g.
//! [`crate::DeepSeekProvider`]) so a later OpenAI-compatible adapter
//! reuses this module wholesale.

use std::collections::BTreeMap;

use loom_core::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelRequest, ModelResponse, StopReason, Usage,
};
use loom_core::transcript::{
    AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The SSE terminator every OpenAI-compatible chat API sends.
pub(crate) const DONE_MARKER: &str = "[DONE]";

/// Provider-injected request knobs that differ between OpenAI-compatible
/// providers.
#[derive(Debug, Clone)]
pub(crate) struct WireOptions {
    /// Serialized `thinking` object (e.g. `{"type":"enabled"}` for
    /// DeepSeek; a later GLM adapter adds `clear_thinking`).
    pub thinking: Value,
    /// Serialized `reasoning_effort` (e.g. `"high"`); always explicit so
    /// endpoint-default changes cannot silently alter behaviour.
    pub reasoning_effort: String,
}

/// Builds the chat-completions request body for one [`ModelRequest`].
///
/// Shape (verified against the DeepSeek API reference, 2026-10-03):
/// `stream` is always `true` (the core's only model interface is the
/// stream), `stream_options.include_usage` is requested, `tool_choice`
/// is `"auto"` **only when tools are sent**, and provider thinking
/// knobs are merged in verbatim.
///
/// Reasoning replay: an assistant message carries `reasoning_content`
/// whenever the context compiler replayed its trace — with `tools` in
/// the request, omitting any prior `reasoning_content` is an HTTP 400 on
/// DeepSeek, so replay here must stay aligned with compiler policy
/// (ADR-006). `provider_data` is never sent: DeepSeek replays text only.
pub(crate) fn build_request_body(request: &ModelRequest, options: &WireOptions) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| match message {
            Message::System { text } => json!({"role": "system", "content": text}),
            Message::User { text } => json!({"role": "user", "content": text}),
            Message::Assistant(assistant) => {
                let mut value = json!({"role": "assistant", "content": assistant.content});
                if let Some(reasoning) = &assistant.reasoning {
                    value["reasoning_content"] = json!(reasoning.text);
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
        "stream": true,
        "stream_options": {"include_usage": true},
        "thinking": options.thinking,
        "reasoning_effort": options.reasoning_effort,
    });
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

/// Deserializes a field treating an explicit `null` as the default.
///
/// `#[serde(default)]` alone only covers a *missing* field; some
/// OpenAI-compatible servers send `"tool_calls": null` or
/// `"choices": null`, which would otherwise fail the whole chunk (and
/// with it the stream) as a protocol error.
fn default_on_null<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let value = Option::<T>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

/// One streamed chat chunk (`data:` payload), tolerating absent fields.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChatChunk {
    /// Usually exactly one; may be empty on usage-only chunks.
    #[serde(default, deserialize_with = "default_on_null")]
    pub choices: Vec<ChunkChoice>,
    /// Rides on the final content chunk (DeepSeek) when requested via
    /// `stream_options.include_usage`.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// One `choices[]` entry.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChunkChoice {
    /// The incremental payload (may be entirely absent).
    #[serde(default, deserialize_with = "default_on_null")]
    pub delta: ChunkDelta,
    /// `null` until the final content chunk of the choice.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The `delta` object of a streamed choice.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChunkDelta {
    /// Visible text fragment.
    #[serde(default)]
    pub content: Option<String>,
    /// Reasoning ("thinking") fragment.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Tool-call fragments; the first for an `index` carries `id` and
    /// `function.name`, later ones only append `function.arguments`.
    #[serde(default, deserialize_with = "default_on_null")]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One `delta.tool_calls[]` fragment.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolCallDelta {
    /// Correlates the fragments of one call. DeepSeek (and OpenAI)
    /// always send it; a gateway that omits it is routed by `id` (or by
    /// sole-pending-call) in [`ChunkAccumulator::ingest`].
    #[serde(default)]
    pub index: Option<u32>,
    /// Present only on the first fragment of the call.
    #[serde(default)]
    pub id: Option<String>,
    /// Present only on the first fragment of the call.
    #[serde(default)]
    pub function: Option<ToolCallFunctionDelta>,
}

/// The `function` sub-object of a tool-call fragment.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolCallFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// A JSON-text fragment; concatenated across fragments.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// The `usage` object of a chunk.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// DeepSeek-specific; absent on providers without cache reporting.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
}

/// One accumulated tool call under construction.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    /// Concatenated `arguments` fragments.
    arguments: String,
}

/// Accumulates streamed chunks and applies §5.1 stop-signal
/// normalization at the end.
///
/// Deltas are exposed for live display as they arrive; tool-call
/// fragments are **never** streamed out — the core only ever sees
/// complete, validated calls inside the final
/// [`ModelEvent::Completed`].
#[derive(Debug, Default)]
pub(crate) struct ChunkAccumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u32, PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<WireUsage>,
    /// Set when an index-less fragment could not be correlated to a
    /// call; finalize turns this into a Protocol error instead of
    /// silently merging unrelated fragments.
    correlation_failed: bool,
}

impl ChunkAccumulator {
    /// Resolves the accumulation slot for a fragment whose `index` the
    /// server left out. A non-empty `id` matches the call it belongs to
    /// (or opens a new slot); an id-less continuation fragment is only
    /// unambiguous while at most one call is pending.
    fn route_indexless(&self, fragment: &ToolCallDelta) -> Option<u32> {
        // Known limitation, accepted: a gateway that sends the first
        // fragment of a call with neither index nor id, then a later
        // fragment of the *same* call with a real id, gets two partials
        // (the idless slot fails to assemble → Protocol error — loud,
        // not silent corruption). DeepSeek always sends `index`, so
        // this only matters behind such gateways; revisit if one
        // materializes.
        let id = fragment.id.as_deref().filter(|id| !id.is_empty());
        if let Some(id) = id {
            return match self
                .tool_calls
                .iter()
                .find(|(_, call)| call.id.as_deref() == Some(id))
            {
                Some((&index, _)) => Some(index),
                None => Some(self.tool_calls.keys().next_back().map_or(0, |k| k + 1)),
            };
        }
        match self.tool_calls.len() {
            0 => Some(0),
            1 => self.tool_calls.keys().next().copied(),
            _ => None,
        }
    }

    /// Ingests one parsed chunk, returning the display deltas it
    /// produced (possibly none).
    pub(crate) fn ingest(&mut self, chunk: ChatChunk) -> Vec<ModelEvent> {
        let mut deltas = Vec::new();
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return deltas; // usage-only chunk
        };
        if let Some(reason) = choice.finish_reason {
            self.finish_reason = Some(reason);
        }
        if let Some(fragment) = choice.delta.content
            && !fragment.is_empty()
        {
            self.text.push_str(&fragment);
            deltas.push(ModelEvent::TextDelta(fragment));
        }
        if let Some(fragment) = choice.delta.reasoning_content
            && !fragment.is_empty()
        {
            self.reasoning.push_str(&fragment);
            deltas.push(ModelEvent::ReasoningDelta(fragment));
        }
        for fragment in choice.delta.tool_calls {
            let index = match fragment.index {
                Some(index) => index,
                None => match self.route_indexless(&fragment) {
                    Some(index) => index,
                    None => {
                        // Ambiguous continuation (several calls pending,
                        // no index, no id): merging into a guess would
                        // glue unrelated arguments together.
                        self.correlation_failed = true;
                        continue;
                    }
                },
            };
            let call = self.tool_calls.entry(index).or_default();
            if let Some(id) = fragment.id
                && !id.is_empty()
            {
                call.id = Some(id);
            }
            if let Some(function) = fragment.function {
                if let Some(name) = function.name
                    && !name.is_empty()
                {
                    call.name = Some(name);
                }
                if let Some(args) = function.arguments {
                    call.arguments.push_str(&args);
                }
            }
        }
        deltas
    }

    /// The raw wire `finish_reason` seen so far, for tracing (§5.1:
    /// recorded, never acted on directly).
    pub(crate) fn raw_finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    /// Assembles the authoritative [`ModelResponse`] or the classified
    /// failure, applying the §5.1 normalization table.
    ///
    /// `saw_done` is whether the `[DONE]` terminator arrived before the
    /// stream ended. `classify_in_stream` maps provider-specific
    /// `finish_reason` values that are *failures, not stop conditions*
    /// (e.g. DeepSeek `"aborted"` → [`ModelErrorKind::Transport`]) to
    /// their kind. `id_prefix` namespaces synthesized tool-call ids.
    pub(crate) fn finalize(
        self,
        saw_done: bool,
        classify_in_stream: impl Fn(&str) -> Option<ModelErrorKind>,
        id_prefix: &str,
    ) -> Result<ModelResponse, ModelError> {
        if self.correlation_failed {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "tool-call fragment without index could not be correlated to a call",
            ));
        }
        let raw_finish = self.finish_reason.as_deref();
        if let Some(kind) = raw_finish.and_then(&classify_in_stream) {
            // Classified before the early-close check so a connection
            // that dies after a failure finish_reason keeps the sharper
            // classification.
            return Err(ModelError::new(
                kind,
                format!("model stopped with failure finish_reason {raw_finish:?}"),
            ));
        }
        if !saw_done {
            return Err(ModelError::new(
                ModelErrorKind::Transport,
                "connection closed before [DONE]",
            ));
        }

        // Try to assemble every call; remember whether any failed so
        // `length` can downgrade to MaxTokens-with-discard instead of
        // Protocol (§5.1 rows 5 and 6).
        let mut calls = Vec::new();
        let mut any_bad = false;
        for partial in self.tool_calls.values() {
            match assemble_call(partial, id_prefix) {
                Ok(call) => calls.push(call),
                Err(_) => any_bad = true,
            }
        }

        let stop_reason = match raw_finish {
            Some("stop") | None => {
                if any_bad {
                    // Without `length` there is no truncation excuse:
                    // unparseable arguments are a protocol violation,
                    // never a silent EndTurn with the call dropped.
                    return Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        "tool call with unparseable or non-object arguments",
                    ));
                }
                if calls.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolCalls
                }
            }
            Some("tool_calls") => {
                if any_bad {
                    return Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        "tool call with unparseable or non-object arguments",
                    ));
                }
                if calls.is_empty() {
                    return Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        "finish_reason \"tool_calls\" with zero assembled calls",
                    ));
                }
                StopReason::ToolCalls
            }
            Some("length") => {
                // Invariant 22: calls from a truncated (length)
                // response are never executed — nor replayed (§context
                // rule 2: MaxTokens items project as plain text).
                // Truncation can land after a short complete call, so
                // clear unconditionally, not only when assembly failed.
                calls.clear();
                StopReason::MaxTokens
            }
            Some("content_filter") => {
                // Consistent with `length`: a message the model was not
                // allowed to finish must not carry calls into replay.
                calls.clear();
                StopReason::ContentFilter
            }
            Some(other) => {
                return Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    format!("unexpected finish_reason {other:?}"),
                ));
            }
        };

        let message = AssistantMessage {
            content: self.text,
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(ReasoningTrace {
                    text: self.reasoning,
                    provider_data: None,
                })
            },
            tool_calls: calls,
            stop_reason,
        };
        let usage = match self.usage.map(|wire| Usage {
            input_tokens: wire.prompt_tokens.unwrap_or(0),
            output_tokens: wire.completion_tokens.unwrap_or(0),
            cached_input_tokens: wire.prompt_cache_hit_tokens,
        }) {
            Some(usage) => usage,
            None => {
                // DeepSeek documents usage on every final chunk
                // (verified live); a missing block means a proxy
                // stripped it. Totals would silently undercount —
                // make that visible.
                tracing::warn!(
                    "model response carried no usage block; \
                     token counts recorded as 0"
                );
                Usage::default()
            }
        };
        Ok(ModelResponse { message, usage })
    }
}

/// Namespaces synthesized ids to the current process. A bare sequence
/// repeats (`deepseek-call-0`) after a restart, which would collide
/// inside a resumed, persisted transcript; uniqueness is a transcript
/// invariant ([`ToolCallId`] docs: "stable, unique"), not just a
/// per-process one. The nonce is wall-clock nanoseconds at first
/// synthesis.
static SYNTHESIZED_CALL_NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
static SYNTHESIZED_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Assembles one accumulated call, synthesizing an id when the provider
/// sent none (`ToolCallId` requires non-empty; synthesis is the
/// adapter's documented duty).
fn assemble_call(partial: &PartialToolCall, id_prefix: &str) -> Result<ToolCall, ModelErrorKind> {
    let id = match &partial.id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            let nonce = *SYNTHESIZED_CALL_NONCE.get_or_init(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
            });
            let seq = SYNTHESIZED_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("{id_prefix}-call-{nonce:x}-{seq}")
        }
    };
    let name = partial.name.as_deref().ok_or(ModelErrorKind::Protocol)?;
    // An absent `arguments` (empty accumulated string) means the model
    // requested no arguments: send `{}`, not a parse error.
    let arguments_value = if partial.arguments.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&partial.arguments).map_err(|_| ModelErrorKind::Protocol)?
    };
    let arguments =
        ToolArguments::try_from(arguments_value).map_err(|_| ModelErrorKind::Protocol)?;
    let id = ToolCallId::try_from(id).map_err(|_| ModelErrorKind::Protocol)?;
    Ok(ToolCall {
        id,
        name: name.to_owned(),
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::model::ModelCapabilities;
    use loom_core::tool::ToolSpec;

    fn no_in_stream(_: &str) -> Option<ModelErrorKind> {
        None
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "deepseek-flash".into(),
            messages: vec![Message::User { text: "hi".into() }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    fn wire_options() -> WireOptions {
        WireOptions {
            thinking: json!({"type": "enabled"}),
            reasoning_effort: "high".into(),
        }
    }

    #[test]
    fn body_shape_minimal_request() {
        let body = build_request_body(&request(), &wire_options());
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn body_carries_tools_and_auto_choice_only_with_tools() {
        let body = build_request_body(
            &ModelRequest {
                tools: vec![ToolSpec {
                    name: "read_file".into(),
                    description: "reads".into(),
                    parameters: json!({"type": "object"}),
                }],
                max_tokens: Some(1024),
                temperature: Some(0.2),
                ..request()
            },
            &wire_options(),
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["max_tokens"], 1024);
        // f32 0.2 does not survive JSON as an exact decimal; compare
        // through the f32 the request carried.
        assert_eq!(body["temperature"].as_f64().map(|x| x as f32), Some(0.2f32));
    }

    #[test]
    fn body_replays_reasoning_and_tool_results() {
        let call = ToolCall {
            id: ToolCallId::try_from("call_1".to_owned()).unwrap(),
            name: "read_file".into(),
            arguments: ToolArguments::try_from(json!({"path": "x.rs"})).unwrap(),
        };
        let body = build_request_body(
            &ModelRequest {
                messages: vec![
                    Message::System { text: "sys".into() },
                    Message::Assistant(AssistantMessage {
                        content: "reading".into(),
                        reasoning: Some(ReasoningTrace {
                            text: "  think … ".into(),
                            provider_data: None,
                        }),
                        tool_calls: vec![call],
                        stop_reason: StopReason::ToolCalls,
                    }),
                    Message::ToolResult(ToolResult {
                        call_id: ToolCallId::try_from("call_1".to_owned()).unwrap(),
                        status: loom_core::transcript::ToolStatus::Success,
                        content: "file body".into(),
                        truncated: false,
                    }),
                ],
                ..request()
            },
            &wire_options(),
        );
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "  think … ");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"x.rs"}"#
        );
        let tool = &body["messages"][2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "file body");
    }

    #[test]
    fn capabilities_used_from_core() {
        // Guards the direction of the dependency: the wire module builds
        // neutral types only.
        let caps = ModelCapabilities::new(1_048_576).with_reasoning(true);
        assert!(caps.reasoning);
    }

    fn chunk_with(delta: ChunkDelta) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta,
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn text_and_call_fragments() -> ChunkAccumulator {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some(r#"{"pa"#.into()),
                }),
            }],
            ..Default::default()
        }));
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: None,
                function: Some(ToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"th":"x.rs"}"#.into()),
                }),
            }],
            ..Default::default()
        }));
        acc
    }

    fn finished(mut acc: ChunkAccumulator, reason: &str) -> ChunkAccumulator {
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: Some(reason.into()),
            }],
            usage: Some(WireUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                prompt_cache_hit_tokens: Some(4),
            }),
        });
        acc
    }

    #[test]
    fn stop_with_calls_normalizes_to_tool_calls() {
        let acc = finished(text_and_call_fragments(), "stop");
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].name, "read_file");
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: Some(4),
            }
        );
    }

    #[test]
    fn done_without_finish_reason_normalizes_structurally() {
        let mut bare = text_and_call_fragments();
        bare.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: None,
            }],
            usage: None,
        });
        let response = bare.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    }

    #[test]
    fn done_without_finish_reason_or_calls_is_end_turn() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            content: Some("hello".into()),
            ..Default::default()
        }));
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::EndTurn);
        assert_eq!(response.message.content, "hello");
    }

    #[test]
    fn early_close_is_transport() {
        let acc = text_and_call_fragments();
        let error = acc.finalize(false, no_in_stream, "test").unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Transport);
        assert!(error.is_retryable());
    }

    #[test]
    fn tool_calls_with_zero_calls_is_protocol() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        });
        let error = acc.finalize(true, no_in_stream, "test").unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Protocol);
        assert!(!error.is_retryable());
    }

    #[test]
    fn length_with_partial_args_discards_calls() {
        let mut acc = text_and_call_fragments();
        // Corrupt the assembled arguments: second fragment missing.
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some(r#"{"path": "x.rs"#.into()), // truncated JSON
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "length");
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
        assert!(response.message.tool_calls.is_empty(), "calls discarded");
    }

    #[test]
    fn length_with_complete_call_still_discards() {
        // Truncation can land after a short, fully parseable call
        // (e.g. `{}`); Invariant 22 still forbids executing or
        // replaying it.
        let acc = finished(text_and_call_fragments(), "length");
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
        assert!(
            response.message.tool_calls.is_empty(),
            "complete calls must be discarded too"
        );
    }

    #[test]
    fn unparseable_args_is_protocol() {
        let mut acc = text_and_call_fragments();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some(r#"{"path": "#.into()), // truncated JSON
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "stop");
        let error = acc.finalize(true, no_in_stream, "test").unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Protocol);
    }

    #[test]
    fn in_stream_failure_classified_before_early_close() {
        let mut acc = text_and_call_fragments();
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: Some("aborted".into()),
            }],
            usage: None,
        });
        let error = acc
            .finalize(
                false,
                |reason| match reason {
                    "aborted" => Some(ModelErrorKind::Transport),
                    _ => None,
                },
                "test",
            )
            .unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Transport);
    }

    #[test]
    fn synthesized_id_when_provider_sent_none() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: None,
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_dir".into()),
                    arguments: Some("{}".into()),
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        let response = acc.finalize(true, no_in_stream, "deepseek").unwrap();
        assert!(
            response.message.tool_calls[0]
                .id
                .as_str()
                .starts_with("deepseek-call-"),
            "got {}",
            response.message.tool_calls[0].id.as_str()
        );

        // Uniqueness is a transcript invariant, so a second message with
        // no ids must not reuse the first message's synthesized id.
        let second = {
            let mut acc = ChunkAccumulator::default();
            acc.ingest(chunk_with(ChunkDelta {
                tool_calls: vec![ToolCallDelta {
                    index: Some(0),
                    id: None,
                    function: Some(ToolCallFunctionDelta {
                        name: Some("list_dir".into()),
                        arguments: Some("{}".into()),
                    }),
                }],
                ..Default::default()
            }));
            let acc = finished(acc, "tool_calls");
            acc.finalize(true, no_in_stream, "deepseek").unwrap()
        };
        assert_ne!(
            response.message.tool_calls[0].id, second.message.tool_calls[0].id,
            "synthesized ids must be unique across messages"
        );
    }

    #[test]
    fn indexless_parallel_calls_routed_by_id() {
        // A gateway that omits `index` but sends `id`s must not merge
        // parallel calls into one slot (gluing their arguments).
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![
                ToolCallDelta {
                    index: None,
                    id: Some("a".into()),
                    function: Some(ToolCallFunctionDelta {
                        name: Some("read_file".into()),
                        arguments: Some(r#"{"path":"x"}"#.into()),
                    }),
                },
                ToolCallDelta {
                    index: None,
                    id: Some("b".into()),
                    function: Some(ToolCallFunctionDelta {
                        name: Some("list_dir".into()),
                        arguments: Some("{}".into()),
                    }),
                },
            ],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        let response = acc.finalize(true, no_in_stream, "deepseek").unwrap();
        let calls = &response.message.tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id.as_str(), "a");
        assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x"}"#);
        assert_eq!(calls[1].id.as_str(), "b");
        assert_eq!(calls[1].arguments.to_string(), "{}");
    }

    #[test]
    fn indexless_continuation_appends_to_sole_call() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: None,
                id: Some("a".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some(r#"{"pa"#.into()),
                }),
            }],
            ..Default::default()
        }));
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: None,
                id: None,
                function: Some(ToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"th":"x"}"#.into()),
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        let response = acc.finalize(true, no_in_stream, "deepseek").unwrap();
        let calls = &response.message.tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "a");
        assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x"}"#);
    }

    #[test]
    fn ambiguous_indexless_continuation_is_protocol_error() {
        // With several calls pending, an id-less index-less fragment
        // cannot be correlated; silently merging it could corrupt
        // arguments, so finalize must fail loudly.
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![
                ToolCallDelta {
                    index: None,
                    id: Some("a".into()),
                    function: Some(ToolCallFunctionDelta {
                        name: Some("read_file".into()),
                        arguments: Some("{}".into()),
                    }),
                },
                ToolCallDelta {
                    index: None,
                    id: Some("b".into()),
                    function: Some(ToolCallFunctionDelta {
                        name: Some("list_dir".into()),
                        arguments: Some("{}".into()),
                    }),
                },
            ],
            ..Default::default()
        }));
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: None,
                id: None,
                function: Some(ToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"{"glue"}"#.into()),
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        let error = acc.finalize(true, no_in_stream, "deepseek").unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Protocol);
    }

    #[test]
    fn empty_arguments_string_becomes_empty_object() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_dir".into()),
                    arguments: Some(String::new()),
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.tool_calls[0].arguments.to_string(), "{}");
    }

    #[test]
    fn usage_only_chunk_produces_no_deltas_but_sets_usage() {
        let mut acc = ChunkAccumulator::default();
        let deltas = acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(1),
                prompt_cache_hit_tokens: None,
            }),
        });
        assert!(deltas.is_empty());
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.usage.cached_input_tokens, None);
    }

    #[test]
    fn null_list_fields_parse_as_default() {
        // Some OpenAI-compatible servers send explicit nulls; the chunk
        // must still parse instead of failing the whole stream.
        let chunk: ChatChunk = serde_json::from_str(r#"{"choices":null,"usage":null}"#).unwrap();
        assert!(chunk.choices.is_empty());
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[{"delta":null,"finish_reason":null}],"usage":null}"#,
        )
        .unwrap();
        assert_eq!(chunk.choices.len(), 1);
        let delta: ChunkDelta = serde_json::from_str(r#"{"tool_calls":null}"#).unwrap();
        assert!(delta.tool_calls.is_empty());
    }

    #[test]
    fn content_filter_discards_tool_calls() {
        let acc = finished(text_and_call_fragments(), "content_filter");
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ContentFilter);
        assert!(
            response.message.tool_calls.is_empty(),
            "calls must not survive into a filtered message"
        );
    }

    #[test]
    fn content_filter_maps_through() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: Some("content_filter".into()),
            }],
            usage: None,
        });
        let response = acc.finalize(true, no_in_stream, "test").unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ContentFilter);
    }

    #[test]
    fn unknown_finish_reason_is_protocol() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: Some("quantum_decay".into()),
            }],
            usage: None,
        });
        let error = acc.finalize(true, no_in_stream, "test").unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Protocol);
    }
}
