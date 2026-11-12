use super::elision::elide_text;
use super::projection::handoff_session;
use super::reclaim::{projected_reclaim, prune_report};
use super::strategies::decide;
use super::*;
use crate::context::messages_serialized;
use crate::context::{
    AgentConfig, ContextBudget, ContextCompiler, ContextError, ContextInput,
    DefaultContextCompiler, HeuristicEstimator, PruneCheckpoint,
};
use crate::model::{Message, ModelCapabilities, StopReason};
use crate::session::{Session, TaskRecord};
use crate::tool::ToolRegistry;
use crate::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use serde_json::json;

fn config() -> AgentConfig {
    AgentConfig::new(std::path::PathBuf::from("/w"), "system")
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities::new(500_000)
        .with_tool_calls(true)
        .with_max_output_tokens(4_096)
}

fn budget() -> ContextBudget {
    ContextBudget {
        max_input_tokens: 400_000,
        reserved_output_tokens: 4_096,
    }
}

fn registry() -> ToolRegistry {
    // A read-only tool declaring its path, and a mutating one.
    struct ReadTool;
    impl crate::tool::Tool for ReadTool {
        fn spec(&self) -> &crate::tool::ToolSpec {
            static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| crate::tool::ToolSpec {
                name: "read_file".to_owned(),
                description: "Reads a file.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<crate::tool::PathTouch>> {
            Some(vec![crate::tool::PathTouch::read(
                input.get("path").and_then(|v| v.as_str()).unwrap_or("x"),
            )])
        }
        fn call<'a>(
            &'a self,
            _input: serde_json::Value,
            _ctx: &'a crate::tool::ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::ToolOutput, crate::tool::ToolError>>
        {
            unreachable!("call not needed for pruning tests")
        }
    }
    struct WriteTool;
    impl crate::tool::Tool for WriteTool {
        fn spec(&self) -> &crate::tool::ToolSpec {
            static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| crate::tool::ToolSpec {
                name: "write_file".to_owned(),
                description: "Writes a file.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn call<'a>(
            &'a self,
            _input: serde_json::Value,
            _ctx: &'a crate::tool::ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::ToolOutput, crate::tool::ToolError>>
        {
            unreachable!("call not needed for pruning tests")
        }
    }
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(ReadTool)).expect("register");
    registry.register(Box::new(WriteTool)).expect("register");
    registry
}

fn session_with(items: Vec<Item>) -> Session {
    let mut session = Session::new(crate::id::SessionId::new());
    for item in items {
        session.transcript.push(item);
    }
    session
}

fn assistant_text(content: &str) -> Item {
    Item::Assistant(AssistantMessage {
        content: content.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
    })
}

fn assistant_calls(content: &str, calls: Vec<ToolCall>) -> Item {
    Item::Assistant(AssistantMessage {
        content: content.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    })
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).expect("call id"),
        name: "read_file".to_owned(),
        arguments: ToolArguments::try_from(json!({ "path": path })).expect("args"),
    }
}

fn write_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).expect("call id"),
        name: "write_file".to_owned(),
        arguments: ToolArguments::try_from(json!({ "path": path, "content": "x" })).expect("args"),
    }
}

fn result(id: &str, content: &str) -> Item {
    Item::ToolResult(ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Success,
        content,
        false,
    ))
}

fn error_result(id: &str) -> Item {
    Item::ToolResult(ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Error(crate::transcript::ToolFailure::Execution),
        big("error", 70),
        false,
    ))
}

fn written_result(id: &str, path: &str) -> Item {
    let mut result = ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Success,
        big("written", 25),
        false,
    );
    result.effects = crate::tool::ToolEffects::none().with_file_written(path);
    Item::ToolResult(result)
}

fn input<'a>(session: &'a Session, prune: Option<&'a PruneCheckpoint>) -> ContextInput<'a> {
    input_with(session, prune, budget())
}

fn input_with<'a>(
    session: &'a Session,
    prune: Option<&'a PruneCheckpoint>,
    budget: ContextBudget,
) -> ContextInput<'a> {
    shared();
    static FACTS: std::sync::OnceLock<crate::context::KnownFacts> = std::sync::OnceLock::new();
    let facts = FACTS.get_or_init(crate::context::KnownFacts::default);
    ContextInput {
        session,
        tools: READ_REGISTRY.get().expect("registry"),
        task: TASK.get().expect("task"),
        budget,
        capabilities: CAPABILITIES.get().expect("capabilities"),
        model: "deepseek-flash",
        config: AGENT_CONFIG.get().expect("config"),
        facts,
        prune_checkpoint: prune,
    }
}

static READ_REGISTRY: std::sync::OnceLock<ToolRegistry> = std::sync::OnceLock::new();
static TASK: std::sync::OnceLock<TaskRecord> = std::sync::OnceLock::new();
static CAPABILITIES: std::sync::OnceLock<ModelCapabilities> = std::sync::OnceLock::new();
static AGENT_CONFIG: std::sync::OnceLock<AgentConfig> = std::sync::OnceLock::new();

fn shared() {
    READ_REGISTRY.get_or_init(registry);
    TASK.get_or_init(|| TaskRecord::new(crate::id::TaskId::new(), 0));
    CAPABILITIES.get_or_init(capabilities);
    AGENT_CONFIG.get_or_init(config);
}

fn compiler(config: PruneConfig) -> PruningContextCompiler {
    shared();
    PruningContextCompiler::new(Box::new(DefaultContextCompiler::default()), config)
}

/// A config that fires at the old flat threshold, so the strategy tests
/// keep their shape regardless of the window-relative default (the 400 K
/// test budget would otherwise push the default trigger to 320 K).
fn firing() -> PruneConfig {
    PruneConfig {
        trigger_percent: 0,
        floor_tokens: 64_000,
        ..PruneConfig::default()
    }
}

fn big(prefix: &str, kb: usize) -> String {
    prefix.to_owned() + &"x".repeat(kb * 1024)
}

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
    assert!(compiled.stubbed_calls.is_empty());
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
    let checkpoint = first.prune_checkpoint.clone().expect("fired");
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

/// A budget tight enough that the *inner* compiler refuses the
/// unpruned projection — the situation the rescue exists for, where
/// the trigger never got the chance to fire below the wall.
fn tight() -> ContextBudget {
    ContextBudget {
        max_input_tokens: 60_000,
        reserved_output_tokens: 4_096,
    }
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
    let checkpoint = first.prune_checkpoint.clone().expect("triggered");

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

fn messages_contain(request: &crate::model::ModelRequest, needle: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| messages_serialized(std::slice::from_ref(message)).contains(needle))
}

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
            text: big("follow-up", 80).to_owned(),
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
            _ => None,
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
/// a provider contract on the OpenAI wire, so the kernel default
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
    let checkpoint = first.prune_checkpoint.clone().expect("fired");
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

/// The handoff view leads with the opening brief, carries the
/// handoff as a user item, then keeps the tail from `upto_item` on.
#[test]
fn handoff_session_is_brief_then_handoff_then_tail() {
    let items = vec![
        Item::User {
            text: "brief".to_owned(),
        },
        assistant_text("middle one"),
        assistant_text("middle two"),
        assistant_text("the tail"),
    ];
    let view = handoff_session(&session_with(items), "HANDOFF", 2);
    assert_eq!(view.transcript.len(), 4);
    assert!(matches!(&view.transcript[0], Item::User { text } if text == "brief"));
    assert!(matches!(&view.transcript[1], Item::User { text } if text == "HANDOFF"));
    assert!(matches!(&view.transcript[2], Item::Assistant(a) if a.content == "middle two"));
    assert!(matches!(&view.transcript[3], Item::Assistant(a) if a.content == "the tail"));
}

/// A transcript that does not open on a user item still gets a valid
/// view (handoff alone leads), and `upto_item` clamps to the end.
#[test]
fn handoff_session_handles_odd_openings_and_clamps() {
    let items = vec![
        assistant_text("first"),
        assistant_text("second"),
        Item::User {
            text: "brief".to_owned(),
        },
    ];
    let view = handoff_session(&session_with(items), "HANDOFF", 99);
    assert_eq!(view.transcript.len(), 1);
    assert!(matches!(&view.transcript[0], Item::User { text } if text == "HANDOFF"));
}

/// An LLM handoff checkpoint projects the view (brief + handoff +
/// tail), applies only call-id-keyed elisions — ordinal-keyed ones
/// would misfire on the renumbered view — and stays byte-stable.
#[test]
fn handoff_checkpoint_projects_view_and_skips_ordinal_elisions() {
    let items = vec![
        Item::User {
            text: "the brief".to_owned(),
        },
        assistant_text(&big("forgotten prose", 2)),
        assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
        result("c1", &big("gone", 2)),
        assistant_calls("tail read", vec![read_call("c2", "src/tail.rs")]),
        result("c2", &big("tail content", 1)),
        assistant_text("done"),
    ];
    let session = session_with(items);
    let checkpoint = PruneCheckpoint {
        upto_item: 4,
        stubs: Vec::new(),
        elided_args: vec![ArgElision {
            call_id: "c2".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/tail.rs",
                "content": "[pruned 1229 chars]"
            }))
            .expect("args"),
        }],
        // Original ordinal 1 is the handoff item's ordinal in the
        // view: applying ordinal elisions under handoff would eat
        // the handoff itself.
        elided_users: vec![1],
        elided_reasoning: Vec::new(),
        elided_assistant_text: Vec::new(),
        handoff: Some("HANDOFF SUMMARY".to_owned()),
    };
    let config = PruneConfig::default();
    let compiled = compiler(config)
        .compile(input(&session, Some(&checkpoint)))
        .expect("compiles");
    let projected = messages_serialized(&compiled.request.messages);
    assert!(projected.contains("the brief"), "brief leads the view");
    assert!(projected.contains("HANDOFF SUMMARY"), "handoff survives");
    assert!(
        projected.contains("tail contentxxxx"),
        "tail from upto_item on is kept"
    );
    assert!(
        !projected.contains("forgotten prosex"),
        "pre-handoff history is summarized away"
    );
    assert!(
        projected.contains("[pruned 1229 chars]"),
        "call-id-keyed arg elision applied"
    );
    // Byte-stable: the same checkpoint compiles to the same bytes.
    let again = compiler(config)
        .compile(input(&session, Some(&checkpoint)))
        .expect("compiles");
    assert_eq!(projected, messages_serialized(&again.request.messages));
    // The checkpoint is preserved (handoff and frozen elisions
    // alike), never churned by a re-decide over the view.
    assert_eq!(again.prune_checkpoint.as_ref(), Some(&checkpoint));
    assert_eq!(compiled.prune_checkpoint.as_ref(), Some(&checkpoint));
}

/// Checkpoints written before item 12 (no `handoff`, no
/// `elided_assistant_text`) replay unchanged via serde defaults.
#[test]
fn old_checkpoint_json_defaults_handoff_none() {
    let legacy = serde_json::json!({
        "upto_item": 3,
        "stubs": [],
        "elided_args": [],
        "elided_users": [],
        "elided_reasoning": [],
    });
    let checkpoint: PruneCheckpoint =
        serde_json::from_value(legacy).expect("legacy checkpoint replays");
    assert_eq!(checkpoint.handoff, None);
    assert!(checkpoint.elided_assistant_text.is_empty());
}
