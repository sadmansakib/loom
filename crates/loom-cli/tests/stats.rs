//! `loom stats` end to end: the binary reads a store it did not create
//! and reports the same transcript analytics the eval harness computes
//! per run — same function, same numbers, zero tokens.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use loom_core::kernel::artifact::ArtifactRef;
use loom_core::kernel::id::{ArtifactId, SessionId};
use loom_core::kernel::model::StopReason;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolFailure, ToolResult, ToolStatus,
};
use loom_core::store::SqliteStore;
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(args).unwrap(),
    }
}

fn result(id: &str, status: ToolStatus, content: &str) -> ToolResult {
    ToolResult::new(
        loom_core::kernel::transcript::ToolCallId::try_from(id.to_owned()).unwrap(),
        status,
        content,
        false,
    )
}

fn turn(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    }
}

/// Mirrors the engine's persist-then-commit flow into the store.
fn commit(
    store: &SqliteStore,
    id: &SessionId,
    session: &mut Session,
    assistant: AssistantMessage,
    results: Vec<ToolResult>,
) {
    let planned = session.plan_turn(assistant, results).unwrap();
    store.append_turn(id, &planned).unwrap();
    session.apply_planned_turn(planned);
}

/// Builds a two-session store: one busy session (success, model-side
/// fault, an oversize spill) and one minimal session.
fn seeded_store(db: &std::path::Path) {
    let store = SqliteStore::open(db).unwrap();

    let busy = SessionId::new();
    let mut session = Session::new(busy);
    store.ensure_session(&busy).unwrap();
    let user = Item::User {
        text: "fix the bug".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&busy, index, &user).unwrap();

    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "reading",
            vec![call("a", "read_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result("a", ToolStatus::Success, "fn main() {}")],
    );
    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "editing",
            vec![call("b", "edit_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result(
            "b",
            ToolStatus::Error(ToolFailure::InvalidInput),
            "bad args",
        )],
    );
    let mut spilled = result("c", ToolStatus::Success, "[artifact: full output omitted]");
    spilled.artifact = Some(ArtifactRef::new(
        ArtifactId::new(),
        "tool_output",
        4_096,
        "cafebabe",
    ));
    commit(
        &store,
        &busy,
        &mut session,
        turn(
            "verifying",
            vec![call("c", "run_command", json!({"program": "cargo"}))],
        ),
        vec![spilled],
    );

    let quiet = SessionId::new();
    let mut session = Session::new(quiet);
    store.ensure_session(&quiet).unwrap();
    commit(
        &store,
        &quiet,
        &mut session,
        turn("done", vec![call("z", "read_file", json!({"path": "x"}))]),
        vec![result("z", ToolStatus::Success, "x")],
    );
}

fn stats(db: &std::path::Path, extra: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .arg("stats")
        .args(extra)
        .env("LOOM_DB", db)
        .output()
        .expect("spawn loom");
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn stats_reports_stored_sessions_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store(&db);

    let (ok, stdout, stderr) = stats(&db, &["--json"]);
    assert!(ok, "loom stats --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sessions = report["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let totals = &report["totals"];
    assert_eq!(totals["turns"], json!(4));
    assert_eq!(totals["calls"], json!(4));
    assert_eq!(totals["failed"], json!(1));
    assert_eq!(totals["malformed_calls"], json!(1));
    assert_eq!(totals["tool_error_rate"], json!(0.25));
    assert_eq!(totals["malformed_rate"], json!(0.25));
    assert_eq!(totals["edit_apply"], json!({"attempts": 1, "applied": 0}));
    assert_eq!(totals["spills"], json!(1));
    // The stub is 31 bytes; the body 4 KiB.
    assert_eq!(totals["spill_saved_bytes"], json!(4_096 - 31));
}

#[test]
fn stats_filters_by_session_and_prints_human_output() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store(&db);

    // A filter that cannot match any v7 id (those open with a hex
    // timestamp): the empty report is a clean success, not an error.
    let (ok, stdout, stderr) = stats(&db, &["--session", "zz-not-a-session", "--json"]);
    assert!(ok, "filter failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["sessions"].as_array().unwrap().len(), 0);

    let (ok, stdout, stderr) = stats(&db, &[]);
    assert!(ok, "human output failed: {stderr}");
    assert!(stdout.contains("sessions in"), "header missing: {stdout}");
    assert!(
        stdout.contains("bytes kept out of context"),
        "spill savings missing: {stdout}"
    );
    assert!(stdout.contains("4 turns"), "totals line missing: {stdout}");
}

/// Seeds one session whose transcript and event stream exercise every
/// mechanism the `mechanisms` section attributes: a cache stub hit and
/// an omission marker in tool results, a repair in effects, and a
/// prune report plus a zero-request fast-path solve in the persisted
/// events. All synthetic, all offline.
fn seeded_store_with_events(db: &std::path::Path) -> SessionId {
    use loom_core::kernel::context::{PruneReport, PruneTrigger, PrunedCall, StubReason};
    use loom_core::kernel::event::{EVENT_SCHEMA_VERSION, EventEnvelope, EventKind};
    use loom_core::kernel::store::EventStore;
    use loom_core::kernel::tool::ToolEffects;

    let store = SqliteStore::open(db).unwrap();
    let id = SessionId::new();
    let mut session = Session::new(id);
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "ship it".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    // A turn with a cache-stub hit (repeat read while the original
    // stays projected) and an output-filter reduction (counted
    // omission markers).
    let mut stub = result("b", ToolStatus::Success, "unchanged since call a");
    stub.effects = ToolEffects::none().with_repair("old_string matched after trimming");
    commit(
        &store,
        &id,
        &mut session,
        turn(
            "reading",
            vec![
                call("a", "read_file", json!({"path": "src/lib.rs"})),
                call("b", "read_file", json!({"path": "src/lib.rs"})),
                call("c", "run_command", json!({"program": "git"})),
            ],
        ),
        vec![
            result("a", ToolStatus::Success, "fn main() {}"),
            stub,
            result(
                "c",
                ToolStatus::Success,
                "abc123 fix the thing\n[... 5 more commits omitted]",
            ),
        ],
    );

    // The observed event stream, persisted exactly as the engine's
    // append-on-emit wrapper would have.
    let task = loom_core::kernel::id::TaskId::new();
    let events = vec![
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 0,
            task_id: task,
            kind: EventKind::TaskStarted,
        },
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 1,
            task_id: task,
            kind: EventKind::ContextPruned {
                turn: 3,
                report: PruneReport {
                    trigger: PruneTrigger::AgeMask,
                    calls: vec![PrunedCall {
                        call_id: "a".to_owned(),
                        reason: StubReason::TurnAge,
                        reclaimed_tokens: 512,
                    }],
                    reclaimed_tokens: 512,
                },
            },
        },
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 2,
            task_id: task,
            kind: EventKind::FastPathHit {
                kind: "rename".to_owned(),
                files_changed: 1,
            },
        },
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence: 3,
            task_id: task,
            kind: EventKind::TaskCompleted,
        },
    ];
    for envelope in &events {
        store.append_event(&id, envelope).unwrap();
    }
    id
}

#[test]
fn stats_attributes_savings_per_mechanism() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store_with_events(&db);

    let (ok, stdout, stderr) = stats(&db, &["--json"]);
    assert!(ok, "loom stats --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    let mechanisms = &report["mechanisms"];
    assert_eq!(mechanisms["prune_events"], json!(1));
    assert_eq!(mechanisms["pruned_calls"], json!(1));
    assert_eq!(mechanisms["reclaimed_tokens_estimate"], json!(512));
    assert_eq!(mechanisms["handoffs"], json!(0));
    assert_eq!(mechanisms["stub_hits"], json!(1));
    assert_eq!(mechanisms["omission_markers"], json!(1));
    assert_eq!(mechanisms["repairs"], json!(1));
    assert_eq!(mechanisms["fast_path_hits"], json!(1));
    assert_eq!(mechanisms["fast_path_files_changed"], json!(1));
    assert!(
        mechanisms["note"]
            .as_str()
            .unwrap()
            .contains("occurrence count"),
        "counted fields must be labeled as counts: {mechanisms}"
    );

    // The human rendering says what each number is.
    let (ok, stdout, stderr) = stats(&db, &[]);
    assert!(ok, "human output failed: {stderr}");
    assert!(
        stdout.contains("mechanisms: 1 fast-path solve (1 file, zero model calls)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1 prune (1 call stubbed, ~512 tok estimator-side)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1 cache stub hit (\"unchanged since call\")"),
        "{stdout}"
    );
    assert!(stdout.contains("1 omission marker"), "{stdout}");
    assert!(stdout.contains("1 repair"), "{stdout}");
}

#[test]
fn stats_reports_mechanisms_for_sessions_without_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    // The pre-events fixture: no event rows anywhere.
    seeded_store(&db);

    let (ok, stdout, stderr) = stats(&db, &["--json"]);
    assert!(ok, "loom stats --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let mechanisms = &report["mechanisms"];
    assert_eq!(mechanisms["prune_events"], json!(0));
    assert_eq!(mechanisms["fast_path_hits"], json!(0));
    assert_eq!(
        mechanisms["stub_hits"],
        json!(0),
        "no stub-shaped results in this fixture"
    );
    // The fixture's spill stub text ends in "omitted", which the marker
    // counter reads as one omission — occurrence counting, exactly as
    // labeled.
    assert_eq!(mechanisms["omission_markers"], json!(1));

    let (ok, stdout, stderr) = stats(&db, &[]);
    assert!(ok, "human output failed: {stderr}");
    assert!(
        stdout.contains("mechanisms: 1 omission marker"),
        "only the marker mechanism fired: {stdout}"
    );
}

/// Seeds one session exercising the skill telemetry: two body loads,
/// one bundled-resource read, one unknown-name miss.
fn seeded_store_with_skills(db: &std::path::Path) {
    let store = SqliteStore::open(db).unwrap();
    let id = SessionId::new();
    let mut session = Session::new(id);
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "ship it".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    commit(
        &store,
        &id,
        &mut session,
        turn(
            "skilling up",
            vec![
                call("a", "load_skill", json!({"name": "release"})),
                call("b", "load_skill", json!({"name": "release"})),
                call(
                    "c",
                    "load_skill",
                    json!({"name": "deploy", "resource": "notes.md"}),
                ),
                call("d", "load_skill", json!({"name": "ghost"})),
            ],
        ),
        vec![
            result("a", ToolStatus::Success, "# skill: release\none"),
            result("b", ToolStatus::Success, "# skill: release\ntwo"),
            result(
                "c",
                ToolStatus::Success,
                "# skill resource: deploy/notes.md\nport",
            ),
            result(
                "d",
                ToolStatus::Error(ToolFailure::InvalidInput),
                "no skill named `ghost`",
            ),
        ],
    );
}

#[test]
fn stats_reports_per_skill_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    seeded_store_with_skills(&db);

    let (ok, stdout, stderr) = stats(&db, &["--json"]);
    assert!(ok, "loom stats --json failed: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    // Totals, name order: deploy, ghost, release.
    let skills = report["totals"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 3);
    assert_eq!(skills[0]["name"], json!("deploy"));
    assert_eq!(skills[0]["loads"], json!(0));
    assert_eq!(skills[0]["resources"], json!(1));
    assert_eq!(skills[1]["name"], json!("ghost"));
    assert_eq!(skills[1]["failures"], json!(1));
    let release = &skills[2];
    assert_eq!(release["name"], json!("release"));
    assert_eq!(release["loads"], json!(2));
    let delivered = "# skill: release\none".len() + "# skill: release\ntwo".len();
    assert_eq!(release["delivered_bytes"], json!(delivered as u64));

    // The session row carries the same per-skill map.
    assert_eq!(
        report["sessions"][0]["stats"]["skills"]["release"]["loads"],
        json!(2)
    );

    // The human rendering says what each number is.
    let (ok, stdout, stderr) = stats(&db, &[]);
    assert!(ok, "human output failed: {stderr}");
    assert!(
        stdout.contains("skills: deploy 0 loads, 1 resource"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ghost 0 loads, 0 resources, 1 failure"),
        "{stdout}"
    );
    assert!(stdout.contains("release 2 loads, 0 resources"), "{stdout}");
    assert!(stdout.contains("bytes delivered"), "{stdout}");
}
