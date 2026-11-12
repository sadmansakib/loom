/// Pruning thresholds; the defaults are ADR-011's hypotheses, tuned by the
/// 2L long-context benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneConfig {
    /// Trigger when the estimated input exceeds this percent of the model's
    /// input budget (its context window less the reserved output).
    pub trigger_percent: u32,
    /// …but never below this many estimated tokens.
    pub floor_tokens: u32,
    /// A prune only proceeds when at least this many tokens are
    /// reclaimable; below it the churn is not worth a cache miss.
    pub min_reclaim_tokens: u32,
    /// The trailing context is never pruned down to less than this many
    /// estimated tokens.
    pub protected_tail_tokens: u32,
    /// …and never fewer than this many whole turns.
    pub protected_tail_turns: usize,
    /// Errors older than this many turns are stale.
    pub stale_error_turns: usize,
    /// Turn-age masking (roadmap item 10): stub successful read-only
    /// results older than this many assistant turns *even below the
    /// window trigger* — SWE-agent's last-observations ablation beat
    /// full history (18.0 vs 15.0), and a window-relative trigger never
    /// fires on a large context. `None` disables the behavior.
    pub age_mask_turns: Option<usize>,
    /// Phase-aware retention (roadmap item 10): once the loop is in
    /// the writing phase (a trailing assistant turn wrote or deleted a
    /// file), stub successful read-only results older than this many
    /// assistant turns even below the `age_mask_turns` window —
    /// TokenPilot's lifecycle finding that exploration reads lose
    /// value fastest once editing starts. `None` (the default) keeps
    /// the shipped behavior; selfopt-gated like every other stub
    /// class.
    pub phase_mask_turns: Option<usize>,
    /// History elision window (roadmap item 12, deterministic half):
    /// tool calls older than this many assistant turns have their
    /// arguments reduced to a signature (long strings elided), and
    /// user items after the task brief older than the same window are
    /// stubbed. `None` disables both.
    pub elide_turns: Option<usize>,
    /// Minimum serialized length a string inside old call arguments
    /// must exceed to be elided; shorter strings (paths, flags, ids)
    /// always survive so the call keeps a usable signature.
    pub elide_arg_chars: usize,
    /// Reasoning-elision window: assistant reasoning traces older than
    /// this many assistant turns are dropped from the projection
    /// (`reasoning = None`). `None` (the default) disables it —
    /// replaying reasoning is a provider contract on the OpenAI wire,
    /// so only the engine's provider gate should turn this on.
    pub elide_reasoning: Option<usize>,
    /// Assistant-text elision window (roadmap item 12, last-resort
    /// deterministic half): assistant texts older than this many
    /// assistant turns are reduced to a fixed-character signature.
    /// Window/rescue-triggered passes only — a routine aging batch
    /// never rewrites prose the model may still anchor on. `None`
    /// disables it.
    pub elide_assistant_text: Option<usize>,
    /// Minimum character length a text must exceed to be elision
    /// candidate; shorter texts survive verbatim. The signature itself
    /// (first 200 characters plus a `[pruned N chars]` marker) is a
    /// constant — frozen re-application must not depend on config
    /// drift.
    pub elide_assistant_chars: usize,
}

impl PruneConfig {
    /// The absolute trigger for a given input budget: `trigger_percent` of
    /// the budget, floored at `floor_tokens`.
    ///
    /// Window-relative by default so that pruning stays dormant for a large
    /// context window (a 1M-token model never prunes a 64K transcript) and
    /// a flat 64K threshold cannot make it fire on every turn of a
    /// prefix-cached session, where each checkpoint is a cache miss.
    #[must_use]
    pub fn trigger_tokens(&self, max_input_tokens: u32) -> u32 {
        (max_input_tokens.saturating_mul(self.trigger_percent) / 100).max(self.floor_tokens)
    }
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            trigger_percent: 80,
            floor_tokens: 64_000,
            min_reclaim_tokens: 20_000,
            protected_tail_tokens: 20_000,
            protected_tail_turns: 3,
            stale_error_turns: 4,
            age_mask_turns: Some(8),
            phase_mask_turns: None,
            elide_turns: Some(8),
            elide_arg_chars: 160,
            elide_reasoning: None,
            elide_assistant_text: Some(8),
            elide_assistant_chars: 400,
        }
    }
}
