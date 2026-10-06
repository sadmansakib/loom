use loom_eval_b11::*;

/// A context parked above the trigger: nine fat messages plus the
/// recent pair, already carrying the maximal checkpoint.
fn stationary() -> (Vec<u64>, Checkpoint) {
    let messages = vec![200; 9];
    let carried = Checkpoint { stubbed_through: optimal_stub_through(&messages) };
    (messages, carried)
}

#[test]
fn stationary_context_carries_the_checkpoint() {
    let (messages, carried) = stationary();
    assert!(messages.iter().sum::<u64>() > TRIGGER_TOKENS);
    assert_eq!(
        decide(&messages, Some(&carried)),
        Decision::Carry(carried),
        "a fresh checkpoint reclaims nothing more; re-adopting only busts the prefix cache"
    );
}

#[test]
fn repeated_decisions_are_stable() {
    let (messages, _) = stationary();
    let Decision::Adopt(first) = decide(&messages, None) else {
        panic!("the first crossing must adopt a checkpoint");
    };
    assert_eq!(decide(&messages, Some(&first)), Decision::Carry(first));
}

#[test]
fn below_the_trigger_keeps_everything() {
    let messages = vec![100; 5];
    assert_eq!(decide(&messages, None), Decision::Keep);
}

#[test]
fn first_crossing_adopts_a_checkpoint() {
    let (messages, _) = stationary();
    let Decision::Adopt(checkpoint) = decide(&messages, None) else {
        panic!("above the trigger with nothing carried, pruning must adopt");
    };
    assert!(stub_reclaim(&messages, checkpoint.stubbed_through) >= MIN_RECLAIM_TOKENS);
}

#[test]
fn reclaim_subtracts_stub_costs() {
    assert_eq!(stub_reclaim(&[100, 100], 2), 196);
    assert_eq!(stub_reclaim(&[100, 100], 0), 0);
}
