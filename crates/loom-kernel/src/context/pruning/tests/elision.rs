use super::*;
use crate::context::ContextCompiler;
use crate::context::messages_serialized;
use crate::context::pruning::elision::elide_text;

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
            text: big("follow-up", 80),
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
    let checkpoint = first.prune_checkpoint.expect("fired");
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
