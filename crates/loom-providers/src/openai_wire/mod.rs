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

mod accumulate;
mod chunk;
mod request;
#[cfg(test)]
mod tests;

pub(crate) use accumulate::{ChunkAccumulator, FinalizePolicy, classify_wire_error};
pub(crate) use chunk::{ChatChunk, DialectUsage};
pub(crate) use request::{WireOptions, build_request_body};

/// The SSE terminator every OpenAI-compatible chat API sends.
pub(crate) const DONE_MARKER: &str = "[DONE]";

/// Reasoning ("thinking") field name on the DeepSeek/GLM/NIM dialect:
/// streamed as `delta.reasoning_content`, replayed on the assistant
/// message under the same key. This is the default because those three
/// adapters are the majority of the shipped surface.
pub(crate) const REASONING_FIELD: &str = "reasoning_content";

/// Reasoning field name on Groq's dialect: the trace streams as
/// `delta.reasoning` and must be replayed as `reasoning` too — Groq
/// *rejects* `reasoning_content` on an assistant message with HTTP 400
/// (`property 'reasoning_content' is unsupported`, live-verified
/// 2026-10-18), so replay cannot merely omit the field.
pub(crate) const REASONING_FIELD_PLAIN: &str = "reasoning";
