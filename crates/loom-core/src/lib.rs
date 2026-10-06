//! Loom's umbrella facade: the single crate a frontend imports (ADR-021).
//!
//! A frontend — CLI, TUI, GUI, or server — depends on `loom-core` and nothing
//! else from this workspace. The facade re-exports the capability crates and
//! owns no domain logic of its own:
//!
//! - [`engine`] — the composition root and control plane: credential
//!   resolution, provider and tool wiring, session lifecycle and durability,
//!   and the driving API.
//! - [`kernel`] — the neutral agent loop and domain types, free of provider,
//!   HTTP, keychain, and SQLite dependencies (ADR-001).
//! - [`providers`] — concrete model providers.
//! - [`tools`] — the built-in tool implementations.
//!
//! Bindings are in-process: a frontend links this crate and calls the engine
//! directly, receiving events through [`kernel::event::EventSink`] with no IPC
//! and no serialization on the hot path. The serializable event protocol is
//! retained only as the optional out-of-process seam for a future server.

pub use loom_engine as engine;
pub use loom_kernel as kernel;
pub use loom_providers as providers;
pub use loom_tools as tools;

// The driving API, surfaced at the facade root so the common path is
// `loom_core::Engine`.
pub use loom_engine::{Engine, EngineConfig, EngineError, ReasoningEffort};
