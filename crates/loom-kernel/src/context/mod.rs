//! Context compiler: the seam that projects durable session state into a
//! model-visible [`ModelRequest`] (architecture.md §8, ADR-006).
//!
//! The agent loop never builds a request itself. Every request passes
//! through [`ContextCompiler::compile`], which owns:
//!
//! * which transcript history is included (and the §8.3 projection rules),
//! * whether stored reasoning is replayed,
//! * which registered tools are exposed,
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

pub use default::DefaultContextCompiler;

use crate::model::{Message, ModelCapabilities, ModelRequest};
use crate::session::{Session, TaskRecord};
use crate::tool::ToolRegistry;
use std::path::PathBuf;

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

/// The result of a successful [`ContextCompiler::compile`].
///
/// A struct (rather than returning [`ModelRequest`] bare) so a future
/// pruning compiler can return a checkpoint without a breaking change
/// (ADR-011).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledContext {
    /// The complete model request: system prefix, projected history, and
    /// the exposed tool definitions.
    pub request: ModelRequest,
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
