//! Stream accumulation and completion: chunk ingest, tool-call
//! fragment assembly, §5.1 stop-signal normalization, and the
//! adapter-supplied classification hooks used at finalize time.

use std::collections::BTreeMap;

use loom_kernel::model::{
    ModelError, ModelErrorKind, ModelEvent, ModelResponse, StopReason, Usage,
};
use loom_kernel::transcript::{
    AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId,
};
use serde_json::Value;

use super::REASONING_FIELD_PLAIN;
use super::chunk::{ChatChunk, DialectUsage, ToolCallDelta, WireError, WireUsage};

/// One accumulated tool call under construction.
#[derive(Debug, Default)]
pub(super) struct PartialToolCall {
    pub(super) id: Option<String>,
    pub(super) name: Option<String>,
    /// Concatenated `arguments` fragments.
    pub(super) arguments: String,
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
    /// Wire field this dialect streams reasoning fragments under (see
    /// [`WireOptions::reasoning_field`]). `None` = the
    /// [`REASONING_FIELD`] default.
    reasoning_field: Option<&'static str>,
}

impl ChunkAccumulator {
    /// Selects the dialect's reasoning field. Read-side counterpart to
    /// [`WireOptions::reasoning_field`]: the write side decides which key
    /// a replayed trace goes under, this decides which delta key the same
    /// trace streams back on, so both must name the same dialect.
    pub(crate) fn with_reasoning_field(mut self, field: Option<&'static str>) -> Self {
        self.reasoning_field = field;
        self
    }
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
        let x_groq_error = chunk.x_groq.and_then(|envelope| envelope.error);
        if let Some(error) = chunk.error.or(x_groq_error)
            && self.wire_error.is_none()
        {
            self.wire_error = Some(error);
        }
        let Some(mut choice) = chunk.choices.into_iter().next() else {
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
        let reasoning_fragment = match self.reasoning_field {
            // Groq dialect: `delta.reasoning`. `reasoning_content` is
            // still accepted as a fallback so an upstream that starts
            // emitting the standard key does not silently drop traces.
            Some(REASONING_FIELD_PLAIN) => choice
                .delta
                .reasoning
                .take()
                .or_else(|| choice.delta.reasoning_content.take()),
            _ => choice.delta.reasoning_content.take(),
        };
        if let Some(fragment) = reasoning_fragment
            && !fragment.is_empty()
        {
            self.reasoning.push_str(&fragment);
            deltas.push(ModelEvent::ReasoningDelta(fragment));
        }
        for fragment in choice.delta.tool_calls {
            if !self.merge_tool_fragment(fragment) {
                self.correlation_failed = true;
            }
        }
        deltas
    }

    /// Merges one streamed [`ToolCallDelta`] into the accumulator,
    /// correlating it to a call by its `index` — or, when the provider
    /// omitted the index, by routing. Returns `false` when the fragment
    /// cannot be correlated (ambiguous continuation: several calls
    /// pending, no index, no id) — merging into a guess would glue
    /// unrelated arguments together, so the caller marks the stream and
    /// `finalize` fails it.
    fn merge_tool_fragment(&mut self, fragment: ToolCallDelta) -> bool {
        let index = match fragment.index {
            Some(index) => index,
            None => match self.route_indexless(&fragment) {
                Some(index) => index,
                None => return false,
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
        true
    }

    /// The raw wire `finish_reason` seen so far, for tracing (§5.1:
    /// recorded, never acted on directly).
    pub(crate) fn raw_finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    /// The classified terminal error for this stream, if any. A
    /// mid-stream error object is classified before any stop-reason
    /// interpretation (the stream may still carry a misleading
    /// `finish_reason`, or end without `[DONE]`); a failure `finish_reason`
    /// is classified before the early-close check so a connection that
    /// dies after one keeps the sharper classification.
    fn terminal_error(&self, saw_done: bool, policy: &FinalizePolicy<'_>) -> Option<ModelError> {
        if self.correlation_failed {
            return Some(ModelError::new(
                ModelErrorKind::Protocol,
                "tool-call fragment without index could not be correlated to a call",
            ));
        }
        if let Some(error) = &self.wire_error {
            let kind = (policy.classify_wire_error)(error.status(), &error.excerpt());
            return Some(ModelError::new(
                kind,
                format!("provider reported a mid-stream error: {}", error.excerpt()),
            ));
        }
        if let Some(kind) = self
            .finish_reason
            .as_deref()
            .and_then(policy.classify_in_stream)
        {
            return Some(ModelError::new(
                kind,
                format!(
                    "model stopped with failure finish_reason {:?}",
                    self.finish_reason
                ),
            ));
        }
        if !saw_done {
            return Some(ModelError::new(
                ModelErrorKind::Transport,
                "connection closed before [DONE]",
            ));
        }
        None
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
        if let Some(error) = self.terminal_error(saw_done, policy) {
            return Err(error);
        }

        // Assemble every call. Unparseable or non-object arguments are
        // salvaged into a placeholder (see `assemble_call`) instead of
        // failing the response: the engine reports them back to the
        // model as tool errors, which it can correct, rather than
        // killing the run with a non-retryable Protocol error.
        let mut calls = Vec::new();
        for partial in self.tool_calls.into_values() {
            calls.push(assemble_call(partial, policy.id_prefix));
        }

        // Duplicate call ids (a provider reusing one id across two
        // calls, or a model-sent id colliding with a synthesized one)
        // would make every id-keyed surface ambiguous — the transcript
        // pairs tool results with calls by id. Refuse loudly instead
        // of dispatching both; the model can reissue the calls.
        {
            let mut seen = std::collections::HashSet::new();
            for call in &calls {
                if !seen.insert(call.id.as_str()) {
                    return Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        format!(
                            "provider sent the same tool-call id `{}` for more than one call",
                            call.id.as_str()
                        ),
                    ));
                }
            }
        }

        let stop_reason = normalized_stop(self.finish_reason.as_deref(), &mut calls)?;

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
        let usage = if let Some(usage) = self.usage.map(|wire| {
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
            usage
        } else {
            // DeepSeek documents usage on every final chunk
            // (verified live); a missing block means a proxy
            // stripped it. Totals would silently undercount —
            // make that visible.
            tracing::warn!(
                "model response carried no usage block; \
                 token counts recorded as 0"
            );
            Usage::default()
        };
        Ok(ModelResponse { message, usage })
    }
}

/// The §5.1 stop-reason normalization table for a completed stream.
///
/// Maps the raw wire `finish_reason` to [`StopReason`], discarding
/// calls when the message was truncated or filtered (Invariant 22:
/// calls from a truncated response are never executed — nor replayed;
/// truncation can land after a short complete call, so they clear
/// unconditionally, not only when assembly failed). Unparseable
/// arguments no longer fail here — the call is salvaged upstream and
/// the engine reports it back to the model as a tool error.
fn normalized_stop(
    raw_finish: Option<&str>,
    calls: &mut Vec<ToolCall>,
) -> Result<StopReason, ModelError> {
    match raw_finish {
        Some("stop") | None => Ok(if calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolCalls
        }),
        Some("tool_calls") => {
            if calls.is_empty() {
                return Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "finish_reason \"tool_calls\" with zero assembled calls",
                ));
            }
            Ok(StopReason::ToolCalls)
        }
        Some("length") => {
            calls.clear();
            Ok(StopReason::MaxTokens)
        }
        Some("content_filter") => {
            // Consistent with `length`: a message the model was not
            // allowed to finish must not carry calls into replay.
            calls.clear();
            Ok(StopReason::ContentFilter)
        }
        // Failure-type reasons never reach here (the dialect hook
        // classifies them before normalization), but vendors keep
        // minting new benign reasons ("eos", "end_of_turn",
        // "max_output_tokens", …). Killing the run on an unknown word
        // turns a forward-compatible stream into a hard protocol
        // error, so classify structurally — like a missing
        // finish_reason — and keep the name in the log.
        Some(other) => {
            tracing::warn!(
                reason = other,
                "unknown finish_reason; classifying structurally from assembled content"
            );
            Ok(if calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolCalls
            })
        }
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
/// stream chunk (`OpenRouter`, several gateways) use HTTP status codes for
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
        // Other 4xx client errors are permanent: the request shape,
        // scope, or route was rejected, and retrying re-sends the same
        // rejected request (double-billing a response that already
        // streamed).
        Some(400..=499) => ModelErrorKind::InvalidRequest,
        // Absent code, 1xx/3xx, anything odd: an unattributed failure
        // is better retried within bounds than failed permanently.
        _ => ModelErrorKind::Server,
    }
}

/// Assembles one accumulated call, synthesizing an id when the provider
/// sent none (`ToolCallId` requires non-empty; synthesis is the
/// adapter's documented duty). Arguments that fail to parse, or parse
/// to a non-object, are salvaged into a placeholder carrying the raw
/// text under [`MALFORMED_ARGUMENTS_KEY`]; the engine refuses to
/// dispatch those calls and reports the error back as the tool result,
/// so the model can reissue them. Failing the whole response here
/// would kill the run over a recoverable slip.
// The `expect`s below are proven invariants, not fallible parsing: an
// empty JSON object is always a valid `ToolArguments`, and the id is
// either echoed non-empty by the model or synthesized above.
#[allow(clippy::expect_used)]
pub(super) fn assemble_call(partial: PartialToolCall, id_prefix: &str) -> ToolCall {
    let id = match partial.id {
        Some(id) if !id.is_empty() => id,
        _ => {
            let nonce = *SYNTHESIZED_CALL_NONCE.get_or_init(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            });
            let seq = SYNTHESIZED_CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("{id_prefix}-call-{nonce:x}-{seq}")
        }
    };
    let name = partial.name.unwrap_or_else(|| "unknown".to_owned());
    // An absent `arguments` (empty accumulated string) means the model
    // requested no arguments: send `{}`, not a parse error.
    let arguments = if partial.arguments.trim().is_empty() {
        ToolArguments::try_from(Value::Object(serde_json::Map::new()))
            .expect("the empty object is always valid ToolArguments")
    } else {
        match serde_json::from_str::<Value>(&partial.arguments) {
            // Some models double-encode the object as a JSON *string*
            // (`"{\"path\":\"x\"}"`). Unwrap it: the inner object is
            // what the model meant, and rejecting it would raise a
            // non-retryable `Protocol` error that costs the whole run.
            Ok(Value::String(inner)) => {
                let unwrapped = serde_json::from_str(&inner).unwrap_or(Value::String(inner));
                ToolArguments::try_from(unwrapped)
                    .unwrap_or_else(|_| ToolArguments::malformed(&partial.arguments))
            }
            // An explicit `null` means the same as an absent string.
            Ok(Value::Null) => ToolArguments::try_from(Value::Object(serde_json::Map::new()))
                .expect("the empty object is always valid ToolArguments"),
            Ok(value) => ToolArguments::try_from(value)
                .unwrap_or_else(|_| ToolArguments::malformed(&partial.arguments)),
            Err(_) => ToolArguments::malformed(&partial.arguments),
        }
    };
    let id = ToolCallId::try_from(id).expect("synthesized or echoed ids are non-empty");
    ToolCall {
        id,
        name,
        arguments,
    }
}
