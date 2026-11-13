use super::budget::turn_extension_reason;
use super::execute::{append_failure_hint, visible_call_ids};
use super::feedback::{
    COMPLETION_REFUSAL_CAP, CompletionGate, ZERO_PROGRESS_TURNS, acceptance_oracle_refusal,
    completion_refusal, green_check, is_test_path, late_budget_due, zero_progress_due,
};
use super::{
    DEFAULT_RESERVED_OUTPUT_TOKENS, MAX_RESERVED_OUTPUT_TOKENS, OutputReservation, TurnBudget,
    derive_reserved_output_tokens,
};
use crate::event::TurnBudgetReason;
use crate::model::{Message, ModelCapabilities, ModelRequest, StopReason};
use crate::session::Session;
use crate::transcript::{Item, ToolFailure, ToolResult, ToolStatus};
use std::collections::HashSet;

#[test]
fn the_zero_progress_nudge_waits_for_the_threshold_and_fires_once() {
    assert!(!zero_progress_due(0, false));
    assert!(!zero_progress_due(ZERO_PROGRESS_TURNS - 1, false));
    assert!(zero_progress_due(ZERO_PROGRESS_TURNS, false));
    assert!(zero_progress_due(ZERO_PROGRESS_TURNS + 5, false));
    // Already nudged: one reminder per stall, not one per turn.
    assert!(!zero_progress_due(ZERO_PROGRESS_TURNS + 5, true));
}

#[test]
fn turn_extensions_need_room_and_evidence() {
    let full = TurnBudget {
        initial_turns: 0,
        extension_turns: 4,
        max_extensions: 2,
    };
    let idle = CompletionGate::default();
    // No extension configured: the default budget is inert.
    assert!(turn_extension_reason(&TurnBudget::default(), 0, 10, 50, &idle, 0).is_none());
    // Room checks: at the hard ceiling, or out of grants.
    assert!(turn_extension_reason(&full, 0, 50, 50, &idle, 0).is_none());
    assert!(turn_extension_reason(&full, 2, 10, 50, &idle, 0).is_none());
    // Successful commands do not count — only a new file mutation
    // since the grant is progress evidence.
    let churned = CompletionGate::default();
    assert!(turn_extension_reason(&full, 0, 10, 50, &churned, 0).is_none());
    // A mutation since the grant extends.
    let edited = CompletionGate {
        mutations: 2,
        ..CompletionGate::default()
    };
    assert_eq!(
        turn_extension_reason(&full, 0, 10, 50, &edited, 1),
        Some(TurnBudgetReason::MutationAtLimit)
    );
    // Unverified mutations extend even without a fresh edit: the
    // run was truncated inside the verify loop.
    let pending = CompletionGate {
        mutations: 1,
        ..CompletionGate::default()
    };
    assert_eq!(
        turn_extension_reason(&full, 0, 10, 50, &pending, 1),
        Some(TurnBudgetReason::UnverifiedWork)
    );
    // Idle with everything verified against the full suite stops.
    let settled = CompletionGate {
        mutations: 1,
        suite_green_at: Some(1),
        ..CompletionGate::default()
    };
    assert!(turn_extension_reason(&full, 0, 10, 50, &settled, 1).is_none());
}
#[test]
fn the_turn_budget_nudge_fires_once_at_three_fifths() {
    // A churning run never changes `turns_since_mutation`, so the
    // zero-progress nudge cannot reach it; this one counts turns.
    assert!(!late_budget_due(0, 50, false));
    assert!(!late_budget_due(29, 50, false));
    assert!(late_budget_due(30, 50, false));
    assert!(late_budget_due(50, 50, false));
    // Already nudged: one reminder per run.
    assert!(!late_budget_due(40, 50, true));
    // A single-turn budget is due immediately.
    assert!(late_budget_due(1, 1, false));
}

#[test]
fn a_tool_supplied_hint_replaces_the_generic_text() {
    let mut content = String::from("invalid input: shape");
    append_failure_hint(
        &mut content,
        ToolFailure::InvalidInput,
        Some("expected fields: program, args"),
    );
    assert_eq!(
        content,
        "invalid input: shape\nexpected fields: program, args"
    );
}

#[test]
fn without_a_tool_hint_the_generic_text_is_used() {
    let mut content = String::from("invalid input: shape");
    append_failure_hint(&mut content, ToolFailure::InvalidInput, None);
    assert_eq!(
        content,
        "invalid input: shape\nHint: correct the arguments described above before calling again."
    );
}

#[test]
fn a_refusal_never_carries_a_hint() {
    let mut content = String::from("refused: the same call again");
    append_failure_hint(&mut content, ToolFailure::RepeatedCall, Some("never shown"));
    assert_eq!(content, "refused: the same call again");
}

#[test]
fn auto_honors_a_declared_output_maximum_up_to_the_hard_cap() {
    // OpenRouter free-tier shape: declares 235_929 on a 262_144 window.
    let caps = ModelCapabilities::new(262_144).with_max_output_tokens(235_929);
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Auto),
        MAX_RESERVED_OUTPUT_TOKENS,
    );
    // Declaring exactly the default floor changes nothing.
    let caps = ModelCapabilities::new(128_000).with_max_output_tokens(8_192);
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Auto),
        DEFAULT_RESERVED_OUTPUT_TOKENS,
    );
}

#[test]
fn auto_keeps_half_the_window_for_input() {
    // A declaration larger than half the window is clamped.
    let caps = ModelCapabilities::new(10_000).with_max_output_tokens(60_000);
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Auto),
        5_000
    );
    // No declared maximum: the default floor, still window-clamped.
    let caps = ModelCapabilities::new(10_000);
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Auto),
        5_000
    );
}

#[test]
fn fixed_is_authoritative_but_window_clamped() {
    let caps = ModelCapabilities::new(10_000).with_max_output_tokens(60_000);
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Fixed(4_096)),
        4_096,
    );
    assert_eq!(
        derive_reserved_output_tokens(&caps, OutputReservation::Fixed(60_000)),
        5_000,
    );
}

fn result(call_id: &str, content: &str) -> Message {
    Message::ToolResult(ToolResult::new(
        crate::transcript::ToolCallId::try_from(call_id.to_owned()).unwrap(),
        ToolStatus::Success,
        content.to_owned(),
        false,
    ))
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "m".into(),
        messages,
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
        context_editing: None,
        reasoning_effort: None,
        session_id: None,
    }
}

#[test]
fn visible_call_ids_keeps_unstubbed_results() {
    let req = request(vec![result("call-a", "body"), result("call-b", "body")]);
    let ids = visible_call_ids(&req, &[]);
    assert_eq!(
        ids,
        ["call-a".to_owned(), "call-b".to_owned()]
            .into_iter()
            .collect::<HashSet<_>>(),
        "every projected result is still visible when nothing is stubbed"
    );
}

#[test]
fn visible_call_ids_drops_a_pruned_result() {
    // Pruning keeps the call id but rewrites the body to a stub, so the
    // id is in the projection while the content is gone. Serving
    // `unchanged since call <id>` for it would point the model at a
    // body it can no longer read.
    let req = request(vec![
        result("call-a", "[pruned] aged out; re-run the tool"),
        result("call-b", "body"),
    ]);
    let ids = visible_call_ids(&req, &["call-a".to_owned()]);
    assert_eq!(
        ids,
        ["call-b".to_owned()].into_iter().collect::<HashSet<_>>(),
        "a stubbed call id must not be treated as still-visible"
    );
}

#[test]
fn visible_call_ids_ignores_unmatched_stub_ids() {
    // A stub for a call that is no longer projected must not suppress
    // an unstubbed result that happens to share the projection.
    let req = request(vec![result("call-b", "body")]);
    let ids = visible_call_ids(&req, &["call-gone".to_owned()]);
    assert_eq!(
        ids,
        ["call-b".to_owned()].into_iter().collect::<HashSet<_>>()
    );
}

#[test]
fn visible_call_ids_skips_non_tool_result_messages() {
    let req = request(vec![
        Message::User {
            text: "task".to_owned(),
        },
        result("call-a", "body"),
        Message::Assistant(crate::transcript::AssistantMessage {
            content: "done".into(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        }),
    ]);
    let ids = visible_call_ids(&req, &[]);
    assert_eq!(
        ids,
        ["call-a".to_owned()].into_iter().collect::<HashSet<_>>()
    );
}

#[test]
fn edit_stalls_count_per_file_and_clear_on_success() {
    use super::feedback::{EDIT_STALL_TURNS, EditStalls, edit_target};
    let call = |name: &str, path: &str| crate::transcript::ToolCall {
        id: crate::transcript::ToolCallId::try_from("call_1".to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: crate::transcript::ToolArguments::try_from(serde_json::json!({
            "path": path,
        }))
        .expect("arguments"),
    };
    let result = |call: &crate::transcript::ToolCall, error: bool| {
        let status = if error {
            ToolStatus::Error(crate::transcript::ToolFailure::Execution)
        } else {
            ToolStatus::Success
        };
        ToolResult::new(call.id.clone(), status, String::new(), false)
    };

    // Only edit-family calls with a path argument count.
    let read = call("read_file", "src/lib.rs");
    assert!(edit_target(&read).is_none());
    let edit = call("edit_file", "src/lib.rs");
    assert_eq!(edit_target(&edit).as_deref(), Some("src/lib.rs"));

    // Two consecutive failures on one file reach the threshold.
    let mut stalls = EditStalls::default();
    stalls.note(&edit, &result(&edit, true));
    assert!(stalls.stalled_file().is_none());
    stalls.note(&edit, &result(&edit, true));
    assert_eq!(
        stalls.stalled_file(),
        Some(("src/lib.rs".to_owned(), EDIT_STALL_TURNS))
    );

    // A success on the file clears the stall; another file is
    // independent.
    let other = call("write_file", "src/main.rs");
    stalls.note(&other, &result(&other, true));
    stalls.note(&other, &result(&other, true));
    assert_eq!(
        stalls.stalled_file(),
        Some(("src/lib.rs".to_owned(), EDIT_STALL_TURNS))
    );
    stalls.note(&edit, &result(&edit, false));
    assert_eq!(
        stalls.stalled_file(),
        Some(("src/main.rs".to_owned(), EDIT_STALL_TURNS))
    );
    stalls.resolved("src/main.rs");
    assert!(stalls.stalled_file().is_none());
}

fn gate_result(content: &str, written: &[&str]) -> ToolResult {
    let mut effects = crate::tool::ToolEffects::default();
    for path in written {
        effects = effects.with_file_written((*path).to_owned());
    }
    let mut r = ToolResult::new(
        crate::transcript::ToolCallId::try_from("call_1".to_owned()).unwrap(),
        ToolStatus::Success,
        content.to_owned(),
        false,
    );
    r.effects = effects;
    r
}

#[test]
fn the_gate_demands_a_green_check_after_mutations() {
    let mut gate = CompletionGate::default();
    assert!(!gate.pending(), "no mutations yet: nothing to verify");
    gate.note(&gate_result("$ cargo check\nexit status: 0\n", &[]));
    assert!(!gate.pending(), "green without mutations is not pending");
    gate.note(&gate_result("edited src/lib.rs", &["src/lib.rs"]));
    assert!(gate.pending(), "a write after green is outstanding");
    gate.note(&gate_result("$ cargo test\nexit status: 0\n", &[]));
    assert!(!gate.pending(), "green after the write settles it");
    gate.note(&gate_result("$ cargo test\nexit status: 101\n", &[]));
    assert!(
        !gate.pending(),
        "a failing check alone does not reopen the gate: no files changed since the green"
    );
    gate.note(&gate_result("edited src/main.rs", &["src/main.rs"]));
    assert!(gate.pending(), "a write after the failing check reopens it");
}

#[test]
fn a_fused_write_with_a_green_check_lands_verified() {
    let mut gate = CompletionGate::default();
    gate.note(&gate_result(
        "edited src/lib.rs\n--- verify ---\n$ cargo test\nexit status: 0",
        &["src/lib.rs"],
    ));
    assert!(!gate.pending(), "effects bump before the full-suite green");
}

#[test]
fn a_narrow_green_does_not_settle_the_gate() {
    // The pytest-7490 hard-mix loss (2026-10-31): one test file
    // green while an untouched regression fails elsewhere.
    let mut gate = CompletionGate::default();
    gate.note(&gate_result("edited src/lib.rs", &["src/lib.rs"]));
    gate.note(&gate_result(
        "$ pytest testing/test_skipping.py\nexit status: 0",
        &[],
    ));
    assert!(gate.pending(), "a file-filtered run is not the full suite");
    gate.note(&gate_result("$ pytest -k skip\nexit status: 0", &[]));
    assert!(gate.pending(), "-k narrows the run");
    gate.note(&gate_result("$ pytest testing/\nexit status: 0", &[]));
    assert!(
        !gate.pending(),
        "a whole-directory run covers the suite and settles"
    );
    gate.note(&gate_result("edited src/main.rs", &["src/main.rs"]));
    gate.note(&gate_result("$ cargo check\nexit status: 0", &[]));
    assert!(gate.pending(), "a compile check is not a suite run");
    gate.note(&gate_result("$ FOO=1 pytest\nexit status: 0", &[]));
    assert!(!gate.pending(), "bare runner with env prefix settles");
}

#[test]
fn the_gate_demands_an_agent_authored_oracle_for_new_behavior() {
    let mut gate = CompletionGate::default();
    gate.note(&gate_result(
        "edited src/transform.rs",
        &["src/transform.rs"],
    ));
    // Old suite green on changed code: verified and tested, but the
    // new behavior has no oracle — the 13398 trap.
    gate.note(&gate_result("$ pytest tests/\nexit status: 0\n", &[]));
    assert!(
        !gate.pending(),
        "green after the write settles verification"
    );
    assert!(!gate.untested(), "a test-like command ran");
    assert!(
        gate.unproven(),
        "no check the agent authored exercises the change"
    );

    // A test-file mutation is an authored oracle.
    gate.note(&gate_result(
        "$ cat >> tests/test_transform.py\nexit status: 0",
        &["tests/test_transform.py"],
    ));
    assert!(!gate.unproven(), "mutating a test file provides the oracle");
}

#[test]
fn running_a_task_authored_script_satisfies_the_oracle() {
    let mut gate = CompletionGate::default();
    gate.note(&gate_result("wrote check_fix.py", &["check_fix.py"]));
    gate.note(&gate_result("$ python check_fix.py\nexit status: 0\n", &[]));
    assert!(
        !gate.unproven(),
        "a green run of a file this task wrote is an authored check"
    );
}

#[test]
fn the_oracle_refusal_quotes_the_acceptance_criteria_demand() {
    let mut session = Session::new(crate::id::SessionId::new());
    session
        .push_item(Item::User {
            text: "make the transform handle straight-overhead inputs".to_owned(),
        })
        .unwrap();
    let text = acceptance_oracle_refusal(&session);
    assert!(text.contains("acceptance criteria"), "{text}");
    assert!(text.contains("straight-overhead"), "{text}");
    assert!(text.contains("pure refactor"), "{text}");
}

#[test]
fn test_path_heuristics_match_standard_conventions() {
    assert!(is_test_path("tests/test_transform.py"));
    assert!(is_test_path("src/foo/test_helper.rs"));
    assert!(is_test_path("test_transform.py"));
    assert!(is_test_path("src/lib_test.go"));
    assert!(!is_test_path("src/transform.rs"));
    assert!(!is_test_path("src/testing_utilities.rs"));
}

#[test]
fn green_checks_must_be_command_shaped() {
    // A tool result that merely mentions the marker is not a check.
    assert!(!green_check(&gate_result("exit status: 0", &[])));
    assert!(green_check(&gate_result(
        "$ cargo test\nexit status: 0",
        &[]
    )));
}

#[test]
fn the_refusal_quotes_changed_paths_and_the_task() {
    let mut session = Session::new(crate::id::SessionId::new());
    session
        .push_item(Item::User {
            text: "fix the bug".to_owned(),
        })
        .unwrap();
    session
        .push_item(Item::ToolResult(gate_result(
            "edited src/lib.rs",
            &["src/lib.rs"],
        )))
        .unwrap();
    let text = completion_refusal(&session, 1);
    assert!(text.contains("src/lib.rs"), "{text}");
    assert!(text.contains("fix the bug"), "{text}");
    assert!(text.contains("cargo test"), "{text}");
    assert!(text.contains("git diff"), "must direct a diff self-review");
    assert!(text.contains("reminder 1 of 3"), "{text}");
}

#[test]
fn the_gate_refuses_until_the_cap_then_gives_up() {
    let mut gate = CompletionGate::default();
    gate.note(&gate_result("edited src/lib.rs", &["src/lib.rs"]));
    assert!(gate.pending() && gate.can_refuse(), "fresh gate can refuse");
    gate.note(&gate_result("edited again", &["src/lib.rs"]));
    gate.refusals = COMPLETION_REFUSAL_CAP;
    assert!(gate.pending(), "mutations are still outstanding, but…");
    assert!(
        !gate.can_refuse(),
        "…the cap stops refiring: three ignored nudges = stalling"
    );
}
