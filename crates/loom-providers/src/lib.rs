//! Model provider adapters for Loom.
//!
//! Each adapter implements `loom_core::model::ModelProvider` for one wire
//! protocol. Iteration 1 ships the DeepSeek adapter: an OpenAI-compatible
//! chat-completions wire module (with streamed `reasoning_content`) plus a
//! DeepSeek error classifier and per-model capability table; later
//! OpenAI-compatible providers reuse the wire module with their own
//! classifier and table. Adapters own transport concerns — SSE decoding,
//! timeout enforcement, drop-to-cancel, and normalization of wire stop
//! signals into `loom_core` semantics — so that no provider specifics leak
//! into the core.
