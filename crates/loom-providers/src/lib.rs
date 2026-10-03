//! Model provider adapters for Loom.
//!
//! Each adapter implements `loom_core::model::ModelProvider` for one wire
//! protocol. Iteration 1 ships the GLM adapter (OpenAI-compatible chat
//! completions with streamed reasoning). Adapters own transport concerns —
//! SSE decoding, timeout enforcement, drop-to-cancel, and normalization of
//! wire stop signals into `loom_core` semantics — so that no provider
//! specifics leak into the core.
