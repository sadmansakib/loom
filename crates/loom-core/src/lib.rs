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
//! - [`store`] — the durable-state implementation (SQLite) behind the
//!   kernel's [`kernel::store`] seams, for frontends that read state the
//!   engine does not surface (e.g. a transcript-analytics listing).
//!
//! The facade also owns [`paths`] — the *suggested* per-user data locations
//! (`LOOM_DATA_DIR` → XDG → `~/.local/share`). Resolving them is a frontend
//! concern, not engine policy (ADR-008: the engine never reads the
//! environment), but every frontend must resolve them identically or each
//! finds a different `loom.db`; frontends adopt the helper instead of
//! copy-pasting the ordering.
//!
//! Bindings are in-process: a frontend links this crate and calls the engine
//! directly, receiving events through [`kernel::event::EventSink`] with no IPC
//! and no serialization on the hot path. The serializable event protocol is
//! retained only as the optional out-of-process seam for a future server.

pub mod paths;

pub use loom_engine as engine;
pub use loom_kernel as kernel;
pub use loom_providers as providers;
pub use loom_store as store;
pub use loom_tools as tools;

// The driving API, surfaced at the facade root so the common path is
// `loom_core::Engine`. Driving-API types are imported from the root, not
// through the `engine` alias; module-scoped APIs stay behind the aliases
// (`loom_core::kernel::…`, `loom_core::engine::auth::…`).
pub use loom_engine::{Engine, EngineConfig, EngineError, ReasoningEffort};
