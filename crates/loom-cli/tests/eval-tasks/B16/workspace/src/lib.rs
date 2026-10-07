//! Window-relative pruning trigger.
//!
//! ## Contract
//!
//! [`PruneConfig`] describes when a session's projected context is large
//! enough to warrant a prune:
//!
//! * `trigger_percent` — the trigger as a percent of the model's input
//!   budget (its context window less the reserved output);
//! * `floor_tokens` — the trigger never drops below this many tokens, so a
//!   small window still gets a sane minimum.
//!
//! [`trigger_tokens`] computes the absolute trigger for a given input
//! budget: `trigger_percent` of `max_input_tokens`, floored at
//! `floor_tokens`, computed with a saturating multiply so an extreme
//! `max_input_tokens` cannot overflow (it saturates toward the floor
//! instead of panicking or wrapping).
//!
//! A session should prune once its estimated total exceeds the trigger —
//! never merely at or below it. [`should_prune`] is that comparison.
//!
//! The trigger is **window-relative**: it scales with the model's input
//! budget, so a model with a huge window stays dormant on a session that
//! would blow a small, flat threshold, and a model with a small window
//! still gets `floor_tokens` as a floor.

/// Pruning trigger configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneConfig {
    /// Trigger as a percent (0-100 and beyond) of the model's input budget.
    pub trigger_percent: u32,
    /// The trigger never drops below this many tokens.
    pub floor_tokens: u32,
}

/// The absolute trigger for a given input budget: `trigger_percent` of
/// `max_input_tokens`, floored at `floor_tokens`.
pub fn trigger_tokens(config: &PruneConfig, max_input_tokens: u32) -> u32 {
    config.floor_tokens
}

/// Whether a session's estimated total token count has crossed the
/// window-relative trigger.
pub fn should_prune(total_tokens: u32, config: &PruneConfig, max_input_tokens: u32) -> bool {
    total_tokens > trigger_tokens(config, max_input_tokens)
}
