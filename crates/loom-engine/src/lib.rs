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
//! - **The model-catalog policy** ([`catalog`]) — fetch, cache beside the
//!   database, and fall back to a stale cache, driven by
//!   [`Engine::open_async`].
//! - **Composition** ([`Engine::open`]) — one concrete provider, the built-in
//!   tool registry, and the durable session store, assembled through the
//!   `loom-kernel` traits.
//! - **Session lifecycle and durability** ([`Engine::new_session`],
//!   [`Engine::load_session`]) — transcripts persist in the frontend-chosen
//!   database (ADR-008).
//! - **The driving API** ([`Engine::run_task`]) — one task in, events out
//!   through an in-process [`loom_kernel::event::EventSink`], a terminal
//!   result returned as data.
//! - **The trace data contract** ([`trace`], [`trace_stats`]) — the
//!   on-disk trace format (`events.ndjson` / `manifest.json` /
//!   `transcript.json`) every frontend must write identically, and the
//!   pure aggregation that reads it back for attribution.
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
pub mod catalog;
pub mod engine;
mod estimate;
mod provider;
pub mod repo;
pub mod trace;
pub mod trace_stats;

pub use engine::{Engine, EngineConfig, EngineError, default_system_prompt};
pub use estimate::{
    ANTHROPIC_TOKEN_SCALE_PER_MILLE, AnthropicEstimator, O200kEstimator, estimator_for,
};
// The provider/effort types a frontend must name to build an `EngineConfig`;
// re-exported so a frontend never depends on the provider crate directly.
pub use loom_providers::{ProviderCatalog, ReasoningEffort};
