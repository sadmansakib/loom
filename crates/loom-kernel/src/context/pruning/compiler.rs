use std::sync::Arc;

use crate::model::Message;
use crate::session::Session;

use super::config::PruneConfig;
use super::projection::{apply_elisions, apply_stubs, handoff_session, stubbed_session};
use super::reclaim::{frozen_reclaim, projected_reclaim, prune_report};
use super::strategies::decide;
use super::types::{PruneReport, PruneTrigger};
use crate::context::default::census;
use crate::context::{
    CompiledContext, ContextCompiler, ContextError, ContextInput, HeuristicEstimator,
    PruneCheckpoint, TokenEstimator, estimate_serialized,
};

/// A [`ContextCompiler`] that prunes projected tool results once the
/// estimated input crosses the window-relative trigger
/// ([`PruneConfig::trigger_tokens`]): `trigger_percent` of the model's input
/// budget, floored at `floor_tokens`.
pub struct PruningContextCompiler {
    inner: Box<dyn ContextCompiler>,
    config: PruneConfig,
    estimator: Arc<dyn TokenEstimator>,
    /// Reclaim total for the currently frozen checkpoint. Frozen call
    /// ids and transcript ordinals do not change as the session grows.
    frozen_reclaim_cache: std::sync::Mutex<Option<(PruneCheckpoint, u64)>>,
}

impl PruningContextCompiler {
    /// Wraps `inner` (the shipped
    /// [`DefaultContextCompiler`](crate::context::DefaultContextCompiler)) with the
    /// given thresholds, estimating with the kernel's conservative
    /// bytes ÷ 3 heuristic.
    #[must_use]
    pub fn new(inner: Box<dyn ContextCompiler>, config: PruneConfig) -> Self {
        Self {
            inner,
            config,
            estimator: Arc::new(HeuristicEstimator),
            frozen_reclaim_cache: std::sync::Mutex::new(None),
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

    /// Reuses frozen-decision reclaim across turns. A poisoned cache is
    /// recovered and recomputed; cache state never affects correctness.
    fn cached_frozen_reclaim(
        &self,
        session: &Session,
        frozen: Option<&PruneCheckpoint>,
        estimator: &dyn TokenEstimator,
    ) -> u64 {
        let Some(frozen) = frozen else {
            return 0;
        };
        let mut cache = self
            .frozen_reclaim_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_checkpoint, reclaimed)) = cache.as_ref()
            && cached_checkpoint == frozen
        {
            return *reclaimed;
        }
        let reclaimed = frozen_reclaim(session, frozen, estimator);
        *cache = Some((frozen.clone(), reclaimed));
        reclaimed
    }
}

impl ContextCompiler for PruningContextCompiler {
    // Layered wrapper: capture, estimate, rescue-arm, handoff retry —
    // one linear protocol; a split would pass its locals by reference.
    #[allow(clippy::too_many_lines)]
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
                        // Take the previous checkpoint so the frozen handoff
                        // moves into the re-freeze instead of cloning its
                        // summary text.
                        let previous = checkpoint.take();
                        checkpoint = Some(PruneCheckpoint {
                            // Under a handoff the tail start is the point
                            // the handoff summarized up to; a re-freeze
                            // must not move it past the summarized region.
                            upto_item: if handoff_active {
                                previous.as_ref().map_or(0, |frozen| frozen.upto_item)
                            } else {
                                session.transcript.len()
                            },
                            stubs: decisions.stubbed,
                            elided_args: decisions.elided_args,
                            elided_users: decisions.elided_users,
                            elided_reasoning: decisions.elided_reasoning,
                            elided_assistant_text: decisions.elided_assistant_text,
                            handoff: previous.and_then(|frozen| frozen.handoff),
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
        let phase_masking = self.config.phase_mask_turns.is_some();
        let elision_enabled = self.config.elide_turns.is_some()
            || self.config.elide_reasoning.is_some()
            || self.config.elide_assistant_text.is_some();
        if window_triggered || age_masking || phase_masking || elision_enabled {
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
            // Review item 3.2: the gate reads the reclaim total alone;
            // the per-call attribution (`prune_report`) is only built
            // once the gate passes, so a steady-state aging turn — the
            // common case — pays one estimate walk instead of two.
            // The frozen side is memoized across turns for the same
            // checkpoint; see `cached_frozen_reclaim`.
            let candidate_reclaim = projected_reclaim(session, &decisions, &*self.estimator);
            let already =
                self.cached_frozen_reclaim(session, checkpoint.as_ref(), &*self.estimator);
            if candidate_reclaim
                >= already.saturating_add(u64::from(self.config.min_reclaim_tokens))
            {
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
                debug_assert_eq!(
                    candidate_report.reclaimed_tokens, candidate_reclaim,
                    "the gate and the attribution share one walk"
                );
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
                // Same move of the frozen handoff as the rescue re-freeze.
                let previous = checkpoint.take();
                checkpoint = Some(PruneCheckpoint {
                    upto_item: if handoff_active {
                        previous.as_ref().map_or(0, |frozen| frozen.upto_item)
                    } else {
                        session.transcript.len()
                    },
                    stubs: decisions.stubbed,
                    elided_args: decisions.elided_args,
                    elided_users: decisions.elided_users,
                    elided_reasoning: decisions.elided_reasoning,
                    elided_assistant_text: decisions.elided_assistant_text,
                    handoff: previous.and_then(|frozen| frozen.handoff),
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

// `ContextCensus` is reconstructed via `census` in `compile`.
