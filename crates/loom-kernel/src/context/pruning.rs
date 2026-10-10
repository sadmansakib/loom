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
//! Never pruned by the result-stubbing strategies: user items,
//! assistant messages, and the protected tail — the last
//! `protected_tail_turns` turns *and* at least
//! `protected_tail_tokens` estimated tokens of trailing items
//! (whichever reaches further back). History elision (below) touches
//! old call arguments, non-first user items, and — when
//! `elide_reasoning` is enabled — old assistant reasoning traces; it
//! never touches assistant text.
//!
//! # History elision (roadmap item 12, deterministic half)
//!
//! Result stubbing alone leaves two buckets stacking verbatim every
//! turn: tool-call *arguments* (an edit call carries the whole new file
//! content — 28 % of context tokens on CliffCompaction's Terminal-Bench
//! unpacking) and old non-first *user items*. When `elide_turns` is
//! set (default 8), the same episodic machinery reduces them below the
//! window trigger:
//!
//! * old calls keep their signature — name, id, short strings (paths,
//!   flags) — while every string longer than `elide_arg_chars` in the
//!   arguments becomes `[pruned N chars]`; the frozen checkpoint stores
//!   the elided arguments verbatim so re-application is byte-stable.
//! * user items *superseded by a later instruction* — never ordinal 0
//!   (the session brief) and never the last user item (the live task's
//!   brief under follow-up tasks) — become a one-line stub once they
//!   age past the window.
//! * assistant reasoning traces older than `elide_reasoning` turns are
//!   dropped from the projection (`reasoning = None`). Off by default:
//!   replaying reasoning verbatim is a provider contract on the OpenAI
//!   wire (DeepSeek rejects a tool-bearing request whose prior
//!   assistant turns lack `reasoning_content`), so the engine enables
//!   it only where omission is provably safe — GLM (forfeits only the
//!   preserved-thinking cache benefit) and Anthropic (never forwards
//!   reasoning on replay anyway).
//!
//! All are deterministic — no model call, no I/O — and run through the
//! same reclaim gate and checkpoint freeze, so each batch costs exactly
//! one prefix-cache miss. Assistant text is still never reduced; the
//! LLM-handoff remains the last resort for it
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

use crate::model::{Message, StopReason};
use crate::session::Session;
use crate::tool::{PathAccess, ToolRegistry, paths_overlap};
use crate::transcript::{Item, ToolArguments, ToolCallId, ToolResult, ToolStatus};

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
            elide_turns: Some(8),
            elide_arg_chars: 160,
            elide_reasoning: None,
            elide_assistant_text: Some(8),
            elide_assistant_chars: 400,
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
    /// A tool call's arguments were reduced to a signature — strings
    /// longer than `elide_arg_chars` replaced with `[pruned N chars]`
    /// (history elision). Never used for result bodies; `stub_text`
    /// exists for exhaustive matches and diagnostics.
    ElidedArgs,
    /// A user item after the task brief was replaced with a stub
    /// (history elision). The brief itself is never elided.
    ElidedUser,
    /// An assistant reasoning trace was dropped from the projection
    /// (history elision, provider-gated via
    /// [`PruneConfig::elide_reasoning`]). Assistant text is only
    /// elided by the last-resort window ([`StubReason::ElidedAssistantText`]).
    ElidedReasoning,
    /// An assistant text was reduced to its leading signature —
    /// the first 200 characters plus a `[pruned N chars]` marker
    /// (last-resort history elision, window/rescue passes only).
    ElidedAssistantText,
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
            Self::ElidedArgs => {
                "[pruned] call arguments reduced to their signature; the result body carries the outcome"
            }
            Self::ElidedUser => {
                "[pruned] earlier user message elided by context pruning; the original task brief is preserved"
            }
            Self::ElidedReasoning => "[pruned] earlier reasoning elided by context pruning",
            Self::ElidedAssistantText => {
                "[pruned] earlier assistant text reduced to its signature by context pruning"
            }
        }
    }
}

/// The constant signature an elided assistant text becomes: its first
/// [`ASSISTANT_TEXT_KEEP_CHARS`] characters (never splitting a UTF-8
/// boundary) plus a `[pruned N chars]` marker naming the full length.
///
/// A constant, not a config knob: frozen elisions re-apply this to the
/// immutable transcript text, so byte-stability across config changes
/// requires the replacement to depend only on the text itself.
fn elide_text(text: &str) -> String {
    // Idempotent: an already-elided signature (200 leading characters
    // plus a short marker) is shorter than the keep window plus marker
    // margin, so re-applying an elision — the rescue arm bakes one into
    // its view and the frozen re-apply then runs over that projection —
    // leaves it byte-identical instead of nesting markers.
    if text.chars().count() <= ASSISTANT_TEXT_KEEP_CHARS + 64 {
        return text.to_owned();
    }
    // Byte offset just past the ASSISTANT_TEXT_KEEP_CHARS-th character:
    // the keep window counts characters, not bytes.
    let mut boundary = 0;
    for (seen, (offset, ch)) in text.char_indices().enumerate() {
        if seen == ASSISTANT_TEXT_KEEP_CHARS {
            boundary = offset;
            break;
        }
        boundary = offset + ch.len_utf8();
    }
    let mut signature = String::from(&text[..boundary]);
    signature.push_str(&format!(" [pruned {} chars]", text.chars().count()));
    signature
}

/// Leading characters an elided assistant text keeps.
const ASSISTANT_TEXT_KEEP_CHARS: usize = 200;

/// One frozen stub decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubDecision {
    /// The call whose result is stubbed.
    pub call_id: String,
    /// Why — decides the stub text.
    pub reason: StubReason,
}

/// One frozen argument elision (history elision): the call keeps its
/// id, name and short argument strings, while every string longer than
/// `PruneConfig::elide_arg_chars` was replaced with `[pruned N chars]`.
///
/// The *elided* arguments are frozen verbatim (not just the call id) so
/// re-applying a checkpoint never depends on config drift between the
/// freeze and the re-apply — the projection stays byte-stable by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgElision {
    /// The call whose arguments are elided.
    pub call_id: String,
    /// The already-elided arguments, projected verbatim on re-apply.
    pub arguments: ToolArguments,
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

        // Handoff view (roadmap item 12's last resort): when the frozen
        // checkpoint carries an LLM handoff, the inner compiler projects
        // brief + handoff + tail instead of the full transcript. Decisions
        // and attribution still run over the *original* transcript; only
        // what the inner compiler sees is the view.
        let handoff_active = checkpoint
            .as_ref()
            .is_some_and(|frozen| frozen.handoff.is_some());
        let handoff_view;
        let projected: &Session = if handoff_active {
            handoff_view = handoff_session(
                session,
                checkpoint
                    .as_ref()
                    .and_then(|frozen| frozen.handoff.as_deref())
                    .unwrap_or_default(),
                checkpoint.as_ref().map_or(0, |frozen| frozen.upto_item),
            );
            &handoff_view
        } else {
            session
        };

        let mut compiled = match self.inner.compile(ContextInput {
            session: projected,
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
                let mut decisions = decide(session, tools, self.config, &*self.estimator, true);
                // Under a handoff the view renumbers user/assistant
                // ordinals, so ordinal-keyed elisions cannot apply there;
                // drop them so the freeze and the report agree on what
                // really runs (call-id-keyed stubs and argument elisions
                // are position-independent and survive).
                if handoff_active {
                    decisions.elided_users.clear();
                    decisions.elided_reasoning.clear();
                    decisions.elided_assistant_text.clear();
                }
                if decisions.is_empty() {
                    return Err(overrun);
                }
                let rescue_report =
                    prune_report(PruneTrigger::Rescue, session, &decisions, &*self.estimator);
                let stubbed = stubbed_session(projected, &decisions);
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
                        // The stubs and elisions are already baked into the
                        // stubbed session the inner compiler projected, so
                        // record the decisions verbatim.
                        stubbed_calls.extend(decisions.stubbed.iter().map(|d| d.call_id.clone()));
                        checkpoint = Some(PruneCheckpoint {
                            // Under a handoff the tail start is the point
                            // the handoff summarized up to; a re-freeze
                            // must not move it past the summarized region.
                            upto_item: if handoff_active {
                                checkpoint.as_ref().map_or(0, |frozen| frozen.upto_item)
                            } else {
                                session.transcript.len()
                            },
                            stubs: decisions.stubbed,
                            elided_args: decisions.elided_args,
                            elided_users: decisions.elided_users,
                            elided_reasoning: decisions.elided_reasoning,
                            elided_assistant_text: decisions.elided_assistant_text,
                            handoff: checkpoint
                                .as_ref()
                                .and_then(|frozen| frozen.handoff.clone()),
                        });
                        report = Some(rescue_report);
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
        // must stay byte-stable (ADR-011). Result stubs and history
        // elisions are independent rewrites; both feed the same recount.
        // Under a handoff only the call-id-keyed rewrites apply — the view
        // renumbers user/assistant ordinals, so ordinal-keyed elisions
        // from the pre-handoff transcript would land on the wrong items.
        if let Some(frozen) = checkpoint.as_ref().filter(|c| !c.is_empty()) {
            stubbed_calls.extend(apply_stubs(&mut compiled.request, &frozen.stubs));
            if frozen.handoff.is_none() {
                apply_elisions(
                    &mut compiled.request,
                    &frozen.elided_args,
                    &frozen.elided_users,
                    &frozen.elided_reasoning,
                    &frozen.elided_assistant_text,
                );
            } else {
                apply_elisions(&mut compiled.request, &frozen.elided_args, &[], &[], &[]);
            }
            compiled.census = census(&compiled.request, facts_tokens, &*self.estimator);
        }

        // Window-relative trigger: recompute decisions over the whole
        // transcript once the estimate nears the model's input budget.
        //
        // ADR-011: pruning is episodic — one cache miss per checkpoint, never
        // per turn. A checkpoint that already covers the transcript is only
        // superseded when the new pass reclaims a meaningful amount *beyond*
        // what the frozen decisions already reclaim; otherwise a session
        // parked above the trigger would re-decide (and rewrite earlier
        // projected messages) on every turn.
        //
        // Turn-age masking (roadmap item 10) and history elision
        // (item 12) add below-window arms: when configured, the decision
        // pass also runs *below* the window trigger so old read-only
        // results are masked and old call arguments and non-first user
        // items are elided as they age out. The same reclaim gate
        // applies — an aging batch is only worth a cache miss when it
        // reclaims at least `min_reclaim_tokens` beyond the frozen
        // checkpoint — so the projection stays byte-stable between
        // aging batches.
        let trigger = self.config.trigger_tokens(budget.max_input_tokens);
        let window_triggered = compiled.census.total > trigger;
        let age_masking = self.config.age_mask_turns.is_some();
        let elision_enabled = self.config.elide_turns.is_some()
            || self.config.elide_reasoning.is_some()
            || self.config.elide_assistant_text.is_some();
        if window_triggered || age_masking || elision_enabled {
            let mut decisions = decide(
                session,
                tools,
                self.config,
                &*self.estimator,
                window_triggered,
            );
            // Same handoff rule as the rescue arm: ordinal-keyed elisions
            // cannot land on the renumbered view, so they are dropped
            // before the gate, the report, and the freeze can see them.
            if handoff_active {
                decisions.elided_users.clear();
                decisions.elided_reasoning.clear();
                decisions.elided_assistant_text.clear();
            }
            let candidate_report = prune_report(
                if window_triggered {
                    PruneTrigger::Window
                } else {
                    PruneTrigger::AgeMask
                },
                session,
                &decisions,
                &*self.estimator,
            );
            let already = checkpoint.as_ref().map_or(0, |frozen| {
                frozen_reclaim(session, frozen, &*self.estimator)
            });
            if candidate_report.reclaimed_tokens
                >= already.saturating_add(u64::from(self.config.min_reclaim_tokens))
            {
                stubbed_calls.extend(apply_stubs(&mut compiled.request, &decisions.stubbed));
                if handoff_active {
                    apply_elisions(&mut compiled.request, &decisions.elided_args, &[], &[], &[]);
                } else {
                    apply_elisions(
                        &mut compiled.request,
                        &decisions.elided_args,
                        &decisions.elided_users,
                        &decisions.elided_reasoning,
                        &decisions.elided_assistant_text,
                    );
                }
                compiled.census = census(&compiled.request, facts_tokens, &*self.estimator);
                checkpoint = Some(PruneCheckpoint {
                    upto_item: if handoff_active {
                        checkpoint.as_ref().map_or(0, |frozen| frozen.upto_item)
                    } else {
                        session.transcript.len()
                    },
                    stubs: decisions.stubbed,
                    elided_args: decisions.elided_args,
                    elided_users: decisions.elided_users,
                    elided_reasoning: decisions.elided_reasoning,
                    elided_assistant_text: decisions.elided_assistant_text,
                    handoff: checkpoint
                        .as_ref()
                        .and_then(|frozen| frozen.handoff.clone()),
                });
                report = Some(candidate_report);
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

/// Builds the handoff projection view: the session's opening brief, the
/// LLM-written handoff as a user item, then the transcript tail from
/// `upto_item` on — everything before the tail was summarized away. The
/// durable transcript is never mutated; only this clone's projection
/// changes, mirroring [`stubbed_session`].
fn handoff_session(session: &Session, text: &str, upto_item: usize) -> Session {
    let upto = upto_item.min(session.transcript.len());
    let mut transcript = Vec::with_capacity(2 + session.transcript.len() - upto);
    // The brief is the session's first user item (ordinal 0, never
    // elided); when the transcript opens on something else the handoff
    // alone leads.
    if matches!(session.transcript.first(), Some(Item::User { .. })) {
        transcript.push(session.transcript[0].clone());
    }
    transcript.push(Item::User {
        text: text.to_owned(),
    });
    transcript.extend(session.transcript[upto..].iter().cloned());
    let mut view = session.clone();
    view.transcript = transcript;
    view
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
/// mirroring [`apply_stubs`] and [`apply_elisions`] item for item.
fn stubbed_session(session: &Session, decisions: &DecisionSet) -> Session {
    let by_call: HashMap<&str, StubReason> = decisions
        .stubbed
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let elided_calls: HashMap<&str, &ToolArguments> = decisions
        .elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let mut stubbed = session.clone();
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for item in &mut stubbed.transcript {
        match item {
            Item::ToolResult(result) => {
                let Some(reason) = by_call.get(result.call_id.as_str()) else {
                    continue;
                };
                result.content = reason.stub_text().to_owned();
                result.truncated = false;
            }
            Item::Assistant(assistant) => {
                for call in &mut assistant.tool_calls {
                    if let Some(args) = elided_calls.get(call.id.as_str()) {
                        call.arguments = (*args).clone();
                    }
                }
                if assistant.stop_reason != StopReason::ContentFilter {
                    if decisions.elided_reasoning.contains(&assistant_ordinal) {
                        assistant.reasoning = None;
                    }
                    if decisions.elided_assistant_text.contains(&assistant_ordinal) {
                        assistant.content = elide_text(&assistant.content);
                    }
                    assistant_ordinal += 1;
                }
            }
            Item::User { text } => {
                if decisions.elided_users.contains(&user_ordinal) {
                    *text = StubReason::ElidedUser.stub_text().to_owned();
                }
                user_ordinal += 1;
            }
        }
    }
    stubbed
}

/// Rewrites projected assistant tool-call arguments, user messages, and
/// assistant reasoning per the frozen elisions: calls keep their
/// signature (the elided arguments are stored verbatim in the
/// checkpoint), user items past the brief become a one-line stub, and
/// elided reasoning traces are dropped outright. User items are matched
/// by ordinal (0-based, transcript order) and assistants by their
/// ordinal among *non-`ContentFilter`* assistant items — the projection
/// filters those turns, so transcript indexes would not line up with
/// request messages (the request contains only the non-filtered
/// assistants, so its own `Message::Assistant` count matches the
/// ordinals exactly).
fn apply_elisions(
    request: &mut crate::model::ModelRequest,
    elided_args: &[ArgElision],
    elided_users: &[usize],
    elided_reasoning: &[usize],
    elided_assistant_text: &[usize],
) {
    if elided_args.is_empty()
        && elided_users.is_empty()
        && elided_reasoning.is_empty()
        && elided_assistant_text.is_empty()
    {
        return;
    }
    let by_call: HashMap<&str, &ToolArguments> = elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for message in &mut request.messages {
        match message {
            Message::Assistant(assistant) => {
                for call in &mut assistant.tool_calls {
                    if let Some(args) = by_call.get(call.id.as_str()) {
                        call.arguments = (*args).clone();
                    }
                }
                if elided_reasoning.contains(&assistant_ordinal) {
                    assistant.reasoning = None;
                }
                if elided_assistant_text.contains(&assistant_ordinal) {
                    assistant.content = elide_text(&assistant.content);
                }
                assistant_ordinal += 1;
            }
            Message::User { text } => {
                if elided_users.contains(&user_ordinal) {
                    *text = StubReason::ElidedUser.stub_text().to_owned();
                }
                user_ordinal += 1;
            }
            _ => {}
        }
    }
}

/// Tokens the already-frozen decisions reclaim from the *current*
/// transcript, so `compile` can gate a fresh checkpoint on the reclaim
/// that is genuinely new (ADR-011: pruning is episodic — one cache miss
/// per checkpoint). Covers result stubs, argument elisions, and user
/// elisions alike: whatever the frozen checkpoint takes off the
/// transcript's back counts.
fn frozen_reclaim(
    session: &Session,
    frozen: &PruneCheckpoint,
    estimator: &dyn TokenEstimator,
) -> u64 {
    let by_call: HashMap<&str, StubReason> = frozen
        .stubs
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let elided_calls: HashMap<&str, &ToolArguments> = frozen
        .elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let elided_users: HashSet<usize> = frozen.elided_users.iter().copied().collect();
    let elided_reasoning: HashSet<usize> = frozen.elided_reasoning.iter().copied().collect();
    let elided_assistant_text: HashSet<usize> =
        frozen.elided_assistant_text.iter().copied().collect();
    let elided_user_stub = Message::User {
        text: StubReason::ElidedUser.stub_text().to_owned(),
    };
    let mut reclaim = 0u64;
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for item in &session.transcript {
        match item {
            Item::ToolResult(result) => {
                let Some(reason) = by_call.get(result.call_id.as_str()) else {
                    continue;
                };
                reclaim += u64::from(estimate_serialized(estimator, item).saturating_sub(
                    stub_tokens(&result.call_id, &result.status, *reason, estimator),
                ));
            }
            Item::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    let Some(elided) = elided_calls.get(call.id.as_str()) else {
                        continue;
                    };
                    reclaim += u64::from(
                        estimate_serialized(estimator, &call.arguments)
                            .saturating_sub(estimate_serialized(estimator, elided)),
                    );
                }
                if assistant.stop_reason != StopReason::ContentFilter {
                    if elided_reasoning.contains(&assistant_ordinal)
                        && let Some(trace) = &assistant.reasoning
                    {
                        reclaim += u64::from(estimate_serialized(estimator, trace));
                    }
                    if elided_assistant_text.contains(&assistant_ordinal) {
                        reclaim += u64::from(
                            estimate_serialized(estimator, &assistant.content).saturating_sub(
                                estimate_serialized(estimator, &elide_text(&assistant.content)),
                            ),
                        );
                    }
                    assistant_ordinal += 1;
                }
            }
            Item::User { text } => {
                if elided_users.contains(&user_ordinal) {
                    let full = Message::User { text: text.clone() };
                    reclaim += u64::from(
                        estimate_serialized(estimator, &full)
                            .saturating_sub(estimate_serialized(estimator, &elided_user_stub)),
                    );
                }
                user_ordinal += 1;
            }
        }
    }
    reclaim
}

/// Builds the freeze-time attribution for `decisions` against
/// `session`'s transcript — per-call estimated reclaim (full item less
/// its stub/elided replacement) plus the arm that fired. User elisions
/// report under the synthetic id `user:{ordinal}`, reasoning elisions
/// under `assistant:{ordinal}`.
fn prune_report(
    trigger: PruneTrigger,
    session: &Session,
    decisions: &DecisionSet,
    estimator: &dyn TokenEstimator,
) -> PruneReport {
    let by_call: HashMap<&str, StubReason> = decisions
        .stubbed
        .iter()
        .map(|stub| (stub.call_id.as_str(), stub.reason))
        .collect();
    let elided_calls: HashMap<&str, &ToolArguments> = decisions
        .elided_args
        .iter()
        .map(|elision| (elision.call_id.as_str(), &elision.arguments))
        .collect();
    let elided_users: HashSet<usize> = decisions.elided_users.iter().copied().collect();
    let elided_reasoning: HashSet<usize> = decisions.elided_reasoning.iter().copied().collect();
    let elided_assistant_text: HashSet<usize> =
        decisions.elided_assistant_text.iter().copied().collect();
    let elided_user_stub = Message::User {
        text: StubReason::ElidedUser.stub_text().to_owned(),
    };
    let mut calls = Vec::new();
    let mut user_ordinal = 0usize;
    let mut assistant_ordinal = 0usize;
    for item in &session.transcript {
        match item {
            Item::ToolResult(result) => {
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
            Item::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    let Some(elided) = elided_calls.get(call.id.as_str()) else {
                        continue;
                    };
                    let full = estimate_serialized(estimator, &call.arguments);
                    let elided_tokens = estimate_serialized(estimator, elided);
                    calls.push(PrunedCall {
                        call_id: call.id.as_str().to_owned(),
                        reason: StubReason::ElidedArgs,
                        reclaimed_tokens: u64::from(full.saturating_sub(elided_tokens)),
                    });
                }
                if assistant.stop_reason != StopReason::ContentFilter {
                    if elided_reasoning.contains(&assistant_ordinal)
                        && let Some(trace) = &assistant.reasoning
                    {
                        calls.push(PrunedCall {
                            call_id: format!("assistant:{assistant_ordinal}"),
                            reason: StubReason::ElidedReasoning,
                            reclaimed_tokens: u64::from(estimate_serialized(estimator, trace)),
                        });
                    }
                    if elided_assistant_text.contains(&assistant_ordinal) {
                        let signature = elide_text(&assistant.content);
                        calls.push(PrunedCall {
                            call_id: format!("assistant:{assistant_ordinal}"),
                            reason: StubReason::ElidedAssistantText,
                            reclaimed_tokens: u64::from(
                                estimate_serialized(estimator, &assistant.content)
                                    .saturating_sub(estimate_serialized(estimator, &signature)),
                            ),
                        });
                    }
                    assistant_ordinal += 1;
                }
            }
            Item::User { text } => {
                if elided_users.contains(&user_ordinal) {
                    let full = Message::User { text: text.clone() };
                    let stub = estimate_serialized(estimator, &elided_user_stub);
                    calls.push(PrunedCall {
                        call_id: format!("user:{user_ordinal}"),
                        reason: StubReason::ElidedUser,
                        reclaimed_tokens: u64::from(
                            estimate_serialized(estimator, &full).saturating_sub(stub),
                        ),
                    });
                }
                user_ordinal += 1;
            }
        }
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

/// Facts about one assistant tool call, for argument elision.
struct CallFacts {
    /// Transcript index of the assistant item carrying the call.
    index: usize,
    /// Assistant turn the call belongs to (1-based).
    turn: usize,
    call_id: ToolCallId,
    /// The call's arguments — elision runs over a copy of these.
    arguments: ToolArguments,
}

/// Facts about one assistant reasoning trace, for reasoning elision.
struct ReasoningFacts {
    /// Transcript index of the assistant item carrying the trace.
    index: usize,
    /// Assistant turn the item belongs to (1-based).
    turn: usize,
    /// Ordinal among *non-`ContentFilter`* assistant items (0-based) —
    /// the key the checkpoint freezes; the projection drops filtered
    /// turns, so plain transcript order would misalign with request
    /// messages.
    ordinal: usize,
    /// Estimated tokens of the serialized trace — the reclaim math.
    tokens: u32,
}

/// Facts about one assistant text, for last-resort text elision. Same
/// shape and ordinality as [`ReasoningFacts`].
struct AssistantTextFacts {
    /// Transcript index of the assistant item carrying the text.
    index: usize,
    /// Assistant turn the item belongs to (1-based).
    turn: usize,
    /// Ordinal among *non-`ContentFilter`* assistant items (0-based).
    ordinal: usize,
    /// Character length of the text — the elision-candidate gate.
    chars: usize,
    /// Estimated tokens of the elided signature — the reclaim math
    /// subtracts this from the full text's estimate.
    signature_tokens: u32,
}

/// Everything the strategy passes reason over, collected in two
/// transcript scans: per-result facts, per-call facts, assistant turn
/// boundaries, user-item ordinals, and the writes that can supersede
/// earlier reads.
struct TranscriptFacts {
    results: Vec<ResultFacts>,
    calls: Vec<CallFacts>,
    reasoning_at: Vec<ReasoningFacts>,
    assistant_text_at: Vec<AssistantTextFacts>,
    /// Transcript index of every assistant item — the turn boundaries.
    assistant_at: Vec<usize>,
    /// Transcript indexes of user items, in order — the position in
    /// this list *is* the ordinal the checkpoint freezes.
    user_at: Vec<usize>,
    /// `(index, changed paths)` per tool result that wrote or deleted.
    writes: Vec<(usize, Vec<String>)>,
}

impl TranscriptFacts {
    /// The two fact-gathering scans behind [`decide`].
    fn collect(session: &Session, registry: &ToolRegistry, estimator: &dyn TokenEstimator) -> Self {
        let transcript: &[Item] = &session.transcript;

        // Scan 1 — call metadata (name, canonical args, declared reads)
        // from assistant tool_calls, keyed by call id for O(1) lookup.
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

        // Scan 2 — tool-result and call facts, assistant turn
        // boundaries, user ordinals, later writes.
        let mut assistant_at: Vec<usize> = Vec::new();
        let mut results: Vec<ResultFacts> = Vec::new();
        let mut call_facts: Vec<CallFacts> = Vec::new();
        let mut reasoning_at: Vec<ReasoningFacts> = Vec::new();
        let mut assistant_text_at: Vec<AssistantTextFacts> = Vec::new();
        let mut user_at: Vec<usize> = Vec::new();
        let mut writes: Vec<(usize, Vec<String>)> = Vec::new();
        let mut turn = 0usize;
        let mut assistant_ordinal = 0usize;
        for (index, item) in transcript.iter().enumerate() {
            match item {
                Item::Assistant(assistant) => {
                    turn += 1;
                    assistant_at.push(index);
                    for call in &assistant.tool_calls {
                        call_facts.push(CallFacts {
                            index,
                            turn,
                            call_id: call.id.clone(),
                            arguments: call.arguments.clone(),
                        });
                    }
                    if assistant.stop_reason != StopReason::ContentFilter {
                        if let Some(trace) = &assistant.reasoning {
                            reasoning_at.push(ReasoningFacts {
                                index,
                                turn,
                                ordinal: assistant_ordinal,
                                tokens: estimate_serialized(estimator, trace),
                            });
                        }
                        if !assistant.content.is_empty() {
                            assistant_text_at.push(AssistantTextFacts {
                                index,
                                turn,
                                ordinal: assistant_ordinal,
                                chars: assistant.content.chars().count(),
                                signature_tokens: estimate_serialized(
                                    estimator,
                                    &elide_text(&assistant.content),
                                ),
                            });
                        }
                        assistant_ordinal += 1;
                    }
                }
                Item::ToolResult(result) => {
                    let read_call =
                        calls
                            .get(result.call_id.as_str())
                            .and_then(|(name, args, reads)| {
                                is_read_only(name)
                                    .then(|| (name.clone(), args.clone(), reads.clone()))
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
                Item::User { .. } => user_at.push(index),
            }
        }
        Self {
            results,
            calls: call_facts,
            reasoning_at,
            assistant_text_at,
            assistant_at,
            user_at,
            writes,
        }
    }

    /// Index of the first unprotected item: the protected tail reaches
    /// back over the last `protected_tail_turns` turns AND at least
    /// `protected_tail_tokens` estimated trailing tokens — whichever
    /// reaches further back.
    fn protected_from(
        &self,
        transcript: &[Item],
        config: PruneConfig,
        estimator: &dyn TokenEstimator,
    ) -> usize {
        let turn_cut = self
            .assistant_at
            .len()
            .checked_sub(config.protected_tail_turns)
            .and_then(|from| self.assistant_at.get(from))
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
        turn_cut.min(token_cut)
    }
}

/// Accumulated stub and elision decisions across the strategy passes,
/// in ADR-011's fixed order — earlier passes win, later ones skip
/// already-decided items so each keeps its most specific reason.
#[derive(Default)]
struct DecisionSet {
    stubbed: Vec<StubDecision>,
    stubbed_ids: HashSet<String>,
    elided_args: Vec<ArgElision>,
    elided_arg_ids: HashSet<String>,
    elided_users: Vec<usize>,
    elided_reasoning: Vec<usize>,
    elided_assistant_text: Vec<usize>,
    reclaim_tokens: u64,
}

impl DecisionSet {
    /// Records one result stub and its estimated reclaim.
    fn add(&mut self, facts: &ResultFacts, reason: StubReason, estimator: &dyn TokenEstimator) {
        self.reclaim_tokens += u64::from(facts.tokens.saturating_sub(stub_tokens(
            &facts.call_id,
            &facts.status,
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
    fn is_empty(&self) -> bool {
        self.stubbed.is_empty()
            && self.elided_args.is_empty()
            && self.elided_users.is_empty()
            && self.elided_reasoning.is_empty()
            && self.elided_assistant_text.is_empty()
    }

    /// Whether `facts` is outside the protected region and not already
    /// stubbed by an earlier pass — the shared guard of passes 2–5.
    fn eligible(&self, facts: &ResultFacts, protected_from: usize) -> bool {
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
        let Some((name, args, _)) = result.read_call.as_ref() else {
            continue;
        };
        newest_by_call
            .entry((name.as_str(), args.as_str()))
            .and_modify(|index| *index = (*index).max(result.index))
            .or_insert(result.index);
    }
    for earlier in &facts.results {
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
        let Some((_, _, reads)) = earlier.read_call.as_ref() else {
            continue;
        };
        let superseded = facts.writes.iter().any(|(index, changed)| {
            *index > earlier.index
                && reads
                    .iter()
                    .any(|read| changed.iter().any(|path| paths_overlap(read, path)))
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

/// Pass 5 — turn-age masking: stub successful read-only results older
/// than `age_mask_turns` assistant turns, independent of the window
/// trigger — the transcript replay is capped at a rolling window, so a
/// long task's request size plateaus instead of growing without bound.
/// Runs after the other passes so already-stubbed calls keep their more
/// specific reasons; the protected tail always wins.
fn mask_aged_reads(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.age_mask_turns else {
        return;
    };
    for earlier in &facts.results {
        if !pass.eligible(earlier, protected_from) {
            continue;
        }
        if earlier.status == ToolStatus::Success
            && earlier.read_call.is_some()
            && total_turns.saturating_sub(earlier.turn) > max_age
        {
            pass.add(earlier, StubReason::TurnAge, estimator);
        }
    }
}

/// Pass 6 — argument elision: tool calls older than `elide_turns`
/// assistant turns have their arguments reduced to a signature — every
/// string longer than `elide_arg_chars` becomes `[pruned N chars]`,
/// while names, ids, paths and flags survive. The elided arguments are
/// frozen verbatim, so re-application is byte-stable by construction.
/// Mutating calls included: their arguments (edit payloads) are the
/// bucket result stubbing never touches. The protected tail always
/// wins.
fn elide_aged_calls(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_turns else {
        return;
    };
    for call in &facts.calls {
        if call.index >= protected_from
            || total_turns.saturating_sub(call.turn) <= max_age
            || pass.elided_arg_ids.contains(call.call_id.as_str())
        {
            continue;
        }
        let Some(elided) = elide_arguments(&call.arguments, config.elide_arg_chars) else {
            continue;
        };
        let reclaim = estimate_serialized(estimator, &call.arguments)
            .saturating_sub(estimate_serialized(estimator, &elided));
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_arg_ids.insert(call.call_id.as_str().to_owned());
        pass.elided_args.push(ArgElision {
            call_id: call.call_id.as_str().to_owned(),
            arguments: elided,
        });
    }
}

/// Pass 7 — user-item elision: user items *superseded by a later
/// instruction* (the last user item — the live task's brief under
/// `run_task`/`continue_session` follow-ups — is never elided, nor is
/// the session's first brief) older than `elide_turns` assistant turns
/// become a one-line stub. Completion-gate refusals and follow-up
/// prompts age out like any other history instead of stacking verbatim
/// forever. The protected tail always wins.
///
/// Task-poison guard: eliding only superseded items means the compiler
/// can never strip the instructions the *current* task runs on — every
/// live brief is either ordinal 0 or the last user item, and both are
/// exempt by construction.
fn elide_aged_users(
    facts: &TranscriptFacts,
    transcript: &[Item],
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_turns else {
        return;
    };
    let Some(&last_user_index) = facts.user_at.last() else {
        return;
    };
    let stub = Message::User {
        text: StubReason::ElidedUser.stub_text().to_owned(),
    };
    let stub_tokens = estimate_serialized(estimator, &stub);
    for (ordinal, &index) in facts.user_at.iter().enumerate() {
        if ordinal == 0
            || index >= last_user_index
            || pass.elided_users.contains(&ordinal)
            || index >= protected_from
            || total_turns.saturating_sub(turn_of(facts, index)) <= max_age
        {
            continue;
        }
        let Item::User { text } = &transcript[index] else {
            continue;
        };
        let full = Message::User { text: text.clone() };
        let reclaim = estimate_serialized(estimator, &full).saturating_sub(stub_tokens);
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_users.push(ordinal);
    }
}

/// Pass 8 — reasoning elision: assistant reasoning traces older than
/// `elide_reasoning` turns are dropped from the projection entirely
/// (`reasoning = None` — no stub text; a reasoning slot is either the
/// provider's verbatim trace or absent, never paraphrased).
///
/// Provider-gated: the default is `None`, and only the engine's
/// provider gate enables it — omitting replayed reasoning is a hard
/// error on DeepSeek's tool-bearing wire but costs GLM only an
/// episodic cache miss, and Anthropic never forwards reasoning on
/// replay. The protected tail always wins. Ordinals are among
/// non-`ContentFilter` assistants, matching what the projection (and
/// thus the request) contains.
fn elide_aged_reasoning(
    facts: &TranscriptFacts,
    protected_from: usize,
    config: PruneConfig,
    total_turns: usize,
    pass: &mut DecisionSet,
) {
    let Some(max_age) = config.elide_reasoning else {
        return;
    };
    for trace in &facts.reasoning_at {
        if trace.index >= protected_from
            || total_turns.saturating_sub(trace.turn) <= max_age
            || trace.tokens == 0
            || pass.elided_reasoning.contains(&trace.ordinal)
        {
            continue;
        }
        pass.reclaim_tokens += u64::from(trace.tokens);
        pass.elided_reasoning.push(trace.ordinal);
    }
}

/// Pass 9 — assistant-text elision (last resort, deterministic): texts
/// older than `elide_assistant_text` assistant turns and longer than
/// `elide_assistant_chars` are reduced to their leading signature —
/// first 200 characters plus a `[pruned N chars]` marker — *only when
/// the window already triggered or the budget-rescue pass is running*.
/// A routine aging batch never rewrites prose the model may still
/// anchor on; this pass exists so the rescue arm has one more
/// deterministic lever before the engine's LLM handoff (roadmap item
/// 12). The protected tail always wins. Ordinals are among
/// non-`ContentFilter` assistants, matching what the projection
/// contains.
fn elide_aged_assistant_text(
    facts: &TranscriptFacts,
    session: &Session,
    protected_from: usize,
    config: PruneConfig,
    window_triggered: bool,
    estimator: &dyn TokenEstimator,
    pass: &mut DecisionSet,
) {
    let total_turns = facts.assistant_at.len();
    let Some(max_age) = config.elide_assistant_text else {
        return;
    };
    if !window_triggered {
        return;
    }
    for text in &facts.assistant_text_at {
        if text.index >= protected_from
            || text.chars <= config.elide_assistant_chars
            || total_turns.saturating_sub(text.turn) <= max_age
            || pass.elided_assistant_text.contains(&text.ordinal)
        {
            continue;
        }
        let Item::Assistant(assistant) = &session.transcript[text.index] else {
            continue;
        };
        let full = estimate_serialized(estimator, &assistant.content);
        let reclaim = full.saturating_sub(text.signature_tokens);
        if reclaim == 0 {
            continue;
        }
        pass.reclaim_tokens += u64::from(reclaim);
        pass.elided_assistant_text.push(text.ordinal);
    }
}

/// The assistant turn a transcript index belongs to (the count of
/// assistant items at or before it).
fn turn_of(facts: &TranscriptFacts, index: usize) -> usize {
    facts.assistant_at.partition_point(|&at| at <= index)
}

/// Computes the decision set from the transcript under ADR-011's fixed
/// strategy order, plus the reclaim estimate (in tokens) against the
/// *unpruned* projection.
fn decide(
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

/// Reduces `args` to its signature: every string longer than
/// `min_chars` (recursively, in objects and arrays alike) is replaced
/// with `[pruned N chars]`; shorter strings and all non-string scalars
/// survive. Returns `None` when nothing changed — the caller treats
/// that as "nothing to elide" and skips the call entirely.
fn elide_arguments(args: &ToolArguments, min_chars: usize) -> Option<ToolArguments> {
    let mut value = serde_json::Value::from(args.clone());
    if !elide_strings(&mut value, min_chars) {
        return None;
    }
    ToolArguments::try_from(value).ok()
}

/// Rewrites long strings inside `value` in place; reports whether
/// anything changed. Deterministic: the replacement names the elided
/// length, so two runs over the same arguments agree byte for byte.
fn elide_strings(value: &mut serde_json::Value, min_chars: usize) -> bool {
    match value {
        serde_json::Value::String(text) => {
            if text.len() > min_chars {
                *text = format!("[pruned {} chars]", text.len());
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .map(|item| elide_strings(item, min_chars))
            .fold(false, |changed, item_changed| changed | item_changed),
        serde_json::Value::Object(fields) => fields
            .iter_mut()
            .map(|(_, item)| elide_strings(item, min_chars))
            .fold(false, |changed, item_changed| changed | item_changed),
        _ => false,
    }
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

    /// A write call whose arguments carry a large payload: at 10 turns
    /// it is older than `elide_turns` (default 8) and outside the
    /// protected tail, so the arguments reduce to a signature below the
    /// window trigger while the recent call keeps full arguments.
    #[test]
    fn old_call_arguments_are_elided_below_the_window_trigger() {
        let big_write = ToolCall {
            id: ToolCallId::try_from("w0".to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/old.rs",
                "content": big("payload", 80),
            }))
            .expect("args"),
        };
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("write", vec![big_write]),
            result("w0", "ok"),
        ];
        for n in 1..9 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("elision fired");
        assert_eq!(
            checkpoint
                .elided_args
                .iter()
                .map(|elision| elision.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["w0"],
            "only the aged call's arguments elide"
        );
        let projected = messages_serialized(&compiled.request.messages);
        assert!(
            projected.contains("[pruned 81927 chars]"),
            "long strings elide"
        );
        assert!(projected.contains("src/old.rs"), "short strings survive");
        assert!(
            projected.contains("src/file8.rs"),
            "recent calls keep arguments"
        );
    }

    /// The session brief (ordinal 0) and the *last* user item — the
    /// live task's instructions under follow-up tasks — are never
    /// elided; a middle user item superseded by a later one ages out
    /// into the one-line stub instead of stacking verbatim forever.
    #[test]
    fn the_task_brief_is_never_elided_but_later_user_items_are() {
        let big_write = ToolCall {
            id: ToolCallId::try_from("w0".to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/old.rs",
                "content": big("payload", 80),
            }))
            .expect("args"),
        };
        let mut items = vec![
            Item::User {
                text: "fix the failing gate".to_owned(),
            },
            Item::User {
                text: big("earlier follow-up", 2),
            },
            Item::User {
                text: "now do the second thing".to_owned(),
            },
            assistant_calls("write", vec![big_write]),
            result("w0", "ok"),
        ];
        for n in 1..9 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("elision fired");
        assert_eq!(checkpoint.elided_users, vec![1], "ordinal 1 ages out");
        let projected = messages_serialized(&compiled.request.messages);
        assert!(projected.contains("fix the failing gate"), "brief intact");
        assert!(
            projected.contains("now do the second thing"),
            "the live (last) brief intact"
        );
        assert!(projected.contains("earlier user message elided"));
        assert!(!projected.contains("earlier follow-up"));
    }

    /// A follow-up task's brief is the last user item, so a long
    /// continuation never loses its instructions: even at 10+ turns the
    /// live brief projects verbatim while the superseded first task's
    /// exchanges age out.
    #[test]
    fn the_live_task_brief_survives_a_long_continuation() {
        let mut items = vec![Item::User {
            text: "do the first thing".to_owned(),
        }];
        for n in 0..9 {
            items.push(assistant_text(&format!("first task filler {n}")));
        }
        items.push(Item::User {
            text: "now the second".to_owned(),
        });
        items.push(assistant_text("second task turn one"));
        // The second task runs past the elision window.
        for n in 2..=9 {
            items.push(assistant_text(&format!("second task filler {n}")));
        }
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let projected = messages_serialized(&compiled.request.messages);
        assert!(projected.contains("now the second"), "live brief intact");
        assert!(
            projected.contains("do the first thing"),
            "the session's first brief is exempt by ordinal"
        );
        // Nothing user-shaped is superseded-and-aged, so no user elision
        // ever freezes — the guard holds by construction, not by gate.
        assert!(
            compiled
                .prune_checkpoint
                .as_ref()
                .is_none_or(|c| c.elided_users.is_empty())
        );
    }

    /// Feeding the checkpoint back re-applies the frozen elisions
    /// byte-exactly — the frozen arguments are projected verbatim, so
    /// later turns never re-decide (one cache miss per freeze).
    #[test]
    fn frozen_elisions_reapply_byte_stable() {
        let big_write = ToolCall {
            id: ToolCallId::try_from("w0".to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/old.rs",
                "content": big("payload", 80),
            }))
            .expect("args"),
        };
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("write", vec![big_write]),
            result("w0", "ok"),
        ];
        for n in 1..9 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items.clone());
        let first = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        items.push(assistant_text("one more turn"));
        let grown = session_with(items);
        let second = compiler(PruneConfig::default())
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        // The frozen projection is a strict prefix: every message from
        // the first compile re-projects byte-identically (the elided
        // arguments and stubs come from the checkpoint, not a re-decide),
        // and growth appends one message.
        assert_eq!(
            second.request.messages.len(),
            first.request.messages.len() + 1
        );
        for (frozen, regrown) in first
            .request
            .messages
            .iter()
            .zip(second.request.messages.iter())
        {
            assert_eq!(
                serde_json::to_string(frozen).expect("serialize"),
                serde_json::to_string(regrown).expect("serialize"),
                "the frozen elision re-applies without churn"
            );
        }
        assert_eq!(second.prune_checkpoint.as_ref(), Some(&checkpoint));
    }

    /// `elide_turns: None` opts out entirely: aged arguments and aged
    /// user items stay verbatim and no elisions freeze.
    #[test]
    fn elision_off_when_none() {
        let big_write = ToolCall {
            id: ToolCallId::try_from("w0".to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/old.rs",
                "content": big("payload", 80),
            }))
            .expect("args"),
        };
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            Item::User {
                text: big("follow-up", 80).to_owned(),
            },
            assistant_calls("write", vec![big_write]),
            result("w0", "ok"),
        ];
        for n in 1..=9 {
            items.push(assistant_text(&format!("filler {n}")));
        }
        let session = session_with(items);
        let compiled = compiler(PruneConfig {
            elide_turns: None,
            ..PruneConfig::default()
        })
        .compile(input(&session, None))
        .expect("compiles");
        let projected = messages_serialized(&compiled.request.messages);
        assert!(projected.contains("payloadxxxx"), "arguments verbatim");
        assert!(projected.contains("follow-up"), "user item verbatim");
        let checkpoint = compiled.prune_checkpoint.as_ref();
        assert!(checkpoint.is_none_or(|c| c.elided_args.is_empty() && c.elided_users.is_empty()));
    }

    /// Elision decisions attribute at the freeze moment like result
    /// stubs: per-call reclaim under `ElidedArgs`/`ElidedUser`, exactly
    /// once — the re-apply reports nothing.
    #[test]
    fn elision_reports_attribution_once() {
        let big_write = ToolCall {
            id: ToolCallId::try_from("w0".to_owned()).expect("call id"),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/old.rs",
                "content": big("payload", 80),
            }))
            .expect("args"),
        };
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            Item::User {
                text: big("follow-up", 2),
            },
            Item::User {
                text: "final".to_owned(),
            },
            assistant_calls("write", vec![big_write]),
            result("w0", "ok"),
        ];
        for n in 1..9 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items.clone());
        let first = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let report = first.prune_report.as_ref().expect("freeze reported");
        assert_eq!(report.trigger, PruneTrigger::AgeMask);
        assert!(report.calls.iter().any(|call| call.call_id == "w0"
            && call.reason == StubReason::ElidedArgs
            && call.reclaimed_tokens > 0));
        assert!(report.calls.iter().any(|call| call.call_id == "user:1"
            && call.reason == StubReason::ElidedUser
            && call.reclaimed_tokens > 0));
        assert_eq!(
            report.reclaimed_tokens,
            report
                .calls
                .iter()
                .map(|call| call.reclaimed_tokens)
                .sum::<u64>()
        );
        // Re-apply only: no new attribution.
        items.push(assistant_text("one more turn"));
        let grown = session_with(items);
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        let second = compiler(PruneConfig::default())
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        assert!(second.prune_report.is_none());
    }

    fn assistant_with_reasoning(content: &str, reasoning: &str) -> Item {
        Item::Assistant(AssistantMessage {
            content: content.to_owned(),
            reasoning: Some(crate::transcript::ReasoningTrace {
                text: reasoning.to_owned(),
                provider_data: None,
            }),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        })
    }

    fn filtered_assistant(content: &str, reasoning: &str) -> Item {
        Item::Assistant(AssistantMessage {
            content: content.to_owned(),
            reasoning: Some(crate::transcript::ReasoningTrace {
                text: reasoning.to_owned(),
                provider_data: None,
            }),
            tool_calls: Vec::new(),
            stop_reason: StopReason::ContentFilter,
        })
    }

    /// The reasoning ordinals projected for a request, in message
    /// order — `None` where the trace was elided, `Some(len)` where it
    /// survives (the text itself is irrelevant to the assertions).
    fn projected_reasoning_lens(request: &crate::model::ModelRequest) -> Vec<Option<usize>> {
        request
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => {
                    Some(assistant.reasoning.as_ref().map(|trace| trace.text.len()))
                }
                _ => None,
            })
            .collect()
    }

    /// With `elide_reasoning` on, reasoning traces older than the
    /// window drop from the projection entirely while recent ones
    /// replay verbatim (test A of the provider-gated phase).
    #[test]
    fn aged_reasoning_is_elided_when_enabled() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 1..=12 {
            items.push(assistant_with_reasoning(
                &format!("turn {n}"),
                &big("think", 30),
            ));
        }
        let session = session_with(items);
        let config = PruneConfig {
            elide_reasoning: Some(8),
            ..firing()
        };
        let compiled = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("elision fired");
        assert_eq!(
            checkpoint.elided_reasoning,
            vec![0, 1, 2],
            "only the traces past the window freeze"
        );
        let lens = projected_reasoning_lens(&compiled.request);
        assert_eq!(lens.len(), 12, "every assistant projects");
        for (ordinal, len) in lens.iter().take(3).enumerate() {
            assert_eq!(*len, None, "ordinal {ordinal} elided");
        }
        for lens in lens.into_iter().skip(3) {
            assert!(lens.is_some(), "recent reasoning replays verbatim");
        }
    }

    /// The default keeps every reasoning trace — replaying reasoning is
    /// a provider contract on the OpenAI wire, so the kernel default
    /// is off and only the engine's provider gate opts in.
    #[test]
    fn reasoning_elision_is_off_by_default() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 1..=12 {
            items.push(assistant_with_reasoning(
                &format!("turn {n}"),
                &big("think", 30),
            ));
        }
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(
            compiled
                .prune_checkpoint
                .as_ref()
                .is_none_or(|c| c.elided_reasoning.is_empty())
        );
        assert!(
            projected_reasoning_lens(&compiled.request)
                .iter()
                .all(Option::is_some),
            "all reasoning replays"
        );
    }

    /// Reasoning ordinals count only non-`ContentFilter` assistants —
    /// the projection drops filtered turns, so the frozen keys must
    /// skip them too. A filtered turn sits between two elided traces
    /// and a reasoning-less assistant occupies the middle ordinal.
    #[test]
    fn reasoning_ordinals_skip_filtered_assistants() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        items.push(assistant_with_reasoning("turn 1", &big("think", 35)));
        items.push(filtered_assistant("refused", &big("think", 35)));
        items.push(assistant_text("no reasoning here"));
        for n in 3..=12 {
            items.push(assistant_with_reasoning(
                &format!("turn {n}"),
                &big("think", 35),
            ));
        }
        let session = session_with(items);
        let config = PruneConfig {
            elide_reasoning: Some(8),
            ..firing()
        };
        let compiled = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("elision fired");
        // The filtered turn takes no ordinal and the reasoning-less
        // assistant takes ordinal 1 without being a candidate.
        assert_eq!(checkpoint.elided_reasoning, vec![0, 2]);
        let lens = projected_reasoning_lens(&compiled.request);
        assert_eq!(lens.len(), 12, "the filtered turn does not project");
        assert_eq!(lens[0], None, "ordinal 0 elided");
        assert_eq!(lens[1], None, "ordinal 1 never carried reasoning");
        assert_eq!(lens[2], None, "ordinal 2 elided past the filtered turn");
        assert!(lens[3..].iter().all(Option::is_some));
    }

    /// Frozen reasoning elisions re-apply byte-stably: the grown
    /// session projects the frozen prefix verbatim and the fresh pass
    /// stays behind the reclaim gate, so nothing re-decides.
    #[test]
    fn frozen_reasoning_elisions_reapply_byte_stable() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 1..=12 {
            items.push(assistant_with_reasoning(
                &format!("turn {n}"),
                &big("think", 30),
            ));
        }
        let session = session_with(items.clone());
        let config = PruneConfig {
            elide_reasoning: Some(8),
            ..firing()
        };
        let first = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        items.push(assistant_with_reasoning("turn 13", &big("think", 30)));
        let grown = session_with(items);
        let second = compiler(config)
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        assert_eq!(
            second.request.messages.len(),
            first.request.messages.len() + 1
        );
        for (frozen, regrown) in first
            .request
            .messages
            .iter()
            .zip(second.request.messages.iter())
        {
            assert_eq!(
                serde_json::to_string(frozen).expect("serialize"),
                serde_json::to_string(regrown).expect("serialize"),
                "the frozen reasoning elision re-applies without churn"
            );
        }
        assert_eq!(second.prune_checkpoint.as_ref(), Some(&checkpoint));
    }

    /// Reasoning elisions attribute under `assistant:{ordinal}` with
    /// the trace's estimated reclaim, exactly once per freeze.
    #[test]
    fn reasoning_elision_reports_attribution_once() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 1..=12 {
            items.push(assistant_with_reasoning(
                &format!("turn {n}"),
                &big("think", 30),
            ));
        }
        let session = session_with(items.clone());
        let config = PruneConfig {
            elide_reasoning: Some(8),
            ..firing()
        };
        let first = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let report = first.prune_report.as_ref().expect("freeze reported");
        for ordinal in 0..3 {
            assert!(
                report
                    .calls
                    .iter()
                    .any(|call| call.call_id == format!("assistant:{ordinal}")
                        && call.reason == StubReason::ElidedReasoning
                        && call.reclaimed_tokens > 0),
                "ordinal {ordinal} attributes"
            );
        }
        assert_eq!(
            report.reclaimed_tokens,
            report
                .calls
                .iter()
                .map(|call| call.reclaimed_tokens)
                .sum::<u64>()
        );
        items.push(assistant_with_reasoning("turn 13", &big("think", 30)));
        let grown = session_with(items);
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        let second = compiler(config)
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        assert!(second.prune_report.is_none(), "re-apply reports nothing");
    }

    /// Pass 9 signature: first 200 characters plus a marker naming the
    /// full length — a pure function of the text, never of config.
    #[test]
    fn elide_text_replaces_with_leading_signature() {
        let text = "y".repeat(50) + &"z".repeat(950);
        let elided = elide_text(&text);
        assert!(elided.starts_with(&"y".repeat(50)));
        assert_eq!(elided.chars().count(), 200 + " [pruned 1000 chars]".len());
        assert!(elided.ends_with("[pruned 1000 chars]"));
    }

    /// The rescue arm bakes an elision into its view and the frozen
    /// re-apply then runs over that projection: re-eliding must be a
    /// no-op, never a nested marker.
    #[test]
    fn elide_text_is_idempotent() {
        let text = big("body", 4);
        assert_eq!(elide_text(&elide_text(&text)), elide_text(&text));
    }

    /// Texts inside the keep window plus marker margin pass through
    /// untouched, and multibyte content never splits a UTF-8 boundary.
    #[test]
    fn elide_text_leaves_short_text_alone() {
        let short = "s".repeat(250);
        assert_eq!(elide_text(&short), short);
        let edge = "e".repeat(265);
        assert!(elide_text(&edge).contains("[pruned 265 chars]"));
        let unicode = "é".repeat(400);
        let elided = elide_text(&unicode);
        assert!(elided.starts_with(&"é".repeat(180)));
        assert!(elided.ends_with("[pruned 400 chars]"));
    }

    /// An aged, long assistant text reduces to its signature once the
    /// window trigger fires; the projection keeps the leading 200
    /// characters and the marker, never the body.
    #[test]
    fn aged_long_assistant_text_elides_when_the_window_fires() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_text(&big("prose", 120)),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 100)),
        ];
        for turn in 2..=10 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items);
        let first = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.as_ref().expect("window fired");
        assert_eq!(checkpoint.elided_assistant_text, vec![0]);
        let projected = messages_serialized(&first.request.messages);
        assert!(
            projected.contains("[pruned 122885 chars]"),
            "marker present"
        );
        assert!(
            !projected.contains(&format!("prose{}", "x".repeat(220))),
            "prose body gone (bulk read may keep its own x's)"
        );
        assert!(projected.contains("bulkxxxx"), "protected tail intact");
        let report = first.prune_report.as_ref().expect("freeze reported");
        assert!(
            report.calls.iter().any(|call| call.call_id == "assistant:0"
                && call.reason == StubReason::ElidedAssistantText
                && call.reclaimed_tokens > 0),
            "assistant text attributes"
        );
    }

    /// Without the window trigger (and no budget rescue), aged prose is
    /// never rewritten — the pass is a last-resort lever, not routine.
    #[test]
    fn assistant_text_elision_waits_for_the_window_trigger() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_text(&big("prose", 40)),
        ];
        for turn in 1..=10 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(
            messages_serialized(&compiled.request.messages).contains(&"x".repeat(220)),
            "full prose survives"
        );
    }

    /// Long but recent (inside the elision window) text is left alone
    /// even when the window trigger fires for other history.
    #[test]
    fn recent_assistant_text_is_never_elided() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_text("old short note"),
            assistant_calls("bulk read", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 120)),
        ];
        for turn in 2..=9 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        items.push(assistant_text(&big("recent prose", 100)));
        let session = session_with(items);
        let compiled = compiler(firing())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.as_ref().expect("fired");
        assert!(checkpoint.elided_assistant_text.is_empty());
        assert!(
            messages_serialized(&compiled.request.messages).contains(&"x".repeat(220)),
            "recent prose intact"
        );
    }

    /// The frozen elision re-applies without churn as the session grows.
    #[test]
    fn elided_assistant_text_freezes_and_reapplies_bytewise() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_text(&big("prose", 120)),
            assistant_calls("later bulk", vec![read_call("c2", "src/b.rs")]),
            result("c2", &big("bulk", 100)),
        ];
        for turn in 2..=10 {
            items.push(assistant_text(&format!("filler {turn}")));
        }
        let config = firing();
        let session = session_with(items.clone());
        let first = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("fired");
        items.push(assistant_text("one more turn"));
        let grown = session_with(items);
        let second = compiler(config)
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        assert!(second.prune_report.is_none(), "re-apply reports nothing");
        assert!(
            messages_serialized(&second.request.messages).contains("[pruned 122885 chars]"),
            "frozen elision still applied"
        );
        assert_eq!(second.prune_checkpoint.as_ref(), Some(&checkpoint));
    }

    /// The handoff view leads with the opening brief, carries the
    /// handoff as a user item, then keeps the tail from `upto_item` on.
    #[test]
    fn handoff_session_is_brief_then_handoff_then_tail() {
        let items = vec![
            Item::User {
                text: "brief".to_owned(),
            },
            assistant_text("middle one"),
            assistant_text("middle two"),
            assistant_text("the tail"),
        ];
        let view = handoff_session(&session_with(items), "HANDOFF", 2);
        assert_eq!(view.transcript.len(), 4);
        assert!(matches!(&view.transcript[0], Item::User { text } if text == "brief"));
        assert!(matches!(&view.transcript[1], Item::User { text } if text == "HANDOFF"));
        assert!(matches!(&view.transcript[2], Item::Assistant(a) if a.content == "middle two"));
        assert!(matches!(&view.transcript[3], Item::Assistant(a) if a.content == "the tail"));
    }

    /// A transcript that does not open on a user item still gets a valid
    /// view (handoff alone leads), and `upto_item` clamps to the end.
    #[test]
    fn handoff_session_handles_odd_openings_and_clamps() {
        let items = vec![
            assistant_text("first"),
            assistant_text("second"),
            Item::User {
                text: "brief".to_owned(),
            },
        ];
        let view = handoff_session(&session_with(items), "HANDOFF", 99);
        assert_eq!(view.transcript.len(), 1);
        assert!(matches!(&view.transcript[0], Item::User { text } if text == "HANDOFF"));
    }

    /// An LLM handoff checkpoint projects the view (brief + handoff +
    /// tail), applies only call-id-keyed elisions — ordinal-keyed ones
    /// would misfire on the renumbered view — and stays byte-stable.
    #[test]
    fn handoff_checkpoint_projects_view_and_skips_ordinal_elisions() {
        let items = vec![
            Item::User {
                text: "the brief".to_owned(),
            },
            assistant_text(&big("forgotten prose", 2)),
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("gone", 2)),
            assistant_calls("tail read", vec![read_call("c2", "src/tail.rs")]),
            result("c2", &big("tail content", 1)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let checkpoint = PruneCheckpoint {
            upto_item: 4,
            stubs: Vec::new(),
            elided_args: vec![ArgElision {
                call_id: "c2".to_owned(),
                arguments: ToolArguments::try_from(json!({
                    "path": "src/tail.rs",
                    "content": "[pruned 1229 chars]"
                }))
                .expect("args"),
            }],
            // Original ordinal 1 is the handoff item's ordinal in the
            // view: applying ordinal elisions under handoff would eat
            // the handoff itself.
            elided_users: vec![1],
            elided_reasoning: Vec::new(),
            elided_assistant_text: Vec::new(),
            handoff: Some("HANDOFF SUMMARY".to_owned()),
        };
        let config = PruneConfig::default();
        let compiled = compiler(config)
            .compile(input(&session, Some(&checkpoint)))
            .expect("compiles");
        let projected = messages_serialized(&compiled.request.messages);
        assert!(projected.contains("the brief"), "brief leads the view");
        assert!(projected.contains("HANDOFF SUMMARY"), "handoff survives");
        assert!(
            projected.contains("tail contentxxxx"),
            "tail from upto_item on is kept"
        );
        assert!(
            !projected.contains("forgotten prosex"),
            "pre-handoff history is summarized away"
        );
        assert!(
            projected.contains("[pruned 1229 chars]"),
            "call-id-keyed arg elision applied"
        );
        // Byte-stable: the same checkpoint compiles to the same bytes.
        let again = compiler(config)
            .compile(input(&session, Some(&checkpoint)))
            .expect("compiles");
        assert_eq!(projected, messages_serialized(&again.request.messages));
        // The checkpoint is preserved (handoff and frozen elisions
        // alike), never churned by a re-decide over the view.
        assert_eq!(again.prune_checkpoint.as_ref(), Some(&checkpoint));
        assert_eq!(compiled.prune_checkpoint.as_ref(), Some(&checkpoint));
    }

    /// Checkpoints written before item 12 (no `handoff`, no
    /// `elided_assistant_text`) replay unchanged via serde defaults.
    #[test]
    fn old_checkpoint_json_defaults_handoff_none() {
        let legacy = serde_json::json!({
            "upto_item": 3,
            "stubs": [],
            "elided_args": [],
            "elided_users": [],
            "elided_reasoning": [],
        });
        let checkpoint: PruneCheckpoint =
            serde_json::from_value(legacy).expect("legacy checkpoint replays");
        assert_eq!(checkpoint.handoff, None);
        assert!(checkpoint.elided_assistant_text.is_empty());
    }
}
