//! The driving API: open an engine, run one task, persist sessions.
//!
//! [`Engine::open`] resolves credentials, builds the concrete provider, and
//! assembles the built-in tools into one [`Agent`]; the frontend then calls
//! [`Engine::run_task`] with an in-process event sink and a cancellation
//! token. The event protocol never crosses a wire: the sink is a Rust trait,
//! so local frontends pay no serialization cost (ADR-021).

mod assembly;
mod budgets;
mod config;
mod error;
mod open;
mod overrides;
mod run;
mod session;
#[cfg(test)]
mod tests;

pub use assembly::default_system_prompt;
pub(crate) use assembly::replay_registry;
pub use budgets::{
    BUDGET_ENV_VARS, PRUNE_ENV_VARS, TURN_BUDGET_ENV_VARS, budgets_with_env, prune_with_env,
    turn_budget_with_env,
};
pub use config::EngineConfig;
#[cfg(test)]
pub(crate) use config::test_config;
pub use error::EngineError;
pub use overrides::{EngineOverrides, ProviderHook};

use std::sync::Arc;

use loom_kernel::agent::{Agent, TaskError, TaskSummary};
use loom_kernel::session::Session;
use loom_kernel::store::SharedEventStore;
use loom_tools::BackgroundJobs;

/// An opened engine: a composed [`Agent`] plus the session policy.
///
/// A frontend holds one engine and drives many tasks; it never sees the
/// provider, the credential, or the tool registry again.
pub struct Engine {
    agent: Agent,
    /// Background-command registry (roadmap item 13): present only
    /// when `EngineConfig::background_commands` opted in and the
    /// loop is not the bare baseline. Holds every detached job for
    /// the engine's lifetime — jobs outlive single tasks; the last
    /// registry drop kills whatever still runs (the same group
    /// reaping a foreground timeout performs).
    background: Option<BackgroundJobs>,
    /// Live LSP handles from the attachment, kept for the fast-path
    /// rename fallback (item 34): a semantic rename resolves the
    /// collision class the blind whole-word rename must decline.
    lsp_servers: Vec<Arc<loom_tools::lsp::LspServer>>,
    /// Routing slice zero (roadmap item 9): the Easy-tier agent,
    /// built at open when `EngineConfig::routing` wires one. Same
    /// provider, registry (scout registration included), and agent
    /// config as `agent` — only the model differs.
    routing_easy: Option<RoutedAgent>,
    /// The Hard-tier agent, same construction as `routing_easy`.
    /// Opt-in: a config without `hard` never routes up.
    routing_hard: Option<RoutedAgent>,
    /// Escalation-by-fork (roadmap item 36): built at open when
    /// `EngineConfig::stall_escalation_model` names a model — same
    /// registry, system prompt, and effort schedule as `agent`.
    stall_escalation: Option<RoutedAgent>,
    /// The session database path, kept so escalation can fork a
    /// stalled session (persist branch only: a memory-only engine
    /// has nothing to fork and never escalates).
    db_path: Option<std::path::PathBuf>,
    /// The canonicalized workspace root, for the fast paths' file walk.
    workspace: std::path::PathBuf,
    /// The command policy the registry was built from; the fast path
    /// verifies with the same sandbox bounds the tools run under.
    policy: loom_tools::policy::CommandPolicy,
    /// Whether [`Engine::run_task`] tries a deterministic fast path
    /// before spawning the agent loop (roadmap item 3). Off in the
    /// bare-loop arm — the A/B baseline must spend its turns like a
    /// basic harness would.
    fast_paths: bool,
    /// The capability-resolution facts (source, model) to surface once
    /// per engine instance as a `CapabilitiesResolved` event (v8).
    /// `None` only for hand-built test engines.
    resolution: Option<(&'static str, String)>,
    /// Guards the one-shot emission of the resolution events.
    notices_flushed: std::sync::OnceLock<()>,
    /// The event-stream persistence seam (roadmap item 1's open tail):
    /// when the engine persists sessions, it also persists every
    /// observed event append-on-emit, so `loom stats` can attribute
    /// savings per mechanism from a stored session. `None` when
    /// `persist` was off — the stream stays in-memory only.
    events: Option<SharedEventStore>,
    /// Per-server MCP attach outcome from open time (manifest order):
    /// name and whether spawn/handshake/`tools/list` succeeded. Empty
    /// when MCP is off or no manifest declares servers.
    mcp_servers: Vec<(String, bool)>,
    /// Operator `/context` pin/drop sets shared with the pruning
    /// compiler. `None` for a bare loop or a caller-supplied compiler —
    /// both have no pruning decisions to steer.
    prune_ops: Option<loom_kernel::context::PruneOpsHandle>,
}

/// A tier agent plus the model label it runs on, for the routing
/// notice the engine attributes to each routed task.
struct RoutedAgent {
    agent: Arc<Agent>,
    model: String,
}

/// A continued session: the advanced transcript plus the task's outcome.
///
/// [`Engine::continue_session`] returns this even when the task failed or
/// was cancelled — the session is the durable artifact; the outcome is
/// what the frontend renders.
#[non_exhaustive]
#[derive(Debug)]
pub struct Continuation {
    /// The session after the task ran: input committed, turns persisted,
    /// ready for another continuation.
    pub session: Session,
    /// The task's terminal outcome. `Err` here is a task-level result
    /// ([`TaskError`]), not a load error — the load already succeeded.
    pub outcome: Result<TaskSummary, TaskError>,
}
