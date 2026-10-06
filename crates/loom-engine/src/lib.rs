//! Loom engine: the composition root and control plane every frontend shares.
//!
//! This crate is the boundary ADR-021 puts between the domain and any user
//! interface. It owns exactly the wiring that is not domain logic and not
//! presentation:
//!
//! - **Configuration** ([`EngineConfig`]) — provider, model, workspace,
//!   reasoning effort, turn ceiling, and whether sessions persist.
//! - **Credential resolution** ([`auth`]) — the SQLite account store, the OS
//!   keychain backend, and ADR-009 resolution order.
//! - **Composition** ([`Engine::open`]) — one concrete provider, the built-in
//!   tool registry, and the durable session store, assembled through the
//!   `loom-kernel` traits.
//! - **Session lifecycle and durability** ([`Engine::new_session`],
//!   [`Engine::load_session`]) — transcripts persist in `loom.db` (ADR-008).
//! - **The driving API** ([`Engine::run_task`]) — one task in, events out
//!   through an in-process [`loom_kernel::event::EventSink`], a terminal
//!   result returned as data.
//!
//! A frontend links this crate and calls it directly: there is no IPC and no
//! serialization on the hot path, so a TUI, GUI, or server reuses the same
//! engine the CLI runs. Rendering, argument parsing, exit codes, and prompts
//! stay in the frontend.
//!
//! This crate must not print to stdout/stderr or read the terminal: those are
//! frontend responsibilities. It may read the process environment for
//! credentials (ADR-009) and uses `tracing` for diagnostics.

pub mod auth;
pub mod engine;
pub mod paths;
pub mod repo;

pub use engine::{Engine, EngineConfig, EngineError};
// The provider/effort types a frontend must name to build an `EngineConfig`;
// re-exported so a frontend never depends on the provider crate directly.
pub use loom_providers::ReasoningEffort;
