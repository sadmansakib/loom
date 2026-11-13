//! Wire chunk and usage types: the `data:` payload shapes every
//! OpenAI-compatible chat stream is parsed into.

use serde::Deserialize;
use serde_json::Value;

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
    /// Rides on the final content chunk (`DeepSeek`) when requested via
    /// `stream_options.include_usage`.
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// A mid-stream failure object. `OpenRouter` (and some gateways) send
    /// `{"error": {...}}` as a chunk instead of an HTTP error status,
    /// then end the stream; without this the failure would surface as a
    /// generic early-close transport error.
    #[serde(default)]
    pub error: Option<WireError>,
    /// Groq reports a mid-stream failure inside its own per-chunk
    /// `x_groq` envelope rather than the OpenAI-standard top-level
    /// `error` key, then ends the stream. Live-verified shape:
    /// `{"x_groq": {"id": ..., "error": {"message", "type", "code"}}}`.
    /// Read into the same [`WireError`] slot so a Groq capacity failure
    /// classifies as itself instead of a generic early close.
    #[serde(default)]
    pub x_groq: Option<GroqChunkEnvelope>,
}

/// Groq's per-chunk `x_groq` envelope (see
/// [`ChatChunk::x_groq`]). Only the failure is read — the envelope also
/// carries a request `id`, `seed`, timing, and `usage_breakdown` keys
/// that nothing in the kernel consumes.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GroqChunkEnvelope {
    /// The failure, when the chunk carries one.
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
    /// Reasoning ("thinking") fragment, DeepSeek/GLM/NIM dialect.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Reasoning fragment, Groq dialect (`delta.reasoning`); read only
    /// when [`ChunkAccumulator::reasoning_field`] names it.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Tool-call fragments; the first for an `index` carries `id` and
    /// `function.name`, later ones only append `function.arguments`.
    #[serde(default, deserialize_with = "default_on_null")]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One `delta.tool_calls[]` fragment.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToolCallDelta {
    /// Correlates the fragments of one call. `DeepSeek` (and `OpenAI`)
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
    /// dialects (`DeepSeek`'s `prompt_cache_hit_tokens`, `OpenRouter`'s
    /// `cost`) are read from here, in their own files.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Provider-dialect additions to a [`WireUsage`], extracted by each
/// adapter from the keys the standard shapes do not claim.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DialectUsage {
    /// The provider's own name for the cache-hit count (`DeepSeek`'s
    /// `prompt_cache_hit_tokens`). Wins over the generic
    /// `prompt_tokens_details.cached_tokens` when both appear.
    pub cached_input_tokens: Option<u64>,
    /// The provider-reported charge for this request in USD (`OpenRouter`
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
