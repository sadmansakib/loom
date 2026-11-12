//! Per-skill `load_skill` telemetry and the item-20.4 lifecycle
//! verdicts computed from it.

use serde::{Deserialize, Serialize};

use super::stats::SessionStats;

/// Per-skill telemetry for `load_skill`, keyed by the menu name the
/// model asked for: usage frequency plus delivered context cost, the
/// two numbers a skill-lifecycle decision (keep, rewrite, retire)
/// needs. Calls whose arguments carried no parseable name land under
/// `"?"`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStats {
    /// Successful body loads (`name` alone).
    pub loads: u64,
    /// Successful bundled-resource reads (`name` + `resource`).
    pub resources: u64,
    /// Failed calls of any kind (unknown name, malformed arguments).
    pub failures: u64,
    /// Bytes delivered into context across successful loads and
    /// resources — the per-skill context cost.
    pub delivered_bytes: u64,
}

impl SkillStats {
    /// Adds `other` into `self`, counter by counter (store-wide
    /// totals).
    pub fn add(&mut self, other: &SkillStats) {
        self.loads += other.loads;
        self.resources += other.resources;
        self.failures += other.failures;
        self.delivered_bytes += other.delivered_bytes;
    }

    /// The lifecycle verdict this skill earns on the telemetry alone
    /// ([`skill_retirement_verdicts`]): what `loom stats` prints as
    /// `[retire]` / `[watch]` beside the numbers.
    ///
    /// Data, not a mutation: the rule never hides a skill. A verdict
    /// of `Retire` says the evidence has now argued against it; acting
    /// on it (renaming the file, shrinking the library) is the
    /// operator's step, mirroring how the artifact GC reports orphans
    /// while a separate pass removes them.
    #[must_use]
    pub fn retirement_verdict(&self) -> SkillVerdict {
        let successes = self.loads + self.resources;
        let raw_failure = successes == 0 && self.failures > 0;
        // Failures outweigh successes, on enough evidence that the
        // sample is not pure noise.
        let failure_heavy = successes + self.failures >= SUCCESSFUL_CALLS_EVALUATED
            && self.failures * 2 > successes;
        let unused = successes == 0
            && self.failures == 0
            && self.delivered_bytes > MIN_DELIVERED_BYTES_EVALUATED;
        if raw_failure || failure_heavy || unused {
            SkillVerdict::Retire
        } else if successes < MIN_SUCCESSFUL_CALLS_EVALUATED
            && self.delivered_bytes > MAX_DELIVERED_BYTES_EVALUATED
        {
            SkillVerdict::Watch
        } else {
            SkillVerdict::Keep
        }
    }
}

/// Minimum successful `load_skill` calls for a skill to count as
/// "evaluated": below this, outcome evidence is too thin for anything
/// but a watch on cost.
pub const MIN_SUCCESSFUL_CALLS_EVALUATED: u64 = 8;

/// A skill is decisively unhealthy when failures outweigh successes
/// and at least this many calls (successes + failures) exist — pure
/// noise must not loop a skill to retirement.
pub const SUCCESSFUL_CALLS_EVALUATED: u64 = 4;

/// Above this many bytes delivered with fewer than
/// [`MIN_SUCCESSFUL_CALLS_EVALUATED`] successful calls, a skill is [watch]ed:
/// expensive relative to rare use.
pub const MAX_DELIVERED_BYTES_EVALUATED: u64 = 1_000;

/// A skill with zero successful calls and zero failures but delivered
/// bytes was never actually exercised — its bytes are bookkeeping
/// ghosts, and it retires outright.
pub const MIN_DELIVERED_BYTES_EVALUATED: u64 = 1;

/// The item-20.4 lifecycle verdict for one skill, computed from its
/// [`SkillStats`] alone. Pure, deterministic, store-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillVerdict {
    /// The telemetry defends this skill: healthy, or simply too
    /// quiet to judge.
    Keep,
    /// Expensive for how seldom it is used: shrink, rewrite, or
    /// gate it before it grows.
    Watch,
    /// The evidence has argued against it: only failures, failures
    /// outweighing successes, or bytes with no successful use at all.
    Retire,
}

impl std::fmt::Display for SkillVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillVerdict::Keep => f.write_str("keep"),
            SkillVerdict::Watch => f.write_str("watch"),
            SkillVerdict::Retire => f.write_str("retire"),
        }
    }
}

/// Every skill's lifecycle verdict over a folded [`SessionStats`],
/// in name order. This is the item-20.4 rule: name-keyed, outcome-
/// joined telemetry in; a keep/watch/retire row out. Names here are
/// the menu names the model asked for — including `"?"` (malformed
/// arguments), which always verdicts `Retire` if any such failure was
/// tallied, because there is no skill to keep.
#[must_use]
pub fn skill_retirement_verdicts(stats: &SessionStats) -> Vec<(String, SkillVerdict)> {
    stats
        .skills
        .iter()
        .map(|(name, skill)| (name.clone(), skill.retirement_verdict()))
        .collect()
}
