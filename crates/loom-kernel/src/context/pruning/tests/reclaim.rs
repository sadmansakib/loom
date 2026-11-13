use super::*;
use crate::context::messages_serialized;
use crate::context::{ContextCompiler, ContextError};
use crate::model::Message;

/// A budget tight enough that the *inner* compiler refuses the
/// unpruned projection — the situation the rescue exists for, where
/// the trigger never got the chance to fire below the wall.
fn tight() -> ContextBudget {
    ContextBudget {
        max_input_tokens: 60_000,
        reserved_output_tokens: 4_096,
    }
}

fn messages_contain(request: &crate::model::ModelRequest, needle: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| messages_serialized(std::slice::from_ref(message)).contains(needle))
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
    let checkpoint = first.prune_checkpoint.expect("fired");
    items.push(assistant_text("one more turn"));
    let session = session_with(items);
    let second = compiler(PruneConfig::default())
        .compile(input(&session, Some(&checkpoint)))
        .expect("compiles");
    // The frozen stub still applies and no new decision churned the
    // older history: the projected prefix is unchanged.
    assert!(
        messages_serialized(&second.request.messages).contains("older than the kept turn window")
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
                arguments: ToolArguments::try_from(json!({ "path": "src/a.rs", "content": "x" }))
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
    let checkpoint = first.prune_checkpoint.expect("triggered");

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
