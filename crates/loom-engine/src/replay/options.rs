//! The replay's surface types: the invocation knobs, the per-turn and
//! whole-run summaries, and the error taxonomy shared by every step.

use std::io;
use std::path::PathBuf;

use loom_kernel::context::{ContextCensus, PruneReport};

/// Assumed window when neither the input nor the caller names one
/// (`deepseek-flash`, the eval model, has 1M).
pub const DEFAULT_WINDOW_TOKENS: u32 = 1_000_000;
/// Assumed output reservation (the engine's automatic reservation cap).
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 32_768;

/// Reconstruction constants shared by both replay arms.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Provider adapter name; picks the estimator family the original run
    /// used (`anthropic` scales o200k, everything else is o200k).
    pub provider: String,
    /// Full context window assumed for the budget (`--window`).
    pub window_tokens: u32,
    /// Output reservation assumed for the budget (`--reserved-output`).
    pub reserved_output_tokens: u32,
    /// System prompt used for the reconstruction (not recorded in traces).
    pub system_prompt: String,
    /// Workspace root for [`AgentConfig`]; compile never touches it.
    pub workspace_root: PathBuf,
}

impl ReplayOptions {
    /// Defaults for a provider; see the field docs for what is assumed.
    #[must_use]
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            window_tokens: DEFAULT_WINDOW_TOKENS,
            reserved_output_tokens: DEFAULT_RESERVED_OUTPUT_TOKENS,
            system_prompt: crate::engine::default_system_prompt(),
            workspace_root: PathBuf::from("."),
        }
    }
}

/// One replayed compile (turn numbers are 1-based compile ordinals).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReplay {
    /// 1-based turn ordinal, aligned with `TurnStarted` numbering.
    pub turn: u32,
    /// `census.total` — the authoritative estimate the budget check sees.
    pub input_tokens: u32,
    /// Per-source census split.
    pub census: ContextCensus,
    /// Set only on the compile that froze a new checkpoint.
    pub froze: Option<PruneReport>,
    /// Common byte prefix with the previous compiled request ÷ previous
    /// request length. `None` on the first turn.
    pub prefix_stability: Option<f64>,
    /// Calls stubbed in this compiled projection.
    pub stubbed_calls: usize,
}

/// Aggregate over one replay arm.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaySummary {
    /// Per-turn results, in order.
    pub turns: Vec<TurnReplay>,
    /// Sum of per-turn `census.total` (projected input spend).
    pub total_input_tokens: u64,
    /// Token estimate reclaimed by freezes.
    pub reclaimed_tokens: u64,
    /// Number of freezes.
    pub freezes: u64,
    /// Median per-turn input tokens (the selfopt gate statistic).
    pub median_input_tokens: f64,
    /// Smallest prefix-stability ratio across the arm.
    pub min_prefix_stability: Option<f64>,
    /// Set when a projection overran past rescue: the live loop would
    /// spend one model call on an LLM handoff, which replay cannot
    /// synthesize — the arm stops here.
    pub handoff_stopped_at: Option<u32>,
}

/// Replay failures that are input problems, not measurement results.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The recorded input is malformed (bad manifest/transcript shape).
    #[error("invalid trace: {0}")]
    InvalidTrace(String),
    /// Reading the trace directory failed.
    #[error("reading trace: {0}")]
    Io(#[from] io::Error),
    /// The budget constants are inconsistent (reservation > window).
    #[error("{0}")]
    Budget(String),
}
