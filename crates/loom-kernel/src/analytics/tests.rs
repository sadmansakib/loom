use std::collections::HashSet;

use serde_json::json;

use super::*;
use crate::artifact::ArtifactRef;
use crate::event::{EventEnvelope, EventKind};
use crate::id::{ArtifactId, SessionId};
use crate::model::StopReason;
use crate::session::Session;
use crate::store::{EventStore, SessionStore};
use crate::tool::ToolEffects;
use crate::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolFailure, ToolResult, ToolStatus,
};

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(args).expect("arguments"),
    }
}

fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    }
}

fn ok_result(id: &str, content: &str) -> ToolResult {
    ToolResult::new(
        crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Success,
        content,
        false,
    )
}

fn failed_result(id: &str, failure: ToolFailure) -> ToolResult {
    ToolResult::new(
        crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Error(failure),
        "boom",
        false,
    )
}

fn session(items: Vec<Item>) -> Session {
    Session {
        id: crate::id::SessionId::new(),
        transcript: items,
        tasks: Vec::new(),
        todos: crate::session::TodoList::default(),
    }
}

#[test]
fn repairs_are_counted_from_effects() {
    let mut repaired = ok_result("a", "edited f.rs");
    repaired.effects = ToolEffects::none().with_repair("old_string matched after trimming");
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![call("a", "edit_file", json!({"path": "f.rs"}))],
        )),
        Item::ToolResult(repaired),
    ];
    let stats = session_stats(&session(items));
    assert_eq!(stats.repairs, 1);
}

#[test]
fn counts_statuses_per_tool_and_in_total() {
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![
                call("a", "read_file", json!({"path": "x"})),
                call("b", "edit_file", json!({"path": "y"})),
                call("c", "run_command", json!({"program": "cargo"})),
            ],
        )),
        Item::ToolResult(ok_result("a", "contents")),
        Item::ToolResult(failed_result("b", ToolFailure::InvalidInput)),
        Item::ToolResult(failed_result("c", ToolFailure::Execution)),
        Item::Assistant(assistant(
            "again",
            vec![call("d", "read_file", json!({"path": "z"}))],
        )),
        Item::ToolResult(failed_result("d", ToolFailure::RepeatedCall)),
    ];
    let stats = session_stats(&session(items));

    assert_eq!(stats.turns, 2);
    let reads = &stats.tools["read_file"];
    assert_eq!((reads.calls, reads.success, reads.repeated), (2, 1, 1));
    assert_eq!(stats.tools["edit_file"].invalid_input, 1);
    assert_eq!(stats.tools["run_command"].execution, 1);
    assert_eq!(stats.totals.calls, 4);
    assert_eq!(stats.totals.failed, 3);
    assert_eq!(stats.malformed_calls, 1);
}

#[test]
fn identical_repeats_count_executed_duplicates_too() {
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![call("a", "read_file", json!({"path": "x"}))],
        )),
        Item::ToolResult(ok_result("a", "contents")),
        Item::Assistant(assistant(
            "other",
            vec![call("b", "edit_file", json!({"path": "y"}))],
        )),
        Item::ToolResult(ok_result("b", "done")),
        // Same tool AND same canonical arguments as call a, but not
        // adjacent: the ledger lets it through, the stats do not.
        Item::Assistant(assistant(
            "again",
            vec![call("c", "read_file", json!({"path": "x"}))],
        )),
        Item::ToolResult(ok_result("c", "contents")),
    ];
    let stats = session_stats(&session(items));
    assert_eq!(stats.identical_repeats, 1);
    // Canonical keying: same path, different key order in the JSON
    // literal, still identical once serialized.
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![call("a", "read_file", json!({"path": "x", "offset": 0}))],
        )),
        Item::ToolResult(ok_result("a", "contents")),
        Item::Assistant(assistant(
            "again",
            vec![call("b", "read_file", json!({"offset": 0, "path": "x"}))],
        )),
        Item::ToolResult(ok_result("b", "contents")),
    ];
    let stats = session_stats(&session(items));
    assert_eq!(stats.identical_repeats, 1);
}

#[test]
fn edit_acceptance_and_spill_savings_are_derived() {
    let mut spilled = ok_result("a", "[artifact: 64 KiB body omitted]");
    spilled.artifact = Some(ArtifactRef::new(
        ArtifactId::new(),
        "tool_output",
        65_536,
        "deadbeef",
    ));
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![
                call("a", "run_command", json!({"program": "cargo"})),
                call("b", "write_file", json!({"path": "y"})),
                call("c", "edit_file", json!({"path": "z"})),
            ],
        )),
        Item::ToolResult(spilled),
        Item::ToolResult(ok_result("b", "written")),
        Item::ToolResult(failed_result("c", ToolFailure::Execution)),
    ];
    let stats = session_stats(&session(items));

    assert_eq!(stats.edit_apply.attempts, 2);
    assert_eq!(stats.edit_apply.applied, 1);
    assert_eq!(stats.spills.len(), 1);
    let spill = &stats.spills[0];
    assert_eq!(spill.tool, "run_command");
    assert_eq!(spill.saved_bytes(), 65_536 - spilled_content_len(spill));
}

/// The stub length the test above planted, for the savings assert.
fn spilled_content_len(spill: &Spill) -> u64 {
    spill.delivered_bytes
}

#[test]
fn skill_telemetry_counts_loads_resources_and_cost_per_skill() {
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![
                call("a", "load_skill", json!({"name": "release"})),
                call("b", "load_skill", json!({"name": "release"})),
                call(
                    "c",
                    "load_skill",
                    json!({"name": "deploy", "resource": "notes.md"}),
                ),
                call("d", "load_skill", json!({"name": "ghost"})),
                call("e", "load_skill", json!({"resource": "no name"})),
            ],
        )),
        Item::ToolResult(ok_result("a", "# skill: release\none")),
        Item::ToolResult(ok_result("b", "# skill: release\ntwo")),
        Item::ToolResult(ok_result("c", "# skill resource: deploy/notes.md\nport")),
        Item::ToolResult(failed_result("d", ToolFailure::InvalidInput)),
        Item::ToolResult(failed_result("e", ToolFailure::InvalidInput)),
    ];
    let stats = session_stats(&session(items));

    let release = &stats.skills["release"];
    assert_eq!(
        (release.loads, release.resources, release.failures),
        (2, 0, 0)
    );
    assert_eq!(
        release.delivered_bytes,
        ("# skill: release\none".len() + "# skill: release\ntwo".len()) as u64
    );
    let deploy = &stats.skills["deploy"];
    assert_eq!((deploy.loads, deploy.resources, deploy.failures), (0, 1, 0));
    assert_eq!(
        deploy.delivered_bytes,
        "# skill resource: deploy/notes.md\nport".len() as u64
    );
    // An unknown name is a failure for the name the model asked
    // for — the retirement signal for stale menu entries.
    let ghost = &stats.skills["ghost"];
    assert_eq!((ghost.loads, ghost.failures), (0, 1));
    // Malformed arguments still reconcile, under the "?" key.
    assert_eq!(stats.skills["?"].failures, 1);

    // Folding merges per-skill counters like per-tool ones.
    let mut folded = stats.clone();
    folded.add(&stats);
    assert_eq!(folded.skills["release"].loads, 4);
    assert_eq!(folded.skills["ghost"].failures, 2);
}

#[test]
fn retirement_verdicts_follow_the_evidence() {
    let skill = |loads, resources, failures, delivered_bytes| SkillStats {
        loads,
        resources,
        failures,
        delivered_bytes,
    };

    // Healthy and used: keep.
    assert_eq!(
        skill(10, 2, 1, 20_000).retirement_verdict(),
        SkillVerdict::Keep
    );
    // Rare but cheap and never failing: keep.
    assert_eq!(skill(3, 0, 0, 900).retirement_verdict(), SkillVerdict::Keep);

    // Expensive relative to rare use: watch.
    assert_eq!(
        skill(2, 0, 0, 5_000).retirement_verdict(),
        SkillVerdict::Watch
    );
    assert_eq!(
        skill(0, 1, 0, 2_000).retirement_verdict(),
        SkillVerdict::Watch
    );

    // Only failures (stale menu entry / broken skill): retire.
    assert_eq!(skill(0, 0, 3, 0).retirement_verdict(), SkillVerdict::Retire);
    // Failures outweigh successes on real evidence: retire.
    assert_eq!(
        skill(1, 0, 5, 100).retirement_verdict(),
        SkillVerdict::Retire
    );
    // Bytes with no successful use at all: retire.
    assert_eq!(
        skill(0, 0, 0, 500).retirement_verdict(),
        SkillVerdict::Retire
    );
    // No bytes, no calls: nothing to judge, keep.
    assert_eq!(skill(0, 0, 0, 0).retirement_verdict(), SkillVerdict::Keep);
}

#[test]
fn verdicts_fold_in_name_order() {
    let mut stats = SessionStats::default();
    stats.skills.insert(
        "b-healthy".to_owned(),
        SkillStats {
            loads: 12,
            ..Default::default()
        },
    );
    stats.skills.insert(
        "a-ghost".to_owned(),
        SkillStats {
            failures: 2,
            ..Default::default()
        },
    );
    stats.skills.insert(
        "c-pricey".to_owned(),
        SkillStats {
            loads: 1,
            delivered_bytes: 9_000,
            ..Default::default()
        },
    );
    // The "?" key (malformed arguments) is no skill to keep.
    stats.skills.insert(
        "?".to_owned(),
        SkillStats {
            failures: 1,
            ..Default::default()
        },
    );

    let verdicts = skill_retirement_verdicts(&stats);
    assert_eq!(
        verdicts,
        vec![
            ("?".to_owned(), SkillVerdict::Retire),
            ("a-ghost".to_owned(), SkillVerdict::Retire),
            ("b-healthy".to_owned(), SkillVerdict::Keep),
            ("c-pricey".to_owned(), SkillVerdict::Watch),
        ]
    );
    assert_eq!(verdicts[0].1.to_string(), "retire");
    assert_eq!(verdicts[2].1.to_string(), "keep");
    assert_eq!(verdicts[3].1.to_string(), "watch");
}

#[test]
fn an_empty_transcript_yields_zeroed_stats() {
    let stats = session_stats(&session(Vec::new()));
    assert_eq!(stats.turns, 0);
    assert_eq!(stats.totals.calls, 0);
    assert!(stats.tools.is_empty());
    assert_eq!(stats.spills, Vec::new());
}

#[test]
fn sessions_overview_folds_totals_across_the_store() {
    use crate::store::MemorySessionStore;

    // Seeds one session through the store's own seams: plan the turn
    // against a local mirror, persist, then apply — the same dance
    // the loop performs per committed turn.
    fn seed(
        store: &MemorySessionStore,
        calls: Vec<ToolCall>,
        results: Vec<ToolResult>,
    ) -> SessionId {
        let mut session = Session::new(SessionId::new());
        store.ensure_session(&session.id).unwrap();
        let user = Item::User {
            text: "task".into(),
        };
        let index = session.push_item(user.clone()).unwrap();
        store.append_item(&session.id, index, &user).unwrap();

        let planned = session.plan_turn(assistant("go", calls), results).unwrap();
        store.append_turn(&session.id, &planned).unwrap();
        session.apply_planned_turn(planned);
        session.id
    }

    let store = MemorySessionStore::new();
    let first = seed(
        &store,
        vec![call("a", "read_file", json!({"path": "x"}))],
        vec![ok_result("a", "contents")],
    );
    let second = seed(
        &store,
        vec![call("b", "run_command", json!({"program": "cargo"}))],
        vec![failed_result("b", ToolFailure::Execution)],
    );

    let overview = sessions_overview(&store, None).unwrap();
    assert_eq!(overview.rows.len(), 2);
    assert_eq!(overview.totals.turns, 2);
    assert_eq!(overview.totals.totals.calls, 2);
    assert_eq!(overview.totals.totals.failed, 1);
    let listed: HashSet<SessionId> = overview.rows.iter().map(|row| row.id).collect();
    assert_eq!(listed, HashSet::from([first, second]));
}

#[test]
fn sessions_overview_filters_by_id_substring() {
    use crate::store::MemorySessionStore;

    let only = session(vec![Item::Assistant(assistant("go", Vec::new()))]);
    let store = MemorySessionStore::with_session(only.clone());

    let overview = sessions_overview(&store, Some(&only.id.to_string())).unwrap();
    assert_eq!(overview.rows.len(), 1);
    assert_eq!(overview.rows[0].id, only.id);

    let none = sessions_overview(&store, Some("no-such-id")).unwrap();
    assert_eq!(none.rows, Vec::new());
    assert_eq!(none.totals.turns, 0);
}

#[test]
fn sessions_overview_folds_repairs_into_totals() {
    use crate::store::MemorySessionStore;

    let mut repaired = ok_result("a", "edited f.rs");
    repaired.effects = ToolEffects::none().with_repair("old_string matched after trimming");
    let repaired_session = session(vec![
        Item::Assistant(assistant(
            "go",
            vec![call("a", "edit_file", json!({"path": "f.rs"}))],
        )),
        Item::ToolResult(repaired),
    ]);
    let store = MemorySessionStore::with_session(repaired_session);

    let overview = sessions_overview(&store, None).unwrap();
    assert_eq!(overview.rows[0].stats.repairs, 1);
    // The fold must carry repairs too, or store-wide totals would
    // silently report zero.
    assert_eq!(overview.totals.repairs, 1);
}

/// A synthetic event stream exercising every mechanism-bearing
/// kind — built in-test, no network, no provider.
fn mechanism_events() -> Vec<EventEnvelope> {
    use crate::context::{PruneReport, PruneTrigger, PrunedCall, StubReason};
    use crate::event::EVENT_SCHEMA_VERSION;
    use crate::id::TaskId;

    let task = TaskId::new();
    let envelope = |sequence, kind| EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        sequence,
        task_id: task,
        kind,
    };
    vec![
        envelope(0, EventKind::TaskStarted),
        envelope(
            1,
            EventKind::ContextPruned {
                turn: 4,
                report: PruneReport {
                    trigger: PruneTrigger::AgeMask,
                    calls: vec![
                        PrunedCall {
                            call_id: "c1".to_owned(),
                            reason: StubReason::TurnAge,
                            reclaimed_tokens: 120,
                        },
                        PrunedCall {
                            call_id: "c2".to_owned(),
                            reason: StubReason::TurnAge,
                            reclaimed_tokens: 80,
                        },
                    ],
                    reclaimed_tokens: 200,
                },
            },
        ),
        envelope(
            2,
            EventKind::FastPathHit {
                kind: "rename".to_owned(),
                files_changed: 2,
            },
        ),
        envelope(
            3,
            EventKind::ContextHandoff {
                turn: 9,
                chars: 4_000,
            },
        ),
        // Kinds with no mechanism attribution are ignored.
        envelope(4, EventKind::TaskCompleted),
    ]
}

#[test]
fn session_mechanisms_counts_each_mechanism_honestly() {
    let mut stubbed = ok_result("b", "unchanged since call a");
    stubbed.effects = ToolEffects::none().with_repair("matched after trimming");
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![
                call("a", "read_file", json!({"path": "x"})),
                call("b", "read_file", json!({"path": "x"})),
                call("c", "run_command", json!({"program": "git"})),
            ],
        )),
        Item::ToolResult(ok_result("a", "contents")),
        Item::ToolResult(stubbed),
        // One result carrying two markers: a byte cut and a
        // reducer's counted cap.
        Item::ToolResult(ok_result(
            "c",
            "commit 1\n[... 5 more commits omitted]\n... [300 bytes omitted] ...\ncommit 7",
        )),
    ];
    let stats = session_mechanisms(&session(items), &mechanism_events());

    // Event-derived: prunes with estimator-side tokens, handoff,
    // fast path.
    assert_eq!(stats.prune_events, 1);
    assert_eq!(stats.pruned_calls, 2);
    assert_eq!(stats.reclaimed_tokens_estimate, 200);
    assert_eq!(stats.handoffs, 1);
    assert_eq!(stats.fast_path_hits, 1);
    assert_eq!(stats.fast_path_files_changed, 2);
    // Transcript-derived: one stub hit, two markers, one repair.
    assert_eq!(stats.stub_hits, 1);
    assert_eq!(stats.omission_markers, 2);
    assert_eq!(stats.repairs, 1);
}

#[test]
fn a_session_without_events_still_counts_transcript_mechanisms() {
    // Every session predating event persistence loads like this:
    // no stream, but the transcript's mechanisms still attribute.
    let items = vec![
        Item::Assistant(assistant(
            "go",
            vec![call("b", "read_file", json!({"path": "x"}))],
        )),
        Item::ToolResult(ok_result("b", "unchanged since call a")),
    ];
    let stats = session_mechanisms(&session(items), &[]);
    assert_eq!(stats.stub_hits, 1);
    assert_eq!(stats.omission_markers, 0);
    assert!(
        !stats.is_empty(),
        "the transcript mechanisms still attribute"
    );
}

#[test]
fn mechanisms_overview_folds_across_the_store_and_filters() {
    use crate::store::{MemoryEventStore, MemorySessionStore};

    let events = MemoryEventStore::new();
    let store = MemorySessionStore::new();

    // Seed two sessions through the store's own seams, one with a
    // persisted stream and one without.
    let with_stream = SessionId::new();
    store.ensure_session(&with_stream).unwrap();
    events.ensure_session(&with_stream);
    for envelope in mechanism_events() {
        events.append_event(&with_stream, &envelope).unwrap();
    }
    let mut plain_session = Session::new(SessionId::new());
    store.ensure_session(&plain_session.id).unwrap();
    let planned = plain_session
        .plan_turn(
            assistant("go", vec![call("b", "read_file", json!({"path": "x"}))]),
            vec![ok_result("b", "unchanged since call a")],
        )
        .unwrap();
    store.append_turn(&plain_session.id, &planned).unwrap();
    plain_session.apply_planned_turn(planned);
    let plain = plain_session.id;

    let totals = mechanisms_overview(&store, &events, None).unwrap();
    assert_eq!(totals.prune_events, 1);
    assert_eq!(totals.reclaimed_tokens_estimate, 200);
    assert_eq!(totals.fast_path_hits, 1);
    // The stub hit from the event-less session folded in too.
    assert_eq!(totals.stub_hits, 1);

    // The filter narrows to the same id set `sessions_overview`
    // would keep.
    let only_plain = mechanisms_overview(&store, &events, Some(&plain.to_string())).unwrap();
    assert_eq!(only_plain.prune_events, 0);
    assert_eq!(only_plain.stub_hits, 1);

    let none = mechanisms_overview(&store, &events, Some("no-such-id")).unwrap();
    assert!(none.is_empty());
}

#[test]
fn mechanisms_json_and_line_label_their_semantics() {
    let stats = session_mechanisms(&session(Vec::new()), &mechanism_events());
    let report = mechanisms_json(&stats);
    assert_eq!(report["prune_events"], 1);
    assert_eq!(report["reclaimed_tokens_estimate"], 200);
    assert_eq!(report["stub_hits"], 0);
    assert!(
        report["note"]
            .as_str()
            .expect("note")
            .contains("occurrence count"),
        "counted fields must be labeled as counts, not tokens"
    );

    let line = mechanisms_line(&stats);
    assert!(line.contains("~200 tok estimator-side"), "{line}");
    assert!(line.contains("1 fast-path solve"), "{line}");
    assert!(!line.contains("stub hit"), "{line}");

    assert_eq!(mechanisms_line(&MechanismStats::default()), "none observed");
}
