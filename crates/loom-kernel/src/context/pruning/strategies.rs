use std::collections::{HashMap, HashSet};

use crate::context::TokenEstimator;
use crate::session::Session;
use crate::tool::{ToolRegistry, paths_overlap};
use crate::transcript::ToolStatus;

use super::config::PruneConfig;
use super::elision::{
    elide_aged_assistant_text, elide_aged_calls, elide_aged_reasoning, elide_aged_users,
};
use super::facts::{ResultFacts, TranscriptFacts};
use super::reclaim::stub_tokens;
use super::types::{ArgElision, StubDecision, StubReason};

/// Accumulated stub and elision decisions across the strategy passes,
/// in ADR-011's fixed order — earlier passes win, later ones skip
/// already-decided items so each keeps its most specific reason.
#[derive(Default)]
pub(super) struct DecisionSet {
    pub(super) stubbed: Vec<StubDecision>,
    pub(super) stubbed_ids: HashSet<String>,
    pub(super) elided_args: Vec<ArgElision>,
    pub(super) elided_arg_ids: HashSet<String>,
    pub(super) elided_users: Vec<usize>,
    pub(super) elided_reasoning: Vec<usize>,
    pub(super) elided_assistant_text: Vec<usize>,
    pub(super) reclaim_tokens: u64,
}

impl DecisionSet {
    /// Records one result stub and its estimated reclaim.
    pub(super) fn add(
        &mut self,
        facts: &ResultFacts,
        reason: StubReason,
        estimator: &dyn TokenEstimator,
    ) {
        self.reclaim_tokens += u64::from(facts.tokens.saturating_sub(stub_tokens(
            &facts.call_id,
            facts.status,
            reason,
            estimator,
        )));
        self.stubbed_ids.insert(facts.call_id.as_str().to_owned());
        self.stubbed.push(StubDecision {
            call_id: facts.call_id.as_str().to_owned(),
            reason,
        });
    }

    /// Whether every decision list is empty — the rescue's
    /// nothing-prunable test.
    pub(super) fn is_empty(&self) -> bool {
        self.stubbed.is_empty()
            && self.elided_args.is_empty()
            && self.elided_users.is_empty()
            && self.elided_reasoning.is_empty()
            && self.elided_assistant_text.is_empty()
    }

    /// Whether `facts` is outside the protected region and not already
    /// stubbed by an earlier pass — the shared guard of passes 2–5.
    pub(super) fn eligible(&self, facts: &ResultFacts, protected_from: usize) -> bool {
        facts.index < protected_from && !self.stubbed_ids.contains(facts.call_id.as_str())
    }
}

/// Pass 1 — dedupe identical read-only calls, keeping the newest.
fn dedupe_reads(
    facts: &TranscriptFacts,
    protected_from: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    // Index of the newest call per (name, canonical args): one pass
    // instead of scanning every later result per candidate.
    let mut newest_by_call: HashMap<(&str, &str), usize> = HashMap::new();
    for result in &facts.results {
        let Some(meta) = facts.read_meta(result) else {
            continue;
        };
        newest_by_call
            .entry((meta.name.as_str(), meta.args.as_str()))
            .and_modify(|index| *index = (*index).max(result.index))
            .or_insert(result.index);
    }
    for earlier in &facts.results {
        if earlier.index >= protected_from {
            continue;
        }
        let Some(meta) = facts.read_meta(earlier) else {
            continue;
        };
        let is_newest = newest_by_call
            .get(&(meta.name.as_str(), meta.args.as_str()))
            .is_some_and(|index| *index == earlier.index);
        if !is_newest {
            pass.add(earlier, StubReason::DuplicateRead, estimator);
        }
    }
}

/// Pass 2 — supersede reads of later-modified paths.
fn supersede_reads(
    facts: &TranscriptFacts,
    protected_from: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    for earlier in &facts.results {
        if !pass.eligible(earlier, protected_from) {
            continue;
        }
        let Some(meta) = facts.read_meta(earlier) else {
            continue;
        };
        let superseded = facts.writes.iter().any(|(index, effects)| {
            *index > earlier.index
                && meta.reads.iter().any(|read| {
                    effects
                        .files_written
                        .iter()
                        .chain(effects.files_deleted.iter())
                        .any(|path| paths_overlap(read, path))
                })
        });
        if superseded {
            pass.add(earlier, StubReason::SupersededRead, estimator);
        }
    }
}

/// Pass 3 — stub error results older than `stale_error_turns`.
fn stub_stale_errors(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    for earlier in &facts.results {
        if !pass.eligible(earlier, protected_from) {
            continue;
        }
        if earlier.status != ToolStatus::Success
            && total_turns.saturating_sub(earlier.turn) > config.stale_error_turns
        {
            pass.add(earlier, StubReason::StaleError, estimator);
        }
    }
}

/// Pass 4 — age out large read-only results outside the protected tail.
/// `min_reclaim_tokens` is a *gate* on whether the prune runs at all
/// (checked in `compile`), never a cap: every unprotected read-only
/// result is stubbed, so the projection cannot grow without bound once
/// the trigger is crossed. Sorted largest-first for a deterministic,
/// size-prioritised order. Window-triggered (or budget-rescue) passes
/// only: turn-age masking alone must not nuke the whole history.
fn age_out_large_reads(
    facts: &TranscriptFacts,
    protected_from: usize,
    window_triggered: bool,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let mut aged: Vec<&ResultFacts> = facts
        .results
        .iter()
        .filter(|result| {
            window_triggered
                && result.index < protected_from
                && result.status == ToolStatus::Success
                && result.read_call.is_some()
                && !pass.stubbed_ids.contains(result.call_id.as_str())
        })
        .collect();
    aged.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.index.cmp(&b.index)));
    for result in aged {
        pass.add(result, StubReason::AgedOut, estimator);
    }
}

/// Passes 5 and 5b — mask successful read-only results older than the
/// configured age windows. Turn-age masking is independent of the window
/// trigger; writing-phase masking is enabled only after a recent write.
/// Runs after earlier passes so already-stubbed calls keep their more
/// specific reasons, and in general-before-specific order so turn-age
/// wins when both windows apply. The protected tail always wins.
fn mask_aged_reads(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let writing_phase = facts
        .last_write_turn
        .is_some_and(|turn| total_turns.saturating_sub(turn) <= WRITING_PHASE_DETECT_TURNS);
    let phase_mask_turns = config.phase_mask_turns.filter(|_| writing_phase);
    let windows = [
        (config.age_mask_turns, StubReason::TurnAge),
        (phase_mask_turns, StubReason::WritingPhase),
    ];

    for (max_age, reason) in windows {
        let Some(max_age) = max_age else {
            continue;
        };
        for result in &facts.results {
            if pass.eligible(result, protected_from)
                && result.status == ToolStatus::Success
                && result.read_call.is_some()
                && total_turns.saturating_sub(result.turn) > max_age
            {
                pass.add(result, reason, estimator);
            }
        }
    }
}

/// How close to the tail a write must sit for the loop to count as in
/// the writing phase. A constant, not a knob: detection is a
/// deterministic phase classifier, and selfopt sweeps the *mask* window
/// (`LOOM_PHASE_MASK_TURNS`), not the classifier.
const WRITING_PHASE_DETECT_TURNS: usize = 3;

/// Computes the decision set from the transcript under ADR-011's fixed
/// strategy order, plus the reclaim estimate (in tokens) against the
/// *unpruned* projection.
pub(super) fn decide(
    session: &Session,
    registry: &ToolRegistry,
    config: PruneConfig,
    estimator: &dyn TokenEstimator,
    window_triggered: bool,
) -> DecisionSet {
    let facts = TranscriptFacts::collect(session, registry, estimator);
    let protected_from = facts.protected_from(&session.transcript, config, estimator);
    let total_turns = facts.assistant_at.len();

    let mut pass = DecisionSet::default();
    dedupe_reads(&facts, protected_from, estimator, &mut pass);
    supersede_reads(&facts, protected_from, estimator, &mut pass);
    stub_stale_errors(
        &facts,
        protected_from,
        config,
        total_turns,
        estimator,
        &mut pass,
    );
    age_out_large_reads(
        &facts,
        protected_from,
        window_triggered,
        estimator,
        &mut pass,
    );
    mask_aged_reads(
        &facts,
        protected_from,
        config,
        total_turns,
        estimator,
        &mut pass,
    );
    elide_aged_calls(
        &facts,
        protected_from,
        config,
        total_turns,
        estimator,
        &mut pass,
    );
    elide_aged_users(
        &facts,
        &session.transcript,
        protected_from,
        config,
        total_turns,
        estimator,
        &mut pass,
    );
    elide_aged_reasoning(&facts, protected_from, config, total_turns, &mut pass);
    elide_aged_assistant_text(
        &facts,
        session,
        protected_from,
        config,
        window_triggered,
        estimator,
        &mut pass,
    );

    pass
}
