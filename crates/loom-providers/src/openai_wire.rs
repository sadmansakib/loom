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

use loom_kernel::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelRequest, ModelResponse, StopReason, Usage,
};
use loom_kernel::transcript::{
    AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The SSE terminator every OpenAI-compatible chat API sends.
pub(crate) const DONE_MARKER: &str = "[DONE]";

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
    /// A mid-stream failure object. OpenRouter (and some gateways) send
    /// `{"error": {...}}` as a chunk instead of an HTTP error status,
    /// then end the stream; without this the failure would surface as a
    /// generic early-close transport error.
    #[serde(default)]
    pub error: Option<WireError>,
}

/// A mid-stream error object.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WireError {
    /// Human-readable message, best effort.
    #[serde(default)]
    pub message: Option<String>,
    /// Provider error code: a number (HTTP status) or a string (business
    /// code). Kept raw; the adapter's classifier decides the kind.
    #[serde(default)]
    pub code: Option<Value>,
}

impl WireError {
    /// The code as an HTTP status number, when the provider sent one.
    pub(crate) fn status(&self) -> Option<u16> {
        match &self.code {
            Some(Value::Number(number)) => number.as_u64().and_then(|n| u16::try_from(n).ok()),
            Some(Value::String(text)) => text.parse().ok(),
            _ => None,
        }
    }

    /// Best-effort message for logs, bounded to the classification excerpt
    /// size so a hostile server cannot balloon memory.
    pub(crate) fn excerpt(&self) -> String {
        self.message
            .as_deref()
            .unwrap_or("(no message)")
            .chars()
            .take(500)
            .collect()
    }
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
///
/// Only the OpenAI-standard shapes live here; every key they do not
/// claim lands in [`WireUsage::extra`], where each adapter reads its
/// own dialect (see [`WireEndpoint::dialect_usage`](crate::wire_stream::WireEndpoint::dialect_usage)).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// OpenAI-standard cache reporting (`prompt_tokens_details.cached_tokens`).
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptDetails>,
    /// `completion_tokens_details` when present; OpenAI-family
    /// providers report the reasoning share of completion tokens here.
    #[serde(default)]
    pub completion_tokens_details: Option<WireCompletionDetails>,
    /// Every key the standard shapes above do not claim. Adapter
    /// dialects (DeepSeek's `prompt_cache_hit_tokens`, OpenRouter's
    /// `cost`) are read from here, in their own files.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Provider-dialect additions to a [`WireUsage`], extracted by each
/// adapter from the keys the standard shapes do not claim.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DialectUsage {
    /// The provider's own name for the cache-hit count (DeepSeek's
    /// `prompt_cache_hit_tokens`). Wins over the generic
    /// `prompt_tokens_details.cached_tokens` when both appear.
    pub cached_input_tokens: Option<u64>,
    /// The provider-reported charge for this request in USD (OpenRouter
    /// reports `0.0` for free models and the actual spend for paid ones).
    pub cost_usd: Option<f64>,
}

/// The `completion_tokens_details` sub-object of a `usage` block.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WireCompletionDetails {
    /// The reasoning/thinking share of `completion_tokens` (already
    /// counted there — an attribution split, not an addition).
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

/// The `prompt_tokens_details` sub-object of a `usage` block.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WirePromptDetails {
    /// The cached share of `prompt_tokens` (already counted there —
    /// an attribution split, not an addition).
    #[serde(default)]
    pub cached_tokens: Option<u64>,
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
    /// First mid-stream error chunk seen; a stream carrying one is a
    /// failure regardless of what else arrived.
    wire_error: Option<WireError>,
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
        if let Some(error) = chunk.error
            && self.wire_error.is_none()
        {
            self.wire_error = Some(error);
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
    /// stream ended; `policy` carries the adapter's classification hooks.
    pub(crate) fn finalize(
        self,
        saw_done: bool,
        policy: &FinalizePolicy<'_>,
    ) -> Result<ModelResponse, ModelError> {
        if self.correlation_failed {
            return Err(ModelError::new(
                ModelErrorKind::Protocol,
                "tool-call fragment without index could not be correlated to a call",
            ));
        }
        if let Some(error) = &self.wire_error {
            // A mid-stream error is definitive: classify it before any
            // stop-reason interpretation (the stream may still carry a
            // misleading finish_reason, or end without [DONE]).
            let kind = (policy.classify_wire_error)(error.status(), &error.excerpt());
            return Err(ModelError::new(
                kind,
                format!("provider reported a mid-stream error: {}", error.excerpt()),
            ));
        }
        let raw_finish = self.finish_reason.as_deref();
        if let Some(kind) = raw_finish.and_then(policy.classify_in_stream) {
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
            match assemble_call(partial, policy.id_prefix) {
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
        let usage = match self.usage.map(|wire| {
            let dialect = (policy.dialect_usage)(&wire.extra);
            Usage {
                input_tokens: wire.prompt_tokens.unwrap_or(0),
                output_tokens: wire.completion_tokens.unwrap_or(0),
                cached_input_tokens: dialect.cached_input_tokens.or_else(|| {
                    wire.prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                }),
                thinking_tokens: wire
                    .completion_tokens_details
                    .and_then(|details| details.reasoning_tokens),
                cache_creation_input_tokens: None,
                cost_usd: dialect.cost_usd,
            }
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

/// Adapter-supplied classification for one completed stream.
///
/// The wire module owns accumulation and the stop-normalization table;
/// each adapter supplies its provider-specific opinions: `finish_reason`
/// values that mean failure, how to classify a mid-stream error object,
/// and the prefix for synthesized tool-call ids.
pub(crate) struct FinalizePolicy<'a> {
    /// Maps a raw `finish_reason` to a failure kind. `None` means the
    /// reason is not a failure and normal normalization applies.
    pub classify_in_stream: &'a dyn Fn(&str) -> Option<ModelErrorKind>,
    /// Classifies a mid-stream error object (message + HTTP-ish status).
    pub classify_wire_error: &'a dyn Fn(Option<u16>, &str) -> ModelErrorKind,
    /// Prefix for synthesized tool-call ids, normally the provider name.
    pub id_prefix: &'a str,
    /// Reads the adapter's dialect keys out of a usage block's
    /// non-standard fields. The wire module itself holds no dialect
    /// knowledge; each adapter's override owns its provider's names.
    pub dialect_usage: &'a dyn Fn(&serde_json::Map<String, serde_json::Value>) -> DialectUsage,
}

/// Classifies a generic mid-stream error object by its HTTP-ish status.
///
/// Shared by adapters: OpenAI-compatible servers that report errors as a
/// stream chunk (OpenRouter, several gateways) use HTTP status codes for
/// the cause. An absent or unknown code is treated as a retryable server
/// failure — the retry policy bounds it, and silently treating it as
/// permanent would fail recoverable runs.
pub(crate) fn classify_wire_error(status: Option<u16>, _message: &str) -> ModelErrorKind {
    match status {
        Some(401) => ModelErrorKind::Auth,
        Some(402) => ModelErrorKind::QuotaExhausted,
        Some(408) => ModelErrorKind::Transport,
        Some(429) => ModelErrorKind::RateLimited,
        Some(503) => ModelErrorKind::Overloaded,
        Some(code) if code >= 500 => ModelErrorKind::Server,
        _ => ModelErrorKind::Server,
    }
}

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
    use loom_kernel::model::ModelCapabilities;
    use loom_kernel::tool::ToolSpec;

    fn no_in_stream(_: &str) -> Option<ModelErrorKind> {
        None
    }

    fn policy(prefix: &str) -> FinalizePolicy<'_> {
        FinalizePolicy {
            classify_in_stream: &no_in_stream,
            classify_wire_error: &classify_wire_error,
            id_prefix: prefix,
            dialect_usage: &|_| DialectUsage::default(),
        }
    }

    /// A policy wired to DeepSeek's real dialect override, so the
    /// `prompt_cache_hit_tokens` extraction is tested through the same
    /// path the stream driver uses.
    fn deepseek_policy(prefix: &str) -> FinalizePolicy<'_> {
        FinalizePolicy {
            classify_in_stream: &no_in_stream,
            classify_wire_error: &classify_wire_error,
            id_prefix: prefix,
            dialect_usage: &<crate::deepseek::DeepSeekProvider as crate::wire_stream::WireEndpoint>::dialect_usage,
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "deepseek-flash".into(),
            messages: vec![Message::User { text: "hi".into() }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            stable_prefix_messages: None,
        }
    }

    fn wire_options() -> WireOptions {
        let mut extra_body = serde_json::Map::new();
        extra_body.insert("thinking".into(), json!({"type": "enabled"}));
        extra_body.insert("reasoning_effort".into(), json!("high"));
        WireOptions { extra_body }
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
                stable_prefix_messages: None,
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
                    Message::ToolResult(ToolResult::new(
                        ToolCallId::try_from("call_1".to_owned()).unwrap(),
                        loom_kernel::transcript::ToolStatus::Success,
                        "file body",
                        false,
                    )),
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
            error: None,
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
                prompt_tokens_details: Some(WirePromptDetails {
                    cached_tokens: Some(4),
                }),
                completion_tokens_details: None,
                extra: Default::default(),
            }),
            error: None,
        });
        acc
    }

    #[test]
    fn stop_with_calls_normalizes_to_tool_calls() {
        let acc = finished(text_and_call_fragments(), "stop");
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].name, "read_file");
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: Some(4),
                thinking_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
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
            error: None,
        });
        let response = bare.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    }

    #[test]
    fn done_without_finish_reason_or_calls_is_end_turn() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            content: Some("hello".into()),
            ..Default::default()
        }));
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.message.stop_reason, StopReason::EndTurn);
        assert_eq!(response.message.content, "hello");
    }

    #[test]
    fn early_close_is_transport() {
        let acc = text_and_call_fragments();
        let error = acc.finalize(false, &policy("test")).unwrap_err();
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
            error: None,
        });
        let error = acc.finalize(true, &policy("test")).unwrap_err();
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
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
        assert!(response.message.tool_calls.is_empty(), "calls discarded");
    }

    #[test]
    fn length_with_complete_call_still_discards() {
        // Truncation can land after a short, fully parseable call
        // (e.g. `{}`); Invariant 22 still forbids executing or
        // replaying it.
        let acc = finished(text_and_call_fragments(), "length");
        let response = acc.finalize(true, &policy("test")).unwrap();
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
        let error = acc.finalize(true, &policy("test")).unwrap_err();
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
            error: None,
        });
        fn classify(reason: &str) -> Option<ModelErrorKind> {
            match reason {
                "aborted" => Some(ModelErrorKind::Transport),
                _ => None,
            }
        }
        let error = acc
            .finalize(
                false,
                &FinalizePolicy {
                    classify_in_stream: &classify,
                    classify_wire_error: &classify_wire_error,
                    id_prefix: "test",
                    dialect_usage: &|_| DialectUsage::default(),
                },
            )
            .unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Transport);
    }

    #[test]
    fn mid_stream_error_object_is_classified() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![ChunkChoice {
                delta: ChunkDelta::default(),
                finish_reason: None,
            }],
            usage: None,
            error: Some(WireError {
                message: Some("upstream provider is overloaded".into()),
                code: Some(json!(503)),
            }),
        });
        let error = acc.finalize(true, &policy("test")).unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Overloaded);
        assert!(error.message.contains("upstream provider is overloaded"));
    }

    #[test]
    fn mid_stream_error_without_status_is_retryable_server() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: None,
            error: Some(WireError {
                message: Some("the selected model is unavailable".into()),
                code: None,
            }),
        });
        let error = acc.finalize(true, &policy("test")).unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Server);
        assert!(error.is_retryable());
    }

    #[test]
    fn reported_cost_passes_through_usage() {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            content: Some("hello".into()),
            ..Default::default()
        }));
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(20),
                prompt_tokens_details: None,
                completion_tokens_details: None,
                extra: Default::default(),
            }),
            error: None,
        });
        // OpenRouter's `cost` key is a dialect field: the policy's
        // dialect hook supplies it, the generic wire only carries it.
        let policy = FinalizePolicy {
            classify_in_stream: &no_in_stream,
            classify_wire_error: &classify_wire_error,
            id_prefix: "openrouter",
            dialect_usage: &|_| DialectUsage {
                cost_usd: Some(0.000123),
                ..Default::default()
            },
        };
        let response = acc.finalize(true, &policy).unwrap();
        assert_eq!(response.usage.cost_usd, Some(0.000123));
        assert_eq!(response.usage.input_tokens, 100);
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
        let response = acc.finalize(true, &policy("deepseek")).unwrap();
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
            acc.finalize(true, &policy("deepseek")).unwrap()
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
        let response = acc.finalize(true, &policy("deepseek")).unwrap();
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
        let response = acc.finalize(true, &policy("deepseek")).unwrap();
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
        let error = acc.finalize(true, &policy("deepseek")).unwrap_err();
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
        let response = acc.finalize(true, &policy("test")).unwrap();
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
                prompt_tokens_details: None,
                completion_tokens_details: None,
                extra: Default::default(),
            }),
            error: None,
        });
        assert!(deltas.is_empty());
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.usage.cached_input_tokens, None);
    }

    #[test]
    fn reasoning_tokens_map_to_the_thinking_attribution() {
        // OpenAI-family `completion_tokens_details.reasoning_tokens`
        // lands in the provider-neutral `thinking_tokens` split.
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(50),
                completion_tokens: Some(20),
                prompt_tokens_details: None,
                completion_tokens_details: Some(WireCompletionDetails {
                    reasoning_tokens: Some(12),
                }),
                extra: Default::default(),
            }),
            error: None,
        });
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.usage.thinking_tokens, Some(12));
        // The split is already inside the output total.
        assert_eq!(response.usage.output_tokens, 20);
    }

    #[test]
    fn openai_dialect_cached_tokens_map_when_the_deepseek_field_is_absent() {
        // Z.ai/GLM reports cache hits via `prompt_tokens_details
        // .cached_tokens` instead of DeepSeek's `prompt_cache_hit_tokens`;
        // the DeepSeek field stays authoritative when both appear.
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(10),
                prompt_tokens_details: Some(WirePromptDetails {
                    cached_tokens: Some(40),
                }),
                completion_tokens_details: None,
                extra: Default::default(),
            }),
            error: None,
        });
        let response = acc.finalize(true, &policy("test")).unwrap();
        assert_eq!(response.usage.cached_input_tokens, Some(40));

        // DeepSeek names the same count `prompt_cache_hit_tokens`; its
        // dialect override wins when both forms appear in one block.
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(10),
                prompt_tokens_details: Some(WirePromptDetails {
                    cached_tokens: Some(40),
                }),
                completion_tokens_details: None,
                extra: {
                    let mut extra = serde_json::Map::new();
                    extra.insert(
                        "prompt_cache_hit_tokens".to_string(),
                        serde_json::Value::from(7u64),
                    );
                    extra
                },
            }),
            error: None,
        });
        let response = acc.finalize(true, &deepseek_policy("test")).unwrap();
        assert_eq!(response.usage.cached_input_tokens, Some(7));

        // With only the generic form present, DeepSeek's dialect
        // falls back to it unchanged.
        let mut acc = ChunkAccumulator::default();
        acc.ingest(ChatChunk {
            choices: vec![],
            usage: Some(WireUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(10),
                prompt_tokens_details: Some(WirePromptDetails {
                    cached_tokens: Some(40),
                }),
                completion_tokens_details: None,
                extra: Default::default(),
            }),
            error: None,
        });
        let response = acc.finalize(true, &deepseek_policy("test")).unwrap();
        assert_eq!(response.usage.cached_input_tokens, Some(40));
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
        let response = acc.finalize(true, &policy("test")).unwrap();
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
            error: None,
        });
        let response = acc.finalize(true, &policy("test")).unwrap();
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
            error: None,
        });
        let error = acc.finalize(true, &policy("test")).unwrap_err();
        assert_eq!(error.kind, ModelErrorKind::Protocol);
    }
}
