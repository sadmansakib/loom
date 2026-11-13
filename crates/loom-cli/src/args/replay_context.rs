//! `loom replay-context` argument types (clap derive).

use clap::Args;

/// Arguments for `loom replay-context`.
#[derive(Debug, Args)]
pub struct ReplayContextArgs {
    /// What to replay: a trace directory written by `loom run --trace`
    /// (contains `manifest.json`) or a session id from `loom sessions`.
    #[arg(value_name = "TRACE_DIR_OR_SESSION")]
    pub source: String,

    /// Overlay one prune knob for the counterfactual arm, e.g.
    /// `--set prune_trigger_percent=50`. Repeatable. Known keys:
    /// `prune_trigger_percent`, `prune_floor_tokens`,
    /// `prune_min_reclaim_tokens`, `prune_tail_tokens`,
    /// `phase_mask_turns`, `age_mask_turns`, `elide_turns`, `elide_reasoning`,
    /// `elide_assistant_text` (`off` disables an optional knob).
    #[arg(long = "set", value_name = "KEY=VAL")]
    pub set: Vec<String>,

    /// Provider whose token estimator and base prune config to use.
    /// Defaults to the trace manifest's provider; required when
    /// replaying a stored session (sessions do not record it).
    #[arg(long)]
    pub provider: Option<String>,

    /// Max context window assumed for the replay (default 1,000,000).
    #[arg(long, value_name = "TOKENS")]
    pub window: Option<u32>,

    /// Output tokens reserved (default 32,768).
    #[arg(long, value_name = "TOKENS")]
    pub reserved_output: Option<u32>,

    /// Override the system prompt (default: Loom's built-in prompt).
    #[arg(long, value_name = "TEXT")]
    pub system_prompt: Option<String>,

    /// Emit machine-readable JSON instead of the human tables.
    #[arg(long)]
    pub json: bool,
}
