//! The scout's bounds and configuration knobs (roadmap item 19).

/// Hard answer bound: the capsule contract promises the parent ≤1 KB.
pub const ANSWER_BUDGET_BYTES: usize = 1_000;

/// Parallel scouts one parent may run at once. Over the cap the call
/// fails soft ("at capacity") rather than queueing — a parent that
/// fires a fan of scouts should not silently stack a backlog of child
/// spends behind a semaphore.
pub const SCOUT_CONCURRENCY: usize = 3;

/// Configuration for the scout child agent (roadmap item 19).
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    /// Provider adapter for the child — mid-task model selection applies
    /// to the child only; the parent's model stays fixed.
    pub provider: String,
    /// The cheap model that answers the capsule.
    pub model: String,
    /// Child turn ceiling; delegation beyond this is a design smell, not
    /// a budget. Defaults to 8 via [`ScoutConfig::defaults`].
    pub max_turns: u32,
    /// Wall-clock budget for one capsule. Defaults to 120 s.
    pub timeout_secs: u64,
    /// Optional stronger model for cascade escalation and cross-checks
    /// (docs/subagents-design.md §3). `None` keeps the single-model
    /// scout.
    pub escalation: Option<ScoutEscalation>,
}

/// The escalation target: a (provider, model) pair for the strong
/// second opinion. One escalation per capsule, hard-bounded — no
/// chains, no judge models.
#[derive(Debug, Clone)]
pub struct ScoutEscalation {
    /// Provider id for the strong model (resolved like the scout's own).
    pub provider: String,
    /// Strong model id; also the provenance label in escalated output.
    pub model: String,
}

impl ScoutConfig {
    /// Fills the knob defaults the roadmap set: ~8 child turns, 120 s.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            max_turns: 8,
            timeout_secs: 120,
            escalation: None,
        }
    }

    /// Sets the cascade/cross-check escalation target (builder).
    pub fn with_escalation(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.escalation = Some(ScoutEscalation {
            provider: provider.into(),
            model: model.into(),
        });
        self
    }
}
