//! Trigger-gated deterministic context pruning (2L, ADR-011).
//!
//! [`PruningContextCompiler`] wraps another compiler (the shipped
//! [`DefaultContextCompiler`](super::DefaultContextCompiler)) and, once the
//! projected input crosses an absolute threshold, replaces *older* tool
//! results with informative stubs so the model keeps working inside its
//! budget. The transcript is never mutated — only this request's projection
//! changes, and every pruned result keeps its `call_id` plus a recovery
//! instruction, so the model can re-run the call if it needs the content
//! again.
//!
//! Determinism and cache behavior:
//!
//! * Decisions are computed only when the trigger fires, then **frozen** in
//!   a [`PruneCheckpoint`] that the caller passes back in on later turns.
//!   Between triggers the projection is byte-stable, so the provider's
//!   prefix cache holds; each trigger costs one cache miss (ADR-011).
//! * Everything is derived from the session transcript with fixed rules —
//!   no I/O, no clocks, no model calls.
//!
//! Never pruned: user items, assistant messages (text and reasoning), and
//! the protected tail — the last `protected_tail_turns` turns *and* at
//! least `protected_tail_tokens` estimated tokens of trailing items
//! (whichever reaches further back).
//!
//! Budget-exceeded rescue: when the inner compiler's estimate already
//! overruns the budget — the trigger never got the chance to fire below
//! the wall — the wrapper decides stubs immediately and recompiles a
//! stubbed view of the same transcript (same rules, same estimator, same
//! protected tail). A rescue with nothing prunable, or one that is still
//! over budget once fully pruned, fails closed with the original
//! [`ContextError::BudgetExceeded`]. The rescue freezes a checkpoint
//! like any other trigger, so later turns stay byte-stable and the
//! rescue costs exactly one cache miss.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::Message;
use crate::session::Session;
use crate::tool::{PathAccess, ToolRegistry, paths_overlap};
use crate::transcript::{Item, ToolCallId, ToolResult, ToolStatus};

use super::default::{census, serialized_size};
use super::{
    CompiledContext, ContextCompiler, ContextError, ContextInput, HeuristicEstimator,
    PruneCheckpoint, TokenEstimator, estimate_serialized,
};

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
        }
    }
}

/// Why a tool result was stubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StubReason {
    /// A later identical read-only call supersedes this one.
    DuplicateRead,
    /// A later recorded write touched a path this call read.
    SupersededRead,
    /// An error result older than `stale_error_turns`.
    StaleError,
    /// A large read-only result aged out of the unprotected region.
    AgedOut,
    /// A read-only result older than `age_mask_turns` (turn-age
    /// masking, independent of the window trigger).
    TurnAge,
}

impl StubReason {
    /// The deterministic stub body replacing the pruned content.
    #[must_use]
    pub fn stub_text(self) -> &'static str {
        match self {
            Self::DuplicateRead => {
                "[pruned] duplicate of a later identical read-only call; re-run the tool to recover the content"
            }
            Self::SupersededRead => {
                "[pruned] the file changed after this read; re-run the tool for the current content"
            }
            Self::StaleError => {
                "[pruned] stale error from an earlier turn; re-run the tool for a fresh result"
            }
            Self::AgedOut => {
                "[pruned] aged out by context pruning; re-run the tool to recover the content"
            }
            Self::TurnAge => {
                "[pruned] read-only result older than the kept turn window; re-run the tool to recover the content"
            }
        }
    }
}

/// One frozen stub decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubDecision {
    /// The call whose result is stubbed.
    pub call_id: String,
    /// Why — decides the stub text.
    pub reason: StubReason,
}

/// Which arm froze a [`PruneCheckpoint`] (and produced its
/// [`PruneReport`]) — the attribution dimension for `loom stats` and
/// trace consumers (item 20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneTrigger {
    /// The estimated input crossed the window-relative trigger.
    Window,
    /// Turn-age masking decided below the window trigger (roadmap
    /// item 10).
    AgeMask,
    /// The budget-exceeded rescue: the unpruned projection could not
    /// fit, so the wrapper decided stubs ahead of any trigger.
    Rescue,
}

/// One stub decision paired with its estimated reclaim — the unit of
/// per-mechanism attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedCall {
    /// The call whose result was stubbed.
    pub call_id: String,
    /// Why — the mechanism that took it.
    pub reason: StubReason,
    /// Estimated tokens the stub reclaimed versus the full result.
    pub reclaimed_tokens: u64,
}

/// Attribution for one checkpoint freeze, reported on the event stream
/// as `ContextPruned` (event schema v3) exactly once — at the decision
/// moment. Later compiles that merely re-apply the frozen stubs report
/// nothing; summing `reclaimed_tokens` across `ContextPruned` events
/// (deduplicating by `call_id`) attributes savings per mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    /// The arm that fired.
    pub trigger: PruneTrigger,
    /// Per-call attribution, in transcript order.
    pub calls: Vec<PrunedCall>,
    /// Sum of `calls`' reclaims, precomputed for cheap consumers.
    pub reclaimed_tokens: u64,
}

/// A [`ContextCompiler`] that prunes projected tool results once the
/// estimated input crosses the window-relative trigger
/// ([`PruneConfig::trigger_tokens`]): `trigger_percent` of the model's input
/// budget, floored at `floor_tokens`.
pub struct PruningContextCompiler {
    inner: Box<dyn ContextCompiler>,
    config: PruneConfig,
    estimator: Arc<dyn TokenEstimator>,
}

impl PruningContextCompiler {
    /// Wraps `inner` (the shipped
    /// [`DefaultContextCompiler`](super::DefaultContextCompiler)) with the
    /// given thresholds, estimating with the kernel's conservative
    /// bytes ÷ 3 heuristic.
    #[must_use]
    pub fn new(inner: Box<dyn ContextCompiler>, config: PruneConfig) -> Self {
        Self {
            inner,
            config,
            estimator: Arc::new(HeuristicEstimator),
        }
    }

    /// Uses `estimator` for the reclaim and protected-tail math, so the
    /// wrapper decides in the same token space as the census that feeds
    /// it. Inject the same estimator the inner compiler uses.
    #[must_use]
    pub fn with_estimator(mut self, estimator: Arc<dyn TokenEstimator>) -> Self {
        self.estimator = estimator;
        self
    }
}

impl ContextCompiler for PruningContextCompiler {
    fn compile(&self, input: ContextInput<'_>) -> Result<CompiledContext, ContextError> {
        // The inner compiler takes `input` by value, so destructure it once
        // and keep every field the wrapper still needs. These are references
        // and `Copy` values — no transcript is cloned on the normal path.
        let ContextInput {
            session,
            tools,
            task,
            budget,
            capabilities,
            model,
            config,
            facts,
            prune_checkpoint,
        } = input;
        let facts_text = facts.render();
        let mut checkpoint = prune_checkpoint.cloned();
        // Attribution for a checkpoint frozen by *this* compile, when one
        // was; re-applications of an earlier freeze report nothing.
        let mut report: Option<PruneReport> = None;
        // Call ids whose projected body this compile stubbed, reported so
        // the caller never mistakes a stub for still-visible content.
        let mut stubbed_calls: Vec<String> = Vec::new();
        let mut compiled = match self.inner.compile(ContextInput {
            session,
            tools,
            task,
            budget,
            capabilities,
            model,
            config,
            facts,
            prune_checkpoint,
        }) {
            Ok(compiled) => compiled,
            // Budget-exceeded rescue: the unpruned projection cannot fit,
            // so the wrapper decides stubs now — ahead of any trigger —
            // and recompiles a stubbed view of the same transcript. The
            // reclaim gate does not apply: the budget is already blown, so
            // cache-churn economics are moot and any prunable result is
            // worth taking. Fails closed when nothing is prunable or the
            // pruned view still does not fit.
            Err(overrun @ ContextError::BudgetExceeded { .. }) => {
                let (decisions, _reclaim) =
                    decide(session, tools, self.config, &*self.estimator, true);
                if decisions.is_empty() {
                    return Err(overrun);
                }
                let stubbed = stubbed_session(session, &decisions);
                match self.inner.compile(ContextInput {
                    session: &stubbed,
                    tools,
                    task,
                    budget,
                    capabilities,
                    model,
                    config,
                    facts,
                    prune_checkpoint,
                }) {
                    Ok(compiled) => {
                        // The stubs are already baked into the stubbed
                        // session the inner compiler projected, so record
                        // the decisions verbatim.
                        stubbed_calls.extend(decisions.iter().map(|d| d.call_id.clone()));
                        report = Some(prune_report(
                            PruneTrigger::Rescue,
                            session,
                            &decisions,
                            &*self.estimator,
                        ));
                        checkpoint = Some(PruneCheckpoint {
                            upto_item: session.transcript.len(),
                            stubs: decisions,
                        });
                        compiled
                    }
                    // Even fully pruned it does not fit: the budget is
                    // genuinely blown; surface the original overrun.
                    Err(_) => return Err(overrun),
                }
            }
            Err(other) => return Err(other),
        };

        // `census` needs the facts-message size the inner projection used;
        // recompute it the same way `project_messages` does.
        let facts_tokens = if facts_text.is_empty() {
            0
        } else {
            estimate_serialized(&*self.estimator, &Message::System { text: facts_text })
        };

        // Apply the frozen decisions first: between triggers the projection
        // must stay byte-stable (ADR-011).
        if let Some(frozen) = checkpoint
            .as_ref()
            .map(|c| c.stubs.clone())
            .filter(|stubs| !stubs.is_empty())
        {
            stubbed_calls.extend(apply_stubs(&mut compiled.request, &frozen));
            compiled.census = census(&compiled.request, facts_tokens, &*self.estimator);
        }

        // Window-relative trigger: recompute decisions over the whole
        // transcript once the estimate nears the model's input budget.
        //
        // ADR-011: pruning is episodic — one cache miss per checkpoint, never
        // per turn. A checkpoint that already covers the transcript is only
        // superseded when the new pass reclaims a meaningful amount *beyond*
        // what the frozen stubs already reclaim; otherwise a session parked
        // above the trigger would re-decide (and rewrite earlier projected
        // messages) on every turn.
        //
        // Turn-age masking (roadmap item 10) adds a second trigger arm:
        // when configured, the decision pass also runs *below* the window
        // trigger so old read-only results are masked as they age out. The
        // same reclaim gate applies — an aging batch is only worth a cache
        // miss when it reclaims at least `min_reclaim_tokens` beyond the
        // frozen checkpoint — so the projection stays byte-stable between
        // aging batches.
        let trigger = self.config.trigger_tokens(budget.max_input_tokens);
        let window_triggered = compiled.census.total > trigger;
        let age_masking = self.config.age_mask_turns.is_some();
        if window_triggered || age_masking {
            let (decisions, reclaim) = decide(
                session,
                tools,
                self.config,
                &*self.estimator,
                window_triggered,
            );
            let already = checkpoint.as_ref().map_or(0, |frozen| {
                frozen_reclaim(session, &frozen.stubs, &*self.estimator)
            });
            if reclaim >= already.saturating_add(u64::from(self.config.min_reclaim_tokens)) {
                stubbed_calls.extend(apply_stubs(&mut compiled.request, &decisions));
                compiled.census = census(&compiled.request, facts_tokens, &*self.estimator);
                report = Some(prune_report(
                    if window_triggered {
                        PruneTrigger::Window
                    } else {
                        PruneTrigger::AgeMask
                    },
                    session,
                    &decisions,
                    &*self.estimator,
                ));
                checkpoint = Some(PruneCheckpoint {
                    upto_item: session.transcript.len(),
                    stubs: decisions,
                });
            }
        }

        // Budget still applies after pruning.
        if compiled.census.total > budget.max_input_tokens {
            return Err(ContextError::BudgetExceeded {
                estimated: compiled.census.total,
                max_input_tokens: budget.max_input_tokens,
            });
        }

        compiled.prune_checkpoint = checkpoint;
        compiled.prune_report = report;
        compiled.stubbed_calls = stubbed_calls;
        Ok(compiled)
    }
}

/// Rewrites projected `ToolResult` messages whose `call_id` is stubbed,
/// leaving every other message (and the call/result pairing) untouched.
///
/// Returns the call ids whose bodies were actually rewritten, so the
/// caller can report them as [`CompiledContext::stubbed_calls`]: a stub
/// body is not content the model can still be pointed at.
fn apply_stubs(request: &mut crate::model::ModelRequest, stubs: &[StubDecision]) -> Vec<String> {
    let by_call: HashMap<&str, StubReason> = stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let mut rewritten = Vec::new();
    for message in &mut request.messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        let Some(reason) = by_call.get(result.call_id.as_str()) else {
            continue;
        };
        result.content = reason.stub_text().to_owned();
        result.truncated = false;
        rewritten.push(result.call_id.as_str().to_owned());
    }
    rewritten
}

/// Clones `session` with the stubbed results rewritten in place, as the
/// private view the budget-exceeded rescue recompiles. The durable
/// transcript is never mutated — only this copy's projection changes,
/// mirroring [`apply_stubs`] item for item.
fn stubbed_session(session: &Session, stubs: &[StubDecision]) -> Session {
    let by_call: HashMap<&str, StubReason> = stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let mut stubbed = session.clone();
    for item in &mut stubbed.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        let Some(reason) = by_call.get(result.call_id.as_str()) else {
            continue;
        };
        result.content = reason.stub_text().to_owned();
        result.truncated = false;
    }
    stubbed
}

/// Tokens the already-frozen stubs reclaim from the *current* transcript, so
/// `compile` can gate a fresh checkpoint on the reclaim that is genuinely new
/// (ADR-011: pruning is episodic — one cache miss per checkpoint).
fn frozen_reclaim(
    session: &Session,
    stubs: &[StubDecision],
    estimator: &dyn TokenEstimator,
) -> u64 {
    let by_call: HashMap<&str, StubReason> = stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let mut reclaim = 0u64;
    for item in &session.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        let Some(reason) = by_call.get(result.call_id.as_str()) else {
            continue;
        };
        reclaim += u64::from(
            estimate_serialized(estimator, item).saturating_sub(stub_tokens(
                &result.call_id,
                &result.status,
                *reason,
                estimator,
            )),
        );
    }
    reclaim
}

/// Builds the freeze-time attribution for `stubs` against `session`'s
/// transcript: per-call estimated reclaim (full item less the stub body)
/// plus the arm that fired.
fn prune_report(
    trigger: PruneTrigger,
    session: &Session,
    stubs: &[StubDecision],
    estimator: &dyn TokenEstimator,
) -> PruneReport {
    let by_call: HashMap<&str, StubReason> = stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let mut calls = Vec::new();
    for item in &session.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        let Some(&reason) = by_call.get(result.call_id.as_str()) else {
            continue;
        };
        let full = estimate_serialized(estimator, item);
        let stub = stub_tokens(&result.call_id, &result.status, reason, estimator);
        calls.push(PrunedCall {
            call_id: result.call_id.as_str().to_owned(),
            reason,
            reclaimed_tokens: u64::from(full.saturating_sub(stub)),
        });
    }
    let reclaimed_tokens = calls.iter().map(|call| call.reclaimed_tokens).sum();
    PruneReport {
        trigger,
        calls,
        reclaimed_tokens,
    }
}

/// Facts gathered from the transcript for the decision pass.
struct ResultFacts {
    index: usize,
    turn: usize,
    call_id: ToolCallId,
    status: ToolStatus,
    /// Serialized byte size — ordering only (the age-out sort).
    bytes: u64,
    /// Estimated tokens of the serialized item — the reclaim math.
    tokens: u32,
    /// Tool name + canonical arguments, when the call is known and
    /// read-only; `None` otherwise (unknown or mutating — never a dedupe,
    /// supersede or age-out candidate).
    read_call: Option<(String, String, Vec<String>)>,
}

/// Computes the stub set from the transcript under ADR-011's fixed
/// strategy order, plus the reclaim estimate (in tokens) against the
/// *unpruned* projection.
fn decide(
    session: &Session,
    registry: &ToolRegistry,
    config: PruneConfig,
    estimator: &dyn TokenEstimator,
    window_triggered: bool,
) -> (Vec<StubDecision>, u64) {
    let transcript: &[Item] = &session.transcript;

    // Pass 1 — call metadata (name, canonical args, declared reads) from
    // assistant tool_calls, keyed by call id for O(1) lookup in pass 2.
    let mut calls: HashMap<&str, (String, String, Vec<String>)> = HashMap::new();
    for item in transcript {
        if let Item::Assistant(assistant) = item {
            for call in &assistant.tool_calls {
                let reads = read_paths(registry, &call.name, &call.arguments);
                calls.insert(
                    call.id.as_str(),
                    (call.name.clone(), call.arguments.to_string(), reads),
                );
            }
        }
    }
    let is_read_only = |name: &str| registry.get(name).is_some_and(|tool| tool.is_read_only());

    // Pass 2 — tool-result facts, assistant turn boundaries, later writes.
    let mut assistant_at: Vec<usize> = Vec::new();
    let mut results: Vec<ResultFacts> = Vec::new();
    let mut writes: Vec<(usize, Vec<String>)> = Vec::new();
    let mut turn = 0usize;
    for (index, item) in transcript.iter().enumerate() {
        match item {
            Item::Assistant(_) => {
                turn += 1;
                assistant_at.push(index);
            }
            Item::ToolResult(result) => {
                let read_call =
                    calls
                        .get(result.call_id.as_str())
                        .and_then(|(name, args, reads)| {
                            is_read_only(name).then(|| (name.clone(), args.clone(), reads.clone()))
                        });
                if !result.effects.files_written.is_empty()
                    || !result.effects.files_deleted.is_empty()
                {
                    let mut changed = result.effects.files_written.clone();
                    changed.extend(result.effects.files_deleted.iter().cloned());
                    writes.push((index, changed));
                }
                results.push(ResultFacts {
                    index,
                    turn,
                    call_id: result.call_id.clone(),
                    status: result.status,
                    bytes: serialized_size(item),
                    tokens: estimate_serialized(estimator, item),
                    read_call,
                });
            }
            Item::User { .. } => {}
        }
    }

    // Protected tail: last `protected_tail_turns` turns AND trailing
    // `protected_tail_tokens` tokens — whichever reaches further back.
    let turn_cut = assistant_at
        .len()
        .checked_sub(config.protected_tail_turns)
        .and_then(|from| assistant_at.get(from))
        .copied()
        .unwrap_or(0);
    let mut trailing = 0u64;
    let mut token_cut = transcript.len();
    for (index, item) in transcript.iter().enumerate().rev() {
        trailing += u64::from(estimate_serialized(estimator, item));
        if trailing >= u64::from(config.protected_tail_tokens) {
            token_cut = index;
            break;
        }
    }
    let protected_from = turn_cut.min(token_cut);

    let total_turns = assistant_at.len();
    let mut stubbed: Vec<StubDecision> = Vec::new();
    let mut stubbed_ids: HashSet<&str> = HashSet::new();
    let mut reclaim_tokens = 0u64;

    fn add_stub<'a>(
        facts: &'a ResultFacts,
        reason: StubReason,
        stubbed: &mut Vec<StubDecision>,
        stubbed_ids: &mut HashSet<&'a str>,
        reclaim_tokens: &mut u64,
        estimator: &dyn TokenEstimator,
    ) {
        *reclaim_tokens += u64::from(facts.tokens.saturating_sub(stub_tokens(
            &facts.call_id,
            &facts.status,
            reason,
            estimator,
        )));
        stubbed_ids.insert(facts.call_id.as_str());
        stubbed.push(StubDecision {
            call_id: facts.call_id.as_str().to_owned(),
            reason,
        });
    }

    // Index of the newest call per (name, canonical args): one pass instead
    // of scanning every later result per candidate.
    let mut newest_by_call: HashMap<(&str, &str), usize> = HashMap::new();
    for facts in &results {
        let Some((name, args, _)) = facts.read_call.as_ref() else {
            continue;
        };
        newest_by_call
            .entry((name.as_str(), args.as_str()))
            .and_modify(|index| *index = (*index).max(facts.index))
            .or_insert(facts.index);
    }

    // (1) Dedupe identical read-only calls, keeping the newest.
    for earlier in &results {
        if earlier.index >= protected_from {
            continue;
        }
        let Some((name, args, _)) = earlier.read_call.as_ref() else {
            continue;
        };
        let is_newest = newest_by_call
            .get(&(name.as_str(), args.as_str()))
            .is_some_and(|index| *index == earlier.index);
        if !is_newest {
            add_stub(
                earlier,
                StubReason::DuplicateRead,
                &mut stubbed,
                &mut stubbed_ids,
                &mut reclaim_tokens,
                estimator,
            );
        }
    }

    // (2) Supersede reads of later-modified paths.
    for earlier in &results {
        if earlier.index >= protected_from || stubbed_ids.contains(earlier.call_id.as_str()) {
            continue;
        }
        let Some((_, _, reads)) = earlier.read_call.as_ref() else {
            continue;
        };
        let superseded = writes.iter().any(|(index, changed)| {
            *index > earlier.index
                && reads
                    .iter()
                    .any(|read| changed.iter().any(|path| paths_overlap(read, path)))
        });
        if superseded {
            add_stub(
                earlier,
                StubReason::SupersededRead,
                &mut stubbed,
                &mut stubbed_ids,
                &mut reclaim_tokens,
                estimator,
            );
        }
    }

    // (3) Stale errors.
    for earlier in &results {
        if earlier.index >= protected_from || stubbed_ids.contains(earlier.call_id.as_str()) {
            continue;
        }
        if earlier.status != ToolStatus::Success
            && total_turns.saturating_sub(earlier.turn) > config.stale_error_turns
        {
            add_stub(
                earlier,
                StubReason::StaleError,
                &mut stubbed,
                &mut stubbed_ids,
                &mut reclaim_tokens,
                estimator,
            );
        }
    }

    // (4) Age out large read-only results outside the protected tail.
    // `min_reclaim_tokens` is a *gate* on whether the prune runs at all
    // (checked in `compile`), never a cap: every unprotected read-only
    // result is stubbed, so the projection cannot grow without bound once
    // the trigger is crossed. Sorted largest-first for a deterministic,
    // size-prioritised order. Window-triggered (or budget-rescue) passes
    // only: turn-age masking alone must not nuke the whole history.
    let mut aged: Vec<&ResultFacts> = results
        .iter()
        .filter(|facts| {
            window_triggered
                && facts.index < protected_from
                && facts.status == ToolStatus::Success
                && facts.read_call.is_some()
                && !stubbed_ids.contains(facts.call_id.as_str())
        })
        .collect();
    aged.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.index.cmp(&b.index)));
    for facts in aged {
        add_stub(
            facts,
            StubReason::AgedOut,
            &mut stubbed,
            &mut stubbed_ids,
            &mut reclaim_tokens,
            estimator,
        );
    }

    // (5) Turn-age masking: stub successful read-only results older than
    // `age_mask_turns` assistant turns, independent of the window
    // trigger — the transcript replay is capped at a rolling window, so
    // a long task's request size plateaus instead of growing without
    // bound. Runs after the other passes so already-stubbed calls keep
    // their more specific reasons; the protected tail always wins.
    if let Some(max_age) = config.age_mask_turns {
        for earlier in &results {
            if earlier.index >= protected_from || stubbed_ids.contains(earlier.call_id.as_str()) {
                continue;
            }
            if earlier.status == ToolStatus::Success
                && earlier.read_call.is_some()
                && total_turns.saturating_sub(earlier.turn) > max_age
            {
                add_stub(
                    earlier,
                    StubReason::TurnAge,
                    &mut stubbed,
                    &mut stubbed_ids,
                    &mut reclaim_tokens,
                    estimator,
                );
            }
        }
    }

    (stubbed, reclaim_tokens)
}

/// Estimated tokens of the stub that would replace this result.
fn stub_tokens(
    call_id: &ToolCallId,
    status: &ToolStatus,
    reason: StubReason,
    estimator: &dyn TokenEstimator,
) -> u32 {
    let probe = Item::ToolResult(ToolResult {
        call_id: call_id.clone(),
        status: *status,
        content: reason.stub_text().to_owned(),
        truncated: false,
        effects: crate::tool::ToolEffects::none(),
        artifact: None,
    });
    estimate_serialized(estimator, &probe)
}

/// The declared read paths of a call, when the tool is registered,
/// read-only, and describes its paths.
fn read_paths(registry: &ToolRegistry, name: &str, arguments: &serde_json::Value) -> Vec<String> {
    let Some(tool) = registry.get(name) else {
        return Vec::new();
    };
    tool.touched_paths(arguments)
        .unwrap_or_default()
        .into_iter()
        .filter(|touch| touch.access == PathAccess::Read)
        .map(|touch| touch.path)
        .collect()
}

// `ContextCensus` is reconstructed via `census` in `compile`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{
        AgentConfig, ContextBudget, ContextInput, DefaultContextCompiler, messages_serialized,
    };
    use crate::model::{ModelCapabilities, StopReason};
    use crate::session::{Session, TaskRecord};
    use crate::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId};
    use serde_json::json;

    fn config() -> AgentConfig {
        AgentConfig::new(std::path::PathBuf::from("/w"), "system")
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities::new(500_000)
            .with_tool_calls(true)
            .with_max_output_tokens(4_096)
    }

    fn budget() -> ContextBudget {
        ContextBudget {
            max_input_tokens: 400_000,
            reserved_output_tokens: 4_096,
        }
    }

    fn registry() -> ToolRegistry {
        // A read-only tool declaring its path, and a mutating one.
        struct ReadTool;
        impl crate::tool::Tool for ReadTool {
            fn spec(&self) -> &crate::tool::ToolSpec {
                static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> =
                    std::sync::OnceLock::new();
                SPEC.get_or_init(|| crate::tool::ToolSpec {
                    name: "read_file".to_owned(),
                    description: "Reads a file.".to_owned(),
                    parameters: serde_json::json!({"type": "object"}),
                })
            }
            fn is_read_only(&self) -> bool {
                true
            }
            fn touched_paths(
                &self,
                input: &serde_json::Value,
            ) -> Option<Vec<crate::tool::PathTouch>> {
                Some(vec![crate::tool::PathTouch::read(
                    input.get("path").and_then(|v| v.as_str()).unwrap_or("x"),
                )])
            }
            fn call<'a>(
                &'a self,
                _input: serde_json::Value,
                _ctx: &'a crate::tool::ToolContext,
            ) -> futures::future::BoxFuture<
                'a,
                Result<crate::tool::ToolOutput, crate::tool::ToolError>,
            > {
                unreachable!("call not needed for pruning tests")
            }
        }
        struct WriteTool;
        impl crate::tool::Tool for WriteTool {
            fn spec(&self) -> &crate::tool::ToolSpec {
                static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> =
                    std::sync::OnceLock::new();
                SPEC.get_or_init(|| crate::tool::ToolSpec {
                    name: "write_file".to_owned(),
                    description: "Writes a file.".to_owned(),
                    parameters: serde_json::json!({"type": "object"}),
                })
            }
            fn is_read_only(&self) -> bool {
                false
            }
            fn call<'a>(
                &'a self,
                _input: serde_json::Value,
                _ctx: &'a crate::tool::ToolContext,
            ) -> futures::future::BoxFuture<
                'a,
                Result<crate::tool::ToolOutput, crate::tool::ToolError>,
            > {
                unreachable!("call not needed for pruning tests")
            }
        }
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(ReadTool)).expect("register");
        registry.register(Box::new(WriteTool)).expect("register");
        registry
    }

    fn session_with(items: Vec<Item>) -> Session {
        let mut session = Session::new(crate::id::SessionId::new());
        for item in items {
            session.transcript.push(item);
        }
        session
    }

    fn assistant_text(content: &str) -> Item {
        Item::Assistant(AssistantMessage {
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        })
    }

    fn assistant_calls(content: &str, calls: Vec<ToolCall>) -> Item {
        Item::Assistant(AssistantMessage {
            content: content.to_owned(),
            reasoning: None,
            tool_calls: calls,
            stop_reason: StopReason::ToolCalls,
        })
    }

    fn read_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).expect("call id"),
            name: "read_file".to_owned(),
            arguments: ToolArguments::try_from(json!({ "path": path })).expect("args"),
        }
    }

    fn write_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({ "path": path, "content": "x" }))
                .expect("args"),
        }
    }

    fn result(id: &str, content: &str) -> Item {
        Item::ToolResult(ToolResult::new(
            ToolCallId::try_from(id.to_owned()).expect("call id"),
            ToolStatus::Success,
            content,
            false,
        ))
    }

    fn error_result(id: &str) -> Item {
        Item::ToolResult(ToolResult::new(
            ToolCallId::try_from(id.to_owned()).expect("call id"),
            ToolStatus::Error(crate::transcript::ToolFailure::Execution),
            big("error", 70),
            false,
        ))
    }

    fn written_result(id: &str, path: &str) -> Item {
        let mut result = ToolResult::new(
            ToolCallId::try_from(id.to_owned()).expect("call id"),
            ToolStatus::Success,
            big("written", 25),
            false,
        );
        result.effects = crate::tool::ToolEffects::none().with_file_written(path);
        Item::ToolResult(result)
    }

    fn input<'a>(session: &'a Session, prune: Option<&'a PruneCheckpoint>) -> ContextInput<'a> {
        input_with(session, prune, budget())
    }

    fn input_with<'a>(
        session: &'a Session,
        prune: Option<&'a PruneCheckpoint>,
        budget: ContextBudget,
    ) -> ContextInput<'a> {
        shared();
        static FACTS: std::sync::OnceLock<crate::context::KnownFacts> = std::sync::OnceLock::new();
        let facts = FACTS.get_or_init(crate::context::KnownFacts::default);
        ContextInput {
            session,
            tools: READ_REGISTRY.get().expect("registry"),
            task: TASK.get().expect("task"),
            budget,
            capabilities: CAPABILITIES.get().expect("capabilities"),
            model: "deepseek-flash",
            config: AGENT_CONFIG.get().expect("config"),
            facts,
            prune_checkpoint: prune,
        }
    }

    static READ_REGISTRY: std::sync::OnceLock<ToolRegistry> = std::sync::OnceLock::new();
    static TASK: std::sync::OnceLock<TaskRecord> = std::sync::OnceLock::new();
    static CAPABILITIES: std::sync::OnceLock<ModelCapabilities> = std::sync::OnceLock::new();
    static AGENT_CONFIG: std::sync::OnceLock<AgentConfig> = std::sync::OnceLock::new();

    fn shared() {
        READ_REGISTRY.get_or_init(registry);
        TASK.get_or_init(|| TaskRecord::new(crate::id::TaskId::new(), 0));
        CAPABILITIES.get_or_init(capabilities);
        AGENT_CONFIG.get_or_init(config);
    }

    fn compiler(config: PruneConfig) -> PruningContextCompiler {
        shared();
        PruningContextCompiler::new(Box::new(DefaultContextCompiler::default()), config)
    }

    /// A config that fires at the old flat threshold, so the strategy tests
    /// keep their shape regardless of the window-relative default (the 400 K
    /// test budget would otherwise push the default trigger to 320 K).
    fn firing() -> PruneConfig {
        PruneConfig {
            trigger_percent: 0,
            floor_tokens: 64_000,
            ..PruneConfig::default()
        }
    }

    fn big(prefix: &str, kb: usize) -> String {
        prefix.to_owned() + &"x".repeat(kb * 1024)
    }

    #[test]
    fn below_trigger_no_stubs_and_no_checkpoint() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 30)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(compiled.census.total < 64_000);
    }

    /// A 100 KiB read at turn 1, 60 KiB of later history, nine filler
    /// turns: the read is older than `age_mask_turns` (default 8), sits
    /// outside the default 20 K-token protected tail (the later bulk
    /// carries the tail past it), and its ~33 K estimated tokens clear
    /// the reclaim gate — masked *below* the window trigger.
    #[test]
    fn old_reads_are_masked_below_the_window_trigger() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 100)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_text("filler three"),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 60)),
        ];
        for turn in 4..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("age mask fired");
        assert_eq!(
            checkpoint.stubs,
            vec![StubDecision {
                call_id: "c1".to_owned(),
                reason: StubReason::TurnAge,
            }]
        );
        assert!(
            messages_serialized(&compiled.request.messages)
                .contains("older than the kept turn window")
        );
        // The rewritten ids are reported so callers never mistake a stub
        // for content the model can still be pointed at (the item-6
        // cache stub consults this).
        assert_eq!(compiled.stubbed_calls, vec!["c1".to_owned()]);
    }

    #[test]
    fn a_compile_below_every_trigger_reports_no_stubbed_calls() {
        let session = session_with(vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c3", "src/c.rs")]),
            result("c3", &big("content", 5)),
        ]);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(compiled.stubbed_calls.is_empty());
    }

    /// A tiny old read reclaims less than `min_reclaim_tokens`, so the
    /// aging batch is not worth a cache miss: no checkpoint, full
    /// content, byte-stable projection.
    #[test]
    fn turn_age_masking_respects_the_reclaim_gate() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 5)),
        ];
        for turn in 1..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(messages_serialized(&compiled.request.messages).contains("contentxxxx"));
    }

    /// A tight age window (2) with the default tails: the turn-2 read
    /// masks (older than 2 turns, outside both tails thanks to the later
    /// bulk); the turn-9 bulk read stays full (inside the protected
    /// tail).
    #[test]
    fn turn_age_masking_leaves_the_protected_tail_alone() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_text("warmup"),
            assistant_calls("old read", vec![read_call("c5", "src/old.rs")]),
            result("c5", &big("old", 100)),
            assistant_text("four"),
            assistant_text("five"),
            assistant_text("six"),
            assistant_text("seven"),
            assistant_calls("tail bulk", vec![read_call("c9", "src/tail.rs")]),
            result("c9", &big("tail", 60)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(PruneConfig {
            age_mask_turns: Some(2),
            ..PruneConfig::default()
        })
        .compile(input(&session, None))
        .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("age mask fired");
        assert_eq!(
            checkpoint.stubs,
            vec![StubDecision {
                call_id: "c5".to_owned(),
                reason: StubReason::TurnAge,
            }]
        );
        assert!(messages_serialized(&compiled.request.messages).contains("tailxxxx"));
    }

    /// `age_mask_turns: None` opts out entirely: an old large read below
    /// the window trigger keeps its full content and no checkpoint forms.
    #[test]
    fn turn_age_masking_off_when_none() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 100)),
        ];
        for turn in 1..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items);
        let compiled = compiler(PruneConfig {
            age_mask_turns: None,
            ..PruneConfig::default()
        })
        .compile(input(&session, None))
        .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(messages_serialized(&compiled.request.messages).contains("contentxxxx"));
    }

    /// Feeding the checkpoint back keeps the projection byte-stable: the
    /// aging decision is frozen, not recomputed per turn.
    #[test]
    fn an_age_mask_checkpoint_freezes_the_projection() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 100)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_text("filler three"),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 60)),
        ];
        for turn in 4..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items.clone());
        let first = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        items.push(assistant_text("one more turn"));
        let session = session_with(items);
        let second = compiler(PruneConfig::default())
            .compile(input(&session, Some(&checkpoint)))
            .expect("compiles");
        // The frozen stub still applies and no new decision churned the
        // older history: the projected prefix is unchanged.
        assert!(
            messages_serialized(&second.request.messages)
                .contains("older than the kept turn window")
        );
        assert_eq!(second.prune_checkpoint.as_ref(), Some(&checkpoint));
    }

    /// Item 20.1 attribution: the freeze moment reports the arm, the
    /// per-call mechanism, and the estimated reclaim; a later compile
    /// that only re-applies the frozen stubs reports nothing (sums
    /// across events must not double-count).
    #[test]
    fn an_age_mask_freeze_reports_per_call_attribution_once() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 100)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_text("filler three"),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 60)),
        ];
        for turn in 4..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items.clone());
        let first = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let report = first.prune_report.as_ref().expect("freeze reported");
        assert_eq!(report.trigger, PruneTrigger::AgeMask);
        assert_eq!(
            report.calls,
            vec![PrunedCall {
                call_id: "c1".to_owned(),
                reason: StubReason::TurnAge,
                reclaimed_tokens: report.reclaimed_tokens,
            }]
        );
        assert!(report.reclaimed_tokens > 0);
        // The next turn re-applies the frozen stub: still stubbed, but
        // no new attribution.
        items.push(assistant_text("one more turn"));
        let session = session_with(items);
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        let second = compiler(PruneConfig::default())
            .compile(input(&session, Some(&checkpoint)))
            .expect("compiles");
        assert!(second.prune_report.is_none());
    }

    /// A window-triggered freeze reports `Window` even when turn-age
    /// masking is also configured (the window arm is the primary cause).
    #[test]
    fn a_window_freeze_reports_the_window_trigger() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 120)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 80)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let report = compiled.prune_report.as_ref().expect("freeze reported");
        assert_eq!(report.trigger, PruneTrigger::Window);
        assert!(
            report
                .calls
                .iter()
                .any(|call| call.call_id == "c1" && call.reclaimed_tokens > 0)
        );
        assert_eq!(
            report.reclaimed_tokens,
            report
                .calls
                .iter()
                .map(|call| call.reclaimed_tokens)
                .sum::<u64>()
        );
    }

    /// The budget-exceeded rescue attributes like any other freeze, with
    /// its own trigger arm.
    #[test]
    fn a_rescue_reports_the_rescue_trigger() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 300)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_calls("reading again", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("tail", 70)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input_with(&session, None, tight()))
            .expect("rescued compile");
        let report = compiled.prune_report.as_ref().expect("rescue reported");
        assert_eq!(report.trigger, PruneTrigger::Rescue);
        assert!(report.calls.iter().any(|call| call.call_id == "c1"));
        assert!(!report.calls.iter().any(|call| call.call_id == "c2"));
        assert!(report.reclaimed_tokens > 0);
    }

    /// A budget tight enough that the *inner* compiler refuses the
    /// unpruned projection — the situation the rescue exists for, where
    /// the trigger never got the chance to fire below the wall.
    fn tight() -> ContextBudget {
        ContextBudget {
            max_input_tokens: 60_000,
            reserved_output_tokens: 4_096,
        }
    }

    #[test]
    fn an_inner_budget_overrun_is_rescued_by_a_forced_prune() {
        // 300 KiB of tool result ≈ 100 K estimated tokens against the 60 K
        // budget; the old read sits far outside the protected tail.
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 300)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_calls("reading again", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("tail", 70)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input_with(&session, None, tight()))
            .expect("rescued compile");
        assert!(compiled.census.total <= 60_000);
        let checkpoint = compiled
            .prune_checkpoint
            .expect("rescue froze a checkpoint");
        assert!(checkpoint.stubs.iter().any(|stub| stub.call_id == "c1"));
        assert!(!checkpoint.stubs.iter().any(|stub| stub.call_id == "c2"));
        assert!(messages_contain(
            &compiled.request,
            StubReason::AgedOut.stub_text()
        ));
    }

    #[test]
    fn a_rescue_without_prunable_results_fails_closed() {
        // User items are never prunable: over budget with nothing to stub
        // must surface the inner error, never an empty success.
        let items = vec![Item::User {
            text: big("question", 300),
        }];
        let session = session_with(items);
        let error = compiler(PruneConfig::default())
            .compile(input_with(&session, None, tight()))
            .expect_err("over budget");
        assert!(matches!(error, ContextError::BudgetExceeded { .. }));
    }

    #[test]
    fn a_rescued_session_stays_stubbed_and_byte_stable() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 300)),
            assistant_text("filler one"),
            assistant_text("filler two"),
            assistant_calls("reading again", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("tail", 70)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let first = compiler(PruneConfig::default())
            .compile(input_with(&session, None, tight()))
            .expect("rescued compile");
        let checkpoint = first.prune_checkpoint.clone().expect("frozen");
        let second = compiler(PruneConfig::default())
            .compile(input_with(&session, Some(&checkpoint), tight()))
            .expect("frozen compile");
        assert!(messages_contain(
            &second.request,
            StubReason::AgedOut.stub_text()
        ));
        // Byte-stability between events: passing the frozen checkpoint
        // back must reproduce the rescued request exactly (ADR-011).
        assert_eq!(
            serde_json::to_string(&first.request).expect("serialize"),
            serde_json::to_string(&second.request).expect("serialize")
        );
    }

    #[test]
    fn duplicate_readonly_calls_are_deduped_keeping_newest() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..8 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("old{n}"), "src/a.rs")],
            ));
            items.push(result(&format!("old{n}"), &big("dup", 25)));
        }
        // The newest duplicate sits inside the protected tail.
        items.push(assistant_calls(
            "reading again",
            vec![read_call("new", "src/a.rs")],
        ));
        items.push(result("new", &big("dup", 25)));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "old0" && stub.reason == StubReason::DuplicateRead)
        );
        assert!(!checkpoint.stubs.iter().any(|stub| stub.call_id == "new"));
        // The projected content is the stub text.
        assert!(messages_contain(
            &compiled.request,
            StubReason::DuplicateRead.stub_text()
        ));
    }

    #[test]
    fn reads_superseded_by_later_writes_are_stubbed() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("read", vec![read_call("r1", "src/a.rs")]),
            result("r1", &big("read", 70)),
        ];
        for n in 0..8 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        // A write to the read path, still outside the protected tail.
        items.push(assistant_calls(
            "write target",
            vec![write_call("wt", "src/a.rs")],
        ));
        items.push(written_result("wt", "src/a.rs"));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "r1" && stub.reason == StubReason::SupersededRead)
        );
    }

    #[test]
    fn stale_errors_are_stubbed_after_four_turns() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls(
                "try",
                vec![ToolCall {
                    id: ToolCallId::try_from("e1".to_owned()).expect("call id"),
                    name: "write_file".to_owned(),
                    arguments: ToolArguments::try_from(
                        json!({ "path": "src/a.rs", "content": "x" }),
                    )
                    .expect("args"),
                }],
            ),
            error_result("e1"),
        ];
        for n in 0..8 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "e1" && stub.reason == StubReason::StaleError)
        );
    }

    #[test]
    fn protected_tail_resists_ageout() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        // Old, large, unique read — an age-out candidate.
        items.push(assistant_calls(
            "read",
            vec![read_call("old", "src/old.rs")],
        ));
        items.push(result("old", &big("old", 70)));
        // Enough later bulk to push the total over the trigger while the
        // protected tail covers only the recent turns.
        for n in 0..6 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        // A recent large read inside the tail — must survive.
        items.push(assistant_calls(
            "read recent",
            vec![read_call("recent", "src/new.rs")],
        ));
        items.push(result("recent", &big("recent", 70)));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "old" && stub.reason == StubReason::AgedOut)
        );
        assert!(!checkpoint.stubs.iter().any(|stub| stub.call_id == "recent"));
    }

    #[test]
    fn min_reclaim_gate_blocks_small_prunes() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("read", vec![read_call("r1", "src/a.rs")]),
            result("r1", "small"),
            assistant_text("done"),
        ];
        let session = session_with(items);
        // A trigger low enough to fire but a reclaim floor the small
        // transcript cannot meet.
        let config = PruneConfig {
            trigger_percent: 0,
            floor_tokens: 10,
            min_reclaim_tokens: 20_000,
            ..PruneConfig::default()
        };
        let compiled = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
    }

    #[test]
    fn frozen_stubs_stay_byte_stable_across_appends() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..8 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("old{n}"), "src/a.rs")],
            ));
            items.push(result(&format!("old{n}"), &big("dup", 25)));
        }
        items.push(assistant_text("done"));
        let session = session_with(items.clone());
        let first = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("triggered");

        // Append turns; pass the checkpoint back in (as the loop does).
        items.push(assistant_calls("more", vec![read_call("late", "src/b.rs")]));
        items.push(result("late", &big("late", 5)));
        items.push(assistant_text("done again"));
        let grown = session_with(items);
        let second = compiler(firing())
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        // Every frozen stub is still applied verbatim.
        for message in &second.request.messages {
            let Message::ToolResult(result) = message else {
                continue;
            };
            let Some(stub) = checkpoint
                .stubs
                .iter()
                .find(|stub| stub.call_id == result.call_id.as_str())
            else {
                continue;
            };
            assert_eq!(result.content, stub.reason.stub_text());
        }
    }

    #[test]
    fn ageout_stubs_every_unprotected_read_not_just_the_floor() {
        // Regression (2L live run): age-out must prune *all* read-only
        // results outside the protected tail, not merely enough to clear
        // `min_reclaim_tokens`. Stopping at the floor re-stubbed the same
        // oldest reads every trigger while newer bulk stayed, so the
        // projection grew without bound.
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..10 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("r{n}"), &format!("src/f{n}.rs"))],
            ));
            items.push(result(&format!("r{n}"), &big("read", 30)));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        let aged = checkpoint
            .stubs
            .iter()
            .filter(|stub| stub.reason == StubReason::AgedOut)
            .count();
        // Ten 30 KB reads: only the protected tail (last few) may survive,
        // and the old floor-limited behaviour would have stubbed ~2.
        assert!(
            aged >= 5,
            "expected most old reads aged out, only {aged} stubbed"
        );
        assert!(
            !checkpoint.stubs.iter().any(|stub| stub.call_id == "r9"),
            "the newest read is inside the protected tail"
        );
    }

    #[test]
    fn trigger_is_window_relative_with_a_floor() {
        let config = PruneConfig::default();
        // 80 % of a 1M-token window is far above the 64K floor, so pruning
        // stays dormant for ordinary transcripts on a large model.
        assert_eq!(config.trigger_tokens(1_048_576), 838_860);
        // A small budget falls back to the floor rather than a fraction.
        assert_eq!(config.trigger_tokens(64_000), 64_000);
        let scaled = PruneConfig {
            trigger_percent: 25,
            floor_tokens: 1_000,
            ..PruneConfig::default()
        };
        assert_eq!(scaled.trigger_tokens(8_000), 2_000);
        assert_eq!(scaled.trigger_tokens(1_000), 1_000);
    }

    #[test]
    fn frozen_checkpoint_is_not_recomputed_while_parked_above_trigger() {
        // Once a checkpoint is adopted, turns that add no meaningful new
        // reclaim must not rewrite it: re-deciding would rewrite earlier
        // projected messages and cost a cache miss every turn (ADR-011
        // forbids per-turn pruning).
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..6 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("r{n}"), &format!("src/f{n}.rs"))],
            ));
            items.push(result(&format!("r{n}"), &big("read", 30)));
        }
        // A recent large read that the protected tail keeps, so the pruned
        // projection stays above the (deliberately low) trigger.
        items.push(assistant_calls(
            "read recent",
            vec![read_call("recent", "src/new.rs")],
        ));
        items.push(result("recent", &big("recent", 70)));
        items.push(assistant_text("done"));
        let config = PruneConfig {
            trigger_percent: 0,
            floor_tokens: 2_000,
            min_reclaim_tokens: 5_000,
            ..PruneConfig::default()
        };
        let session = session_with(items.clone());
        let first = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("triggered");
        assert!(
            first.census.total > config.trigger_tokens(budget().max_input_tokens),
            "the pruned projection is still above the trigger"
        );

        // A further user turn adds no newly reclaimable content.
        items.push(Item::User {
            text: "still working".to_owned(),
        });
        let grown = session_with(items);
        let second = compiler(config)
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        assert_eq!(
            second.prune_checkpoint.as_ref(),
            Some(&checkpoint),
            "the frozen checkpoint must be carried verbatim, not recomputed"
        );
    }

    fn messages_contain(request: &crate::model::ModelRequest, needle: &str) -> bool {
        request
            .messages
            .iter()
            .any(|message| messages_serialized(std::slice::from_ref(message)).contains(needle))
    }
}
