//! Model provider adapters for Loom.
//!
//! Each adapter implements `loom_kernel::model::ModelProvider` for one wire
//! protocol. Iteration 1 ships the DeepSeek adapter: an OpenAI-compatible
//! chat-completions wire module (with streamed `reasoning_content`) plus a
//! DeepSeek error classifier and per-model capability table; later
//! OpenAI-compatible providers reuse the wire module with their own
//! classifier and table. Adapters own transport concerns — SSE decoding,
//! timeout enforcement, drop-to-cancel, and normalization of wire stop
//! signals into `loom_kernel` semantics — so that no provider specifics leak
//! into the core.

mod deepseek;
mod openai_wire;
mod secret;
mod sse;

pub use deepseek::{
    DeepSeekConfig, DeepSeekProvider, ParseReasoningEffortError, ReasoningEffort, ThinkingMode,
    TimeoutConfig,
};
pub use secret::SecretString;
