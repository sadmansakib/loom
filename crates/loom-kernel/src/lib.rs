//! Loom core: domain model and deterministic orchestration.
//!
//! This crate owns sessions, the append-only transcript, the provider-neutral
//! model and tool abstractions, the context compiler boundary, and the agent
//! turn loop. It must not depend on any concrete provider or tool crate
//! (ADR-001); provider- and tool-specific behavior lives behind the
//! `ModelProvider` and `Tool` traits (linked once those modules land).

pub mod agent;
pub mod context;
pub mod event;
pub mod id;
pub mod model;
pub mod session;
pub mod store;
pub mod tool;
pub mod transcript;
