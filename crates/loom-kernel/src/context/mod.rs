//! Context compiler: the seam that projects durable session state into a
//! model-visible [`ModelRequest`] (architecture.md §8, ADR-006).
//!
//! The agent loop never builds a request itself. Every request passes
//! through [`ContextCompiler::compile`], which owns:
//!
//! * which transcript history is included (and the §8.3 projection rules),
//! * whether stored reasoning is replayed,
//! * which registered tools are exposed,
//! * the repository facts block (2E, [`KnownFacts`]), when one was captured,
//! * the budget check that refuses to send a request known to overflow.
//!
//! The compiler is **pure and deterministic**: no I/O, no clocks, no task
//! state beyond what [`ContextInput`] carries. IO (instruction files, repo
//! maps) is prepared before the call and passed in. It is fallible only in
//! the budget sense — a compiler that cannot fit the request returns
//! [`ContextError::BudgetExceeded`] instead of overflowing the window.
//!
//! Iteration 1 ships only [`DefaultContextCompiler`]; compaction, repo-aware
//! and subagent compilers are future implementations of the same trait
//! (§8), so context selection stays outside the agent loop.

mod default;
mod facts;
mod pruning;

pub use default::DefaultContextCompiler;
pub use facts::{FactsSource, GitState, InstructionFile, KnownFacts, NoFacts, StaticFacts};
pub use pruning::{PruneConfig, PruningContextCompiler, StubDecision, StubReason};

use crate::model::{Message, ModelCapabilities, ModelRequest};
use crate::session::{Session, TaskRecord};
use crate::tool::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How many bytes of serialized projection one estimated token stands for.
///
/// Iteration 1 has no tokenizer; this is the conservative estimate the
/// architecture mandates (§8: "≈ bytes / 3"). A real tokenizer is a
/// compiler-internal change and never touches the trait boundary.
pub(crate) const BYTES_PER_TOKEN: u32 = 3;

/// Estimates how many model tokens a serialized projection payload is.
///
/// The kernel ships only the conservative [`HeuristicEstimator`]
/// (§8: bytes ÷ [`BYTES_PER_TOKEN`]); a frontend that links a real
/// tokenizer injects it through the compiler constructors. The trait
/// keeps the kernel tokenizer-free (ADR-001) while letting the engine
/// estimate with the same family of tokenizer the provider bills.
pub trait TokenEstimator: Send + Sync + 'static {
    /// Estimated tokens for `text` — the exact serialized payload the
    /// provider bills, not the logical content.
    fn estimate(&self, text: &str) -> u32;
}

/// The kernel's default estimate: one token per [`BYTES_PER_TOKEN`]
/// bytes of serialized payload.
///
/// Conservative for code-shaped JSON (real BPE runs ~3.5–4 bytes per
/// token), so a budget decided by it errs toward refusing early rather
/// than sending an over-window request.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicEstimator;

impl TokenEstimator for HeuristicEstimator {
    fn estimate(&self, text: &str) -> u32 {
        u32::try_from(text.len() / BYTES_PER_TOKEN as usize).unwrap_or(u32::MAX)
    }
}

/// Estimates the tokens of a serialized JSON value (0 if the value
/// cannot serialize — it cannot for the kernel's types; the fallback
/// keeps estimation panic-free like [`serialized_size`]).
pub(crate) fn estimate_serialized<T: serde::Serialize>(
    estimator: &dyn TokenEstimator,
    value: &T,
) -> u32 {
    serde_json::to_string(value).map_or(0, |json| estimator.estimate(&json))
}

/// Serializes a list of [`Message`]s into a canonical string for token
/// estimation.
///
/// Infallible for the message types in this crate: every field is a
/// `String` or a serde-supported `Value`, so JSON string serialization
/// cannot fail. A `debug_assert` documents the invariant and catches a
/// future field type that could make it fallible.
fn messages_serialized(messages: &[Message]) -> String {
    let json = serde_json::to_string(messages);
    debug_assert!(json.is_ok(), "message serialization must not fail");
    json.unwrap_or_default()
}

/// How much of a model's context window a request may consume.
///
/// Derived by the caller (agent loop) from [`ModelCapabilities`] and agent
/// configuration: `max_input_tokens = max_context_tokens −
/// reserved_output_tokens` (§8). The compiler compares its token estimate
/// against this budget and refuses to compile a request that would exceed
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Maximum input tokens the compiled request may consume.
    pub max_input_tokens: u32,
    /// Tokens reserved for the model's output within the same window.
    ///
    /// Not consumed by the projection; kept so callers can derive the
    /// budget once and reason about both halves of the window.
    pub reserved_output_tokens: u32,
}

impl ContextBudget {
    /// Builds a budget from a model's context window and the output
    /// reservation. `max_context_tokens` is the model's full window; the
    /// compiler may use `max_input_tokens = max_context_tokens −
    /// reserved_output_tokens` of it (§8). Underflow (a reservation larger
    /// than the window) is a caller error, so it is rejected loudly.
    pub fn from_window(
        max_context_tokens: u32,
        reserved_output_tokens: u32,
    ) -> Result<Self, ContextError> {
        let max_input_tokens = max_context_tokens
            .checked_sub(reserved_output_tokens)
            .ok_or_else(|| {
                ContextError::InvalidConfig(format!(
                    "reserved output tokens ({reserved_output_tokens}) exceed the \
                     context window ({max_context_tokens})"
                ))
            })?;
        Ok(Self {
            max_input_tokens,
            reserved_output_tokens,
        })
    }
}

/// Everything a context compiler needs to build one request.
///
/// Pure inputs only: the compiler may touch nothing but these fields and
/// must stay deterministic. IO (instruction files, repo maps, memory
/// snapshots) is prepared by the caller and passed in.
#[non_exhaustive]
pub struct ContextInput<'a> {
    /// The durable session whose transcript is projected.
    pub session: &'a Session,
    /// Registered tools; the compiler decides which to expose (§8.2).
    pub tools: &'a ToolRegistry,
    /// The task this request serves, for task-scoped context decisions.
    pub task: &'a TaskRecord,
    /// The token budget the compiled request must fit.
    pub budget: ContextBudget,
    /// The target model's capabilities, so unsupported requests are never
    /// built (e.g. tools to a model without `tool_calls`).
    pub capabilities: &'a ModelCapabilities,
    /// The provider-specific model identifier the request targets
    /// (§6.4). This is the `model` string the agent resolves via
    /// [`crate::model::ModelProvider::capabilities`]; the compiler copies
    /// it into [`ModelRequest::model`] so a request always names its target
    /// model.
    pub model: &'a str,
    /// Agent configuration: workspace root (for relative tool arguments)
    /// plus any future knobs the compiler reads.
    pub config: &'a AgentConfig,
    /// Repository facts captured once for this task (2E, ADR-015), or an
    /// empty value when no source is attached. Rendered as a stable system
    /// message right after the system prompt; empty facts add no message.
    pub facts: &'a KnownFacts,
    /// The pruning checkpoint from the previous turn, when a pruning
    /// compiler is in use (ADR-011). Frozen decisions are re-applied so the
    /// projection stays byte-stable between triggers.
    pub prune_checkpoint: Option<&'a PruneCheckpoint>,
}

/// Agent configuration the context compiler reads.
///
/// Kept as a separate struct (not scattered fields on [`ContextInput`])
/// because it is the one caller-owned value the compiler both needs now and
/// will grow later. `#[non_exhaustive]` so adding knobs is not a breaking
/// change for other crates.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The workspace root: what relative tool arguments resolve against and
    /// what absolute workspace-relative paths report (§9.1).
    pub workspace_root: PathBuf,
    /// Static system prompt, emitted as the stable prefix of every
    /// request (§8.1.1).
    pub system_prompt: String,
}

impl AgentConfig {
    /// Assembles a config from its current fields.
    #[must_use]
    pub fn new(workspace_root: PathBuf, system_prompt: impl Into<String>) -> Self {
        Self {
            workspace_root,
            system_prompt: system_prompt.into(),
        }
    }
}

/// Per-source attribution of a compiled request's estimated context
/// (ADR-016, V18).
///
/// Every source field is the compiler's Iteration-1 estimate — serialized
/// bytes ÷ 3 — for one part of the projection, so the sources can be compared
/// across turns without a tokenizer. The per-source values round down
/// independently and therefore need not sum exactly to
/// [`total`](ContextCensus::total); `total` is the authoritative figure the
/// budget check compares (the whole message list plus tool list, serialized
/// once, ÷ 3, plus one token per message for framing) and is exactly what
/// [`ContextError::BudgetExceeded`] reports.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextCensus {
    /// The static system prompt (§8.1.1).
    pub system_prompt: u32,
    /// The repository facts block, when one was projected (2E, ADR-015).
    /// Counted apart from the system prompt it follows.
    pub repo_facts: u32,
    /// Task and user items in the projection.
    pub user_items: u32,
    /// Replayed assistant history: text and tool-call arguments, excluding
    /// reasoning.
    pub assistant_history: u32,
    /// Replayed reasoning traces (§4.2), counted apart from the assistant
    /// text they accompany.
    pub reasoning_replay: u32,
    /// Tool results replayed from the transcript.
    pub tool_results: u32,
    /// Exposed tool definitions.
    pub tool_specs: u32,
    /// Authoritative total: the estimate the budget check compares.
    pub total: u32,
}

/// A frozen pruning decision set (ADR-011).
///
/// Produced by [`PruningContextCompiler`] when the absolute trigger fires,
/// and passed back in on later turns via [`ContextInput::prune_checkpoint`].
/// Freezing matters for caching: between triggers the projection is
/// byte-stable, so the provider's prefix cache holds and only each *trigger*
/// costs one miss. Recomputing decisions every turn would invalidate the
/// cache every turn, which costs more than the pruning saves.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneCheckpoint {
    /// Transcript length (in items) when the decisions were computed. A
    /// later turn appends past this; the frozen decisions still apply.
    pub upto_item: usize,
    /// The frozen stub decisions, in strategy order.
    pub stubs: Vec<StubDecision>,
}

/// The result of a successful [`ContextCompiler::compile`].
///
/// A struct (rather than returning [`ModelRequest`] bare) so the pruning
/// compiler can return a [`PruneCheckpoint`] for the caller to persist
/// without a breaking change (ADR-011).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledContext {
    /// The complete model request: system prefix, projected history, and
    /// the exposed tool definitions.
    pub request: ModelRequest,
    /// Per-source attribution of `request`'s estimated context (ADR-016).
    pub census: ContextCensus,
    /// The pruning checkpoint to persist for the next compile, when a
    /// pruning compiler produced one (ADR-011). `None` for the default
    /// compiler and for a pruning compiler that has not triggered.
    pub prune_checkpoint: Option<PruneCheckpoint>,
}

/// Why [`ContextCompiler::compile`] failed.
///
/// Iteration 1 has a single failure mode: the projection's token estimate
/// exceeds the budget, and sending the request would be known to overflow.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// The compiled projection's estimated token count exceeds
    /// [`ContextBudget::max_input_tokens`]. The caller may retry with a
    /// larger budget or a smaller projection (e.g. a new task).
    #[error("context budget exceeded: {estimated} estimated tokens > {max_input_tokens} allowed")]
    BudgetExceeded {
        /// The compiler's token estimate for the projection.
        estimated: u32,
        /// The budget it had to fit.
        max_input_tokens: u32,
    },
    /// The configuration itself is unusable (e.g. an output reservation
    /// larger than the model's window).
    #[error("invalid context configuration: {0}")]
    InvalidConfig(String),
}

/// The seam through which every model request is built (§8, ADR-006).
///
/// Pure and deterministic: calling `compile` twice with the same inputs
/// yields the same request. Implementations must never perform I/O.
pub trait ContextCompiler: Send + Sync {
    /// Projects `input` into a model request, enforcing the budget.
    ///
    /// # Errors
    ///
    /// [`ContextError::BudgetExceeded`] when the projection cannot fit the
    /// budget, so the caller fails before sending a request known to
    /// overflow.
    fn compile(&self, input: ContextInput<'_>) -> Result<CompiledContext, ContextError>;
}
