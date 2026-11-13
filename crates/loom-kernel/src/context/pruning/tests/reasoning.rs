use super::*;
use crate::context::ContextCompiler;
use crate::model::Message;

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
            Message::System { .. } | Message::User { .. } | Message::ToolResult(_) => None,
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
/// a provider contract on the `OpenAI` wire, so the kernel default
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
