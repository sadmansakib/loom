//! Operator `/context` actions: a dropped result is stubbed below the
//! window trigger and outside the reclaim gate, and a pinned result is
//! never stubbed by any heuristic.

use super::*;
use crate::context::{ContextCompiler, PruneTrigger, StubReason};

fn compiler_with(ops: &PruneOpsHandle, config: PruneConfig) -> PruningContextCompiler {
    compiler(config).with_operator_handle(ops.clone())
}

#[test]
fn operator_drop_stubs_below_the_window_trigger() {
    let session = session_with(vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("reading", vec![read_call("call_1", "src/a.rs")]),
        result("call_1", &big("content", 30)),
        assistant_text("done"),
    ]);
    // A fresh compiler: no window trigger, no age arm, no frozen
    // checkpoint — the drop alone must force the decision.
    let handle = PruneOpsHandle::default();
    handle.drop("call_1");
    let compiled = compiler_with(&handle, PruneConfig::default())
        .compile(input(&session, None))
        .expect("compiles");

    let checkpoint = compiled
        .prune_checkpoint
        .expect("drop freezes a checkpoint");
    assert_eq!(checkpoint.stubs.len(), 1);
    assert_eq!(checkpoint.stubs[0].call_id, "call_1");
    assert_eq!(checkpoint.stubs[0].reason, StubReason::OperatorDropped);
    let report = compiled.prune_report.expect("attribution for the drop");
    assert_eq!(report.trigger, PruneTrigger::Operator);
    assert!(
        compiled.request.messages.iter().any(|message| matches!(
            message,
            crate::model::Message::ToolResult(result)
                if result.content.starts_with("[pruned] result omitted by the operator")
        )),
        "the projection carries the operator stub"
    );
}

#[test]
fn a_frozen_checkpoint_is_not_re_frozen_for_the_same_drop() {
    let session = session_with(vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("reading", vec![read_call("call_1", "src/a.rs")]),
        result("call_1", &big("content", 30)),
        assistant_text("done"),
    ]);
    let handle = PruneOpsHandle::default();
    handle.drop("call_1");
    let agent = compiler_with(&handle, PruneConfig::default());
    let first = agent.compile(input(&session, None)).expect("compiles");
    let checkpoint = first.prune_checkpoint.expect("frozen");
    let second = agent
        .compile(input(&session, Some(&checkpoint)))
        .expect("recompiles");
    assert!(
        second.prune_report.is_none(),
        "a checkpoint already carrying the drop stays frozen"
    );
}

#[test]
fn pin_keeps_a_duplicate_read_out_of_the_decision_set() {
    use crate::context::HeuristicEstimator;
    use crate::context::pruning::reclaim::projected_reclaim;
    use crate::context::pruning::strategies::decide;

    let session = session_with(vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("first read", vec![read_call("call_1", "src/a.rs")]),
        result("call_1", &big("content", 120)),
        assistant_calls("second read", vec![read_call("call_2", "src/a.rs")]),
        result("call_2", &big("content", 120)),
        assistant_text("done"),
        assistant_text("filler"),
        assistant_text("filler"),
        assistant_text("filler"),
        assistant_text("filler"),
    ]);
    let registry = registry();
    let estimator = HeuristicEstimator;

    let unpinned = decide(
        &session,
        &registry,
        firing(),
        &estimator,
        true,
        &PruneOps::default(),
    );
    assert!(
        unpinned.stubbed.iter().any(|stub| stub.call_id == "call_1"),
        "without a pin the older duplicate is stubbed"
    );

    let handle = PruneOpsHandle::default();
    handle.pin("call_1");
    let pinned = decide(
        &session,
        &registry,
        firing(),
        &estimator,
        true,
        &handle.snapshot(),
    );
    assert!(
        pinned.stubbed.iter().all(|stub| stub.call_id != "call_1"),
        "the pinned duplicate is not stubbed: {:?}",
        pinned.stubbed
    );
    assert!(
        projected_reclaim(&session, &pinned, &estimator)
            < projected_reclaim(&session, &unpinned, &estimator),
        "the pin removes the duplicate's reclaim"
    );
}

#[test]
fn a_drop_with_no_committed_result_is_inert() {
    let session = session_with(vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_text("done"),
    ]);
    let handle = PruneOpsHandle::default();
    handle.drop("call_missing");
    let compiled = compiler_with(&handle, PruneConfig::default())
        .compile(input(&session, None))
        .expect("compiles");
    assert!(
        compiled.prune_checkpoint.is_none(),
        "nothing to stub, so nothing freezes"
    );
}
