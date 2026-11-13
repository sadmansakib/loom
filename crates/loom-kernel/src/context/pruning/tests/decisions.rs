use super::*;
use crate::context::messages_serialized;
use crate::context::pruning::reclaim::{projected_reclaim, prune_report};
use crate::context::pruning::strategies::decide;
use crate::context::{ContextCompiler, HeuristicEstimator};

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

#[test]
fn gate_reclaim_matches_the_report_walk_on_existing_sessions() {
    // Property-style parity check: for the existing session shapes
    // exercised across this module (read stubs, arg/user/reasoning/
    // assistant-text elisions), the cheap gate total must be exactly
    // the full per-call report total. This pins output identity while
    // allowing the failed-gate path to omit attribution allocation.
    let mut items = vec![Item::User {
        text: "task brief".to_owned(),
    }];
    for index in 0..24 {
        let id = format!("call-{index}");
        items.push(assistant_calls(
            "read",
            vec![read_call(&id, &format!("src/{index}.rs"))],
        ));
        items.push(result(&id, &big("body", 4)));
        if index % 3 == 0 {
            items.push(assistant_text(&big("older assistant prose", 1)));
            items.push(Item::User {
                text: big("follow up", 1),
            });
        }
    }
    items.push(assistant_text("current task"));
    let session = session_with(items);
    let registry = registry();
    let estimator = HeuristicEstimator;
    for config in [
        PruneConfig::default(),
        PruneConfig {
            age_mask_turns: Some(2),
            elide_turns: Some(2),
            elide_reasoning: Some(2),
            elide_assistant_text: Some(2),
            ..PruneConfig::default()
        },
    ] {
        let decisions = decide(&session, &registry, config, &estimator, true);
        let gated = projected_reclaim(&session, &decisions, &estimator);
        let reported = prune_report(PruneTrigger::Window, &session, &decisions, &estimator);
        assert_eq!(gated, reported.reclaimed_tokens);
    }
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
        messages_serialized(&compiled.request.messages).contains("older than the kept turn window")
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
    assert_eq!(compiled.stubbed_calls.as_slice(), &[] as &[String]);
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

/// Phase-aware retention (roadmap item 10): with age masking off
/// and a tight phase window (2), a recent write puts the loop in
/// the writing phase and the old exploration read is masked with
/// `WritingPhase`.
#[test]
fn phase_masking_stubs_exploration_reads_during_writing() {
    let items = vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("explore", vec![read_call("c1", "src/other.rs")]),
        result("c1", &big("explore", 100)),
        assistant_text("filler one"),
        assistant_text("filler two"),
        assistant_text("filler three"),
        assistant_calls("editing", vec![write_call("c2", "src/a.rs")]),
        written_result("c2", "src/a.rs"),
        assistant_calls("recent", vec![read_call("c3", "src/b.rs")]),
        result("c3", &big("recent", 100)),
    ];
    let session = session_with(items);
    let compiled = compiler(PruneConfig {
        age_mask_turns: None,
        phase_mask_turns: Some(2),
        ..PruneConfig::default()
    })
    .compile(input(&session, None))
    .expect("compiles");
    let checkpoint = compiled
        .prune_checkpoint
        .as_ref()
        .expect("phase mask fired");
    assert_eq!(
        checkpoint.stubs,
        vec![StubDecision {
            call_id: "c1".to_owned(),
            reason: StubReason::WritingPhase,
        }]
    );
}

/// No write within the detection window: no writing phase, no
/// phase stubs, even with `phase_mask_turns` set.
#[test]
fn phase_masking_silent_without_a_recent_write() {
    let items = vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("editing", vec![write_call("c2", "src/a.rs")]),
        written_result("c2", "src/a.rs"),
        assistant_text("filler one"),
        assistant_calls("explore", vec![read_call("c1", "src/a.rs")]),
        result("c1", &big("explore", 100)),
        assistant_text("filler two"),
        assistant_text("filler three"),
        assistant_text("filler four"),
        assistant_text("filler five"),
    ];
    let session = session_with(items);
    let compiled = compiler(PruneConfig {
        age_mask_turns: None,
        phase_mask_turns: Some(2),
        ..PruneConfig::default()
    })
    .compile(input(&session, None))
    .expect("compiles");
    assert!(compiled.prune_checkpoint.is_none());
    assert!(messages_serialized(&compiled.request.messages).contains("explorexxxx"));
}

/// Default config keeps the shipped behavior: phase-aware
/// retention is off, a recent write plus an old read stubs nothing.
#[test]
fn phase_masking_off_by_default() {
    let items = vec![
        Item::User {
            text: "go".to_owned(),
        },
        assistant_calls("explore", vec![read_call("c1", "src/a.rs")]),
        result("c1", &big("explore", 100)),
        assistant_text("filler one"),
        assistant_text("filler two"),
        assistant_calls("editing", vec![write_call("c2", "src/a.rs")]),
        written_result("c2", "src/a.rs"),
        assistant_text("filler three"),
    ];
    let session = session_with(items);
    let compiled = compiler(PruneConfig::default())
        .compile(input(&session, None))
        .expect("compiles");
    assert!(compiled.prune_checkpoint.is_none());
    assert!(messages_serialized(&compiled.request.messages).contains("explorexxxx"));
}
