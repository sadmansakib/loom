//! Agent orchestration: driving the model–tool loop deterministically.
//!
//! [`Agent::run_task`] is the turn loop of architecture.md §13: it compiles
//! bounded context through the [`ContextCompiler`], streams a response
//! through the [`ModelProvider`] with bounded, classification-based retries
//! ([`RetryPolicy`], §6.3), executes any requested tool calls through the
//! tool engine ([`tools`], §10), and commits each turn atomically to the
//! [`Session`] transcript (§6.1).
//!
//! The loop is a deterministic controller; the model is the reasoning
//! component. It contains no provider-specific branches beyond the
//! capability and response semantics defined by `loom-kernel`, and it never
//! imports a concrete provider or tool (ADR-001): both arrive as
//! trait objects and a registry.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::context::{AgentConfig, ContextCompiler, FactsSource};
use crate::model::ModelProvider;
use crate::store::{ArtifactStore, SessionStore};
use crate::tool::ToolRegistry;
use crate::tool_cache::ToolCache;
pub mod retry;
pub mod surface;
pub mod tools;

pub use retry::RetryPolicy;
pub use surface::{DEFAULT_EVICT_AFTER_IDLE_TURNS, SurfacePolicy, ToolSurface};

mod budget;
mod builder;
mod continuation;
mod control;
mod error;
mod execute;
mod feedback;
mod run;
mod streaming;
mod summary;
#[cfg(test)]
mod tests;
mod turn;

pub use budget::{
    DEFAULT_RESERVED_OUTPUT_TOKENS, MAX_RESERVED_OUTPUT_TOKENS, OutputReservation, RemainingBudget,
    TaskBudget, TurnBudget, derive_reserved_output_tokens,
};
pub use control::TaskControl;
pub use error::TaskError;
pub use feedback::{
    EDIT_STALL_TURNS, FAILED_CALL_TURNS_LIMIT, FAILED_CALL_TURNS_NUDGE,
    MALFORMED_ARGUMENT_TURNS_LIMIT, ZERO_PROGRESS_TURNS,
};
pub use summary::{FastPathSolve, TaskSummary};

/// Default maximum number of model turns per task when nothing else is
/// configured.
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Default per-tool-call timeout: the bound on the agent's wait for a tool
/// result (§6.6). Configurable via [`Agent::with_tool_timeout`].
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// Default bound on concurrent read-only tool calls (§10).
pub const DEFAULT_MAX_TOOL_CONCURRENCY: usize = tools::DEFAULT_MAX_CONCURRENCY;

/// The deterministic controller of the model–tool loop (§13).
///
/// Constructed from the four seams it depends on — a [`ModelProvider`], a
/// [`ContextCompiler`], a [`ToolRegistry`], and an [`AgentConfig`] — plus the
/// model identifier whose capabilities are resolved per task (§6.4). All
/// other knobs have documented defaults and builder setters.
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    compiler: Box<dyn ContextCompiler>,
    registry: ToolRegistry,
    config: AgentConfig,
    model: String,
    retry: RetryPolicy,
    max_turns: u32,
    turn_budget: TurnBudget,
    budget: TaskBudget,
    output_reservation: OutputReservation,
    tool_timeout: Duration,
    max_tool_concurrency: usize,
    store: Option<Box<dyn SessionStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    facts: Arc<dyn FactsSource>,
    cache: Arc<Mutex<ToolCache>>,
    /// Whether the loop's evidence-shaped feedback is active (roadmap
    /// item 1's bare-loop arm turns it off): the completion-gate
    /// refusal, the failure-ledger repeat refusal, and the
    /// recent-changes evidence handed to tools. `false` reduces the
    /// loop to stream → execute → commit with no loop-owned reactions.
    loop_shaping: bool,
    /// The session's dynamic tool surface (slice D2): tools queued by
    /// a meta-tool mid-turn, promoted at each turn boundary. `None`
    /// for every engine that doesn't opt in — the loop then uses the
    /// base registry unchanged.
    surface: Option<Arc<ToolSurface>>,
}
