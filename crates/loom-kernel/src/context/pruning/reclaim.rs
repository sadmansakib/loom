use std::collections::{HashMap, HashSet};

use crate::model::{Message, StopReason};
use crate::session::Session;
use crate::transcript::{Item, ToolArguments, ToolCallId, ToolResult, ToolStatus};

use super::elision::elide_text;
use super::strategies::DecisionSet;
use super::types::{PruneReport, PruneTrigger, PrunedCall, StubReason};
use crate::context::{PruneCheckpoint, TokenEstimator, estimate_serialized};

/// Tokens the already-frozen decisions reclaim from the *current*
/// transcript, so `compile` can gate a fresh checkpoint on the reclaim
/// that is genuinely new (ADR-011: pruning is episodic — one cache miss
/// per checkpoint). Covers result stubs, argument elisions, and user
/// elisions alike: whatever the frozen checkpoint takes off the
/// transcript's back counts.
pub(super) fn frozen_reclaim(
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
pub(super) fn prune_report(
    trigger: PruneTrigger,
    session: &Session,
    decisions: &DecisionSet,
    estimator: &dyn TokenEstimator,
) -> PruneReport {
    let (reclaimed_tokens, calls) = reclaim_walk(session, decisions, estimator, true);
    PruneReport {
        trigger,
        calls,
        reclaimed_tokens,
    }
}

/// The reclaim total alone — the min-reclaim gate's input — without
/// building the per-call attribution (review item 3.2: the gate fails
/// on most aging turns, and a failing turn no longer pays for the
/// `PrunedCall` walk). Shares one walk implementation with
/// [`prune_report`], so the two numbers are equal by construction.
pub(super) fn projected_reclaim(
    session: &Session,
    decisions: &DecisionSet,
    estimator: &dyn TokenEstimator,
) -> u64 {
    reclaim_walk(session, decisions, estimator, false).0
}

/// The single walk behind both [`prune_report`] and
/// [`projected_reclaim`]: per matched decision, full item estimate less
/// its replacement's. `collect` controls whether the `PrunedCall`
/// entries are built (report) or skipped (gate).
fn reclaim_walk(
    session: &Session,
    decisions: &DecisionSet,
    estimator: &dyn TokenEstimator,
    collect: bool,
) -> (u64, Vec<PrunedCall>) {
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
    let mut reclaim = 0u64;
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
                reclaim += u64::from(full.saturating_sub(stub));
                if collect {
                    calls.push(PrunedCall {
                        call_id: result.call_id.as_str().to_owned(),
                        reason,
                        reclaimed_tokens: u64::from(full.saturating_sub(stub)),
                    });
                }
            }
            Item::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    let Some(elided) = elided_calls.get(call.id.as_str()) else {
                        continue;
                    };
                    let full = estimate_serialized(estimator, &call.arguments);
                    let elided_tokens = estimate_serialized(estimator, elided);
                    reclaim += u64::from(full.saturating_sub(elided_tokens));
                    if collect {
                        calls.push(PrunedCall {
                            call_id: call.id.as_str().to_owned(),
                            reason: StubReason::ElidedArgs,
                            reclaimed_tokens: u64::from(full.saturating_sub(elided_tokens)),
                        });
                    }
                }
                if assistant.stop_reason != StopReason::ContentFilter {
                    if elided_reasoning.contains(&assistant_ordinal)
                        && let Some(trace) = &assistant.reasoning
                    {
                        let tokens = u64::from(estimate_serialized(estimator, trace));
                        reclaim += tokens;
                        if collect {
                            calls.push(PrunedCall {
                                call_id: format!("assistant:{assistant_ordinal}"),
                                reason: StubReason::ElidedReasoning,
                                reclaimed_tokens: tokens,
                            });
                        }
                    }
                    if elided_assistant_text.contains(&assistant_ordinal) {
                        let signature = elide_text(&assistant.content);
                        let tokens = u64::from(
                            estimate_serialized(estimator, &assistant.content)
                                .saturating_sub(estimate_serialized(estimator, &signature)),
                        );
                        reclaim += tokens;
                        if collect {
                            calls.push(PrunedCall {
                                call_id: format!("assistant:{assistant_ordinal}"),
                                reason: StubReason::ElidedAssistantText,
                                reclaimed_tokens: tokens,
                            });
                        }
                    }
                    assistant_ordinal += 1;
                }
            }
            Item::User { text } => {
                if elided_users.contains(&user_ordinal) {
                    let full = Message::User { text: text.clone() };
                    let stub = estimate_serialized(estimator, &elided_user_stub);
                    let tokens =
                        u64::from(estimate_serialized(estimator, &full).saturating_sub(stub));
                    reclaim += tokens;
                    if collect {
                        calls.push(PrunedCall {
                            call_id: format!("user:{user_ordinal}"),
                            reason: StubReason::ElidedUser,
                            reclaimed_tokens: tokens,
                        });
                    }
                }
                user_ordinal += 1;
            }
        }
    }
    if collect {
        debug_assert_eq!(
            reclaim,
            calls.iter().map(|call| call.reclaimed_tokens).sum::<u64>(),
            "the running total and the per-call attribution must agree"
        );
    }
    (reclaim, calls)
}

/// Estimated tokens of the stub that would replace this result.
pub(super) fn stub_tokens(
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
        notices: Vec::new(),
    });
    estimate_serialized(estimator, &probe)
}
