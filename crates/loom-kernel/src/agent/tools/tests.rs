use super::result::*;
use super::*;
use crate::store::ArtifactStore;
use crate::tool::{CommandEffect, PathTouch, Tool, ToolEffects, ToolOutput, ToolSpec};
use crate::transcript::{ToolArguments, ToolCallId};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// What a [`MockTool`] does when called.
#[derive(Clone)]
enum Action {
    /// Succeeds with the given text.
    Text(String),
    /// Fails input validation.
    InvalidInput,
    /// Fails execution.
    Fail,
    /// Sleeps, then succeeds (timeout / ordering / cancellation
    /// substrate).
    Sleep { ms: u64 },
    /// Signals `notify`, then succeeds immediately: a deterministic
    /// sync point for the cancellation-table probe.
    SignalText {
        notify: Arc<tokio::sync::Notify>,
        text: String,
    },
    /// Tracks concurrent invocations while sleeping (concurrency
    /// bound and sequential-mode probes).
    CountSleep {
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        ms: u64,
    },
    /// Sleeps for a minute (a call that must be cancelled).
    Hang,
    /// Succeeds and reports side effects (the effects seam).
    Effects { effects: ToolEffects },
    /// Tracks concurrency and records completion order, and — when
    /// `declared` — declares its touched paths to the scheduler
    /// (ADR-018 probes).
    Track {
        reads: Vec<String>,
        writes: Vec<String>,
        declared: bool,
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
        label: String,
        ms: u64,
    },
}

struct MockTool {
    spec: ToolSpec,
    read_only: bool,
    action: Action,
}

impl MockTool {
    fn text(name: &str, text: &str) -> Self {
        Self {
            spec: spec(name),
            read_only: true,
            action: Action::Text(text.to_owned()),
        }
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: "mock tool".to_owned(),
        parameters: json!({"type": "object"}),
    }
}

impl Tool for MockTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn touched_paths(&self, _input: &Value) -> Option<Vec<PathTouch>> {
        // Seven test-action variants and only Track+declared produce
        // touches; the rest deliberately yield none.
        #[allow(clippy::wildcard_enum_match_arm)]
        match &self.action {
            Action::Track {
                reads,
                writes,
                declared: true,
                ..
            } => {
                let mut touches: Vec<PathTouch> =
                    reads.iter().cloned().map(PathTouch::read).collect();
                touches.extend(writes.iter().cloned().map(PathTouch::write));
                Some(touches)
            }
            _ => None,
        }
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let action = self.action.clone();
        Box::pin(async move {
            match action {
                Action::Text(text) => Ok(ToolOutput::new(text)),
                Action::InvalidInput => Err(ToolError::InvalidInput("bad args".to_owned())),
                Action::Fail => Err(ToolError::Execution("boom".to_owned())),
                Action::Sleep { ms } => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(ToolOutput::new("slept"))
                }
                Action::SignalText { notify, text } => {
                    notify.notify_one();
                    Ok(ToolOutput::new(text))
                }
                Action::CountSleep {
                    active,
                    max_seen,
                    ms,
                } => {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(ToolOutput::new("counted"))
                }
                Action::Hang => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(ToolOutput::new("finally"))
                }
                Action::Effects { effects } => {
                    Ok(ToolOutput::new("touched the world").with_effects(effects))
                }
                Action::Track {
                    active,
                    max_seen,
                    log,
                    label,
                    ms,
                    ..
                } => {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    log.lock().expect("log lock").push(label);
                    Ok(ToolOutput::new("tracked"))
                }
            }
        })
    }
}

fn make_call(name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(format!("call-{name}")).expect("valid id"),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(json!({})).expect("object"),
    }
}

fn registry(tools: Vec<MockTool>) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for tool in tools {
        registry.register(Box::new(tool)).expect("unique name");
    }
    registry
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<EventKind>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        self.events.lock().expect("event lock").push(kind);
    }
}

fn test_ctx(timeout: Duration, token: &CancellationToken) -> ToolContext {
    ToolContext::new(PathBuf::new(), token.clone(), timeout)
}

#[test]
fn exposure_set_membership() {
    let exposure = ToolExposure::from_names(["read_file", "list_dir"]);
    assert!(exposure.contains("read_file"));
    assert!(!exposure.contains("search"));
    assert_eq!(exposure.len(), 2);
    assert!(!exposure.is_empty());
    assert!(ToolExposure::default().is_empty());
}

#[tokio::test]
async fn successful_call_persists_the_effects_it_reported() {
    let effects = ToolEffects::none()
        .with_file_written("src/lib.rs")
        .with_file_deleted("src/old.rs")
        .with_command(CommandEffect::new("cargo", vec!["test".into()], Some(101)));
    let mut tool = MockTool::text("edit_file", "ignored");
    tool.read_only = false;
    tool.action = Action::Effects {
        effects: effects.clone(),
    };
    let registry = registry(vec![tool]);
    let exposure = ToolExposure::from_names(["edit_file"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(
        &registry,
        &exposure,
        &make_call("edit_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.effects, effects);
}

#[tokio::test]
async fn a_repeated_read_is_served_from_cache_until_a_write_invalidates() {
    let dir = std::env::temp_dir().join(format!("loom-tools-cache-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("src")).expect("workspace");
    std::fs::write(dir.join("src/a.rs"), "fn a() {}").expect("file");

    let log = Arc::new(Mutex::new(Vec::new()));
    let tool = MockTool {
        spec: spec("read_file"),
        read_only: true,
        action: Action::Track {
            reads: vec!["src/a.rs".to_owned()],
            writes: Vec::new(),
            declared: true,
            active: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            log: log.clone(),
            label: "read".to_owned(),
            ms: 0,
        },
    };
    let registry = registry(vec![tool]);
    let exposure = ToolExposure::from_names(["read_file"]);
    let token = CancellationToken::new();
    let cache = Arc::new(Mutex::new(crate::tool_cache::ToolCache::new()));
    let ctx = ToolContext::new(dir.clone(), token.clone(), Duration::from_secs(5))
        .with_cache(Some(cache.clone()));
    let started = AtomicBool::new(false);

    let first = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    let second = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(first.status, ToolStatus::Success);
    assert_eq!(second.status, ToolStatus::Success);
    assert_eq!(second.content, first.content);
    assert_eq!(
        log.lock().expect("log").len(),
        1,
        "the second identical read must be served from the cache"
    );
    assert_eq!(cache.lock().expect("cache").hits(), 1);

    cache
        .lock()
        .expect("cache")
        .invalidate_effects(&ToolEffects::none().with_file_written("src/a.rs"));
    let third = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(third.status, ToolStatus::Success);
    assert_eq!(
        log.lock().expect("log").len(),
        2,
        "a recorded write to the read path invalidates the entry"
    );
    assert_eq!(third.content, first.content);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_repeat_of_a_projected_call_is_served_as_a_stub() {
    let dir = std::env::temp_dir().join(format!("loom-tools-stub-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("src")).expect("workspace");
    std::fs::write(dir.join("src/a.rs"), "fn a() {}").expect("file");

    let log = Arc::new(Mutex::new(Vec::new()));
    let tool = MockTool {
        spec: spec("read_file"),
        read_only: true,
        action: Action::Track {
            reads: vec!["src/a.rs".to_owned()],
            writes: Vec::new(),
            declared: true,
            active: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            log: log.clone(),
            label: "read".to_owned(),
            ms: 0,
        },
    };
    let registry = registry(vec![tool]);
    let exposure = ToolExposure::from_names(["read_file"]);
    let token = CancellationToken::new();
    let cache = Arc::new(Mutex::new(crate::tool_cache::ToolCache::new()));
    // The original call's result is still in the projection the model
    // sees, so the engine marks its id as projected.
    let ctx = ToolContext::new(dir.clone(), token.clone(), Duration::from_secs(5))
        .with_cache(Some(cache.clone()))
        .with_projected_calls(std::iter::once("call-read_file".to_owned()).collect());
    let started = AtomicBool::new(false);

    let first = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    let second = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(first.status, ToolStatus::Success);
    assert_eq!(second.status, ToolStatus::Success);
    assert_eq!(
        second.content, "unchanged since call call-read_file",
        "a repeat whose original is still projected is a cheap stub"
    );
    assert_eq!(
        log.lock().expect("log").len(),
        1,
        "the stub must not re-execute the tool"
    );
    assert_eq!(cache.lock().expect("cache").hits(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_call_that_never_ran_reports_no_effects() {
    let registry = registry(vec![MockTool::text("read_file", "unused")]);
    let exposure = ToolExposure::from_names(["read_file"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(
        &registry,
        &exposure,
        &make_call("edit_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::UnknownTool));
    assert!(result.effects.is_empty());
}

#[tokio::test]
async fn execute_call_stop_like_unknown_tool_says_how_to_end() {
    let registry = registry(vec![MockTool::text("read_file", "ok")]);
    let exposure = ToolExposure::from_names(["read_file"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("finish"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::UnknownTool));
    assert!(
        result.content.contains("available: read_file"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("replying with no tool call"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn execute_call_unknown_tool_lists_registered() {
    let registry = registry(vec![MockTool::text("read_file", "ok")]);
    let exposure = ToolExposure::from_names(["read_file"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("grep"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::UnknownTool));
    assert_eq!(
        result.content,
        "error: unknown tool \"grep\"; available: read_file"
    );
    assert!(!started.load(Ordering::Acquire));
}

#[tokio::test]
async fn execute_call_unknown_tool_empty_registry() {
    let registry = registry(vec![]);
    let exposure = ToolExposure::default();
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("grep"), &ctx, &started).await;
    assert_eq!(
        result.content,
        "error: unknown tool \"grep\"; no tools registered"
    );
}

#[tokio::test]
async fn execute_call_registered_but_not_exposed() {
    let registry = registry(vec![MockTool::text("read_file", "ok")]);
    let exposure = ToolExposure::from_names(["list_dir"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(
        &registry,
        &exposure,
        &make_call("read_file"),
        &ctx,
        &started,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::NotExposed));
    assert_eq!(
        result.content,
        "error: tool \"read_file\" is not exposed in this request"
    );
    assert!(!started.load(Ordering::Acquire));
}

#[tokio::test]
async fn execute_call_maps_tool_errors() {
    let registry = registry(vec![
        MockTool {
            spec: spec("bad"),
            read_only: true,
            action: Action::InvalidInput,
        },
        MockTool {
            spec: spec("boom"),
            read_only: true,
            action: Action::Fail,
        },
    ]);
    let exposure = ToolExposure::from_names(["bad", "boom"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);

    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("bad"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::InvalidInput));
    assert_eq!(result.content, "error: invalid input: bad args");

    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("boom"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::Execution));
    assert_eq!(result.content, "error: execution failed: boom");
    assert!(started.load(Ordering::Acquire));
}

#[tokio::test]
async fn execute_call_enforces_timeout() {
    let registry = registry(vec![MockTool {
        spec: spec("slow"),
        read_only: true,
        action: Action::Sleep { ms: 10_000 },
    }]);
    let exposure = ToolExposure::from_names(["slow"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_millis(25), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("slow"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Error(ToolFailure::Timeout));
    assert_eq!(result.content, "error: tool \"slow\" timed out after 25ms");
    assert!(started.load(Ordering::Acquire));
}

#[tokio::test]
async fn execute_call_bounds_output_at_char_boundary() {
    // 3-byte chars: 66_000 bytes total; the 65_536-byte cut lands
    // mid-char and must back off to 65_535.
    let big = "✀".repeat(22_000);
    let registry = registry(vec![MockTool::text("big", &big)]);
    let exposure = ToolExposure::from_names(["big"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("big"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content.len(), MAX_TOOL_OUTPUT_BYTES - 1);
    assert!(result.content.is_char_boundary(result.content.len()));
    assert!(result.truncated);
}

#[tokio::test]
async fn execute_call_preserves_tool_truncated_flag() {
    // A tool that already truncated (short content, flag set):
    // the engine must not clear the flag.
    let mut registry = ToolRegistry::default();
    registry
        .register(Box::new(TruncatingTool { spec: spec("pre") }))
        .expect("unique");
    let exposure = ToolExposure::from_names(["pre"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("pre"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content, "small");
    assert!(result.truncated);
}

struct TruncatingTool {
    spec: ToolSpec,
}

impl Tool for TruncatingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::new("small").with_truncated()) })
    }
}

#[tokio::test]
async fn execute_call_pre_cancelled_never_starts() {
    let registry = registry(vec![MockTool::text("t", "ok")]);
    let exposure = ToolExposure::from_names(["t"]);
    let token = CancellationToken::new();
    token.cancel();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let started = AtomicBool::new(false);
    let result = execute_call(&registry, &exposure, &make_call("t"), &ctx, &started).await;
    assert_eq!(result.status, ToolStatus::Cancelled { started: false });
    assert_eq!(result.content, "error: cancelled before execution; not run");
    assert!(!started.load(Ordering::Acquire));
}

#[tokio::test]
async fn execute_call_cancelled_mid_flight_reports_started() {
    let registry = registry(vec![MockTool {
        spec: spec("hang"),
        read_only: true,
        action: Action::Sleep { ms: 10_000 },
    }]);
    let exposure = ToolExposure::from_names(["hang"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(30), &token);
    let started = AtomicBool::new(false);
    let hang_call = make_call("hang");
    let mut call = std::pin::pin!(execute_call(
        &registry, &exposure, &hang_call, &ctx, &started
    ));
    // Let the call reach its sleep, then cancel from the outside.
    let grace = std::pin::pin!(tokio::time::sleep(Duration::from_millis(10)));
    match future::select(call.as_mut(), grace).await {
        Either::Left(_) => panic!("hang tool finished without cancellation"),
        Either::Right(_) => {}
    }
    token.cancel();
    let result = call.await;
    assert_eq!(result.status, ToolStatus::Cancelled { started: true });
    assert_eq!(
        result.content,
        "error: cancelled during execution; side effects may have occurred"
    );
    assert!(started.load(Ordering::Acquire));
}

#[tokio::test]
async fn batch_results_in_call_order() {
    // Later calls finish earlier; `buffered` must still yield in
    // call order.
    let tools: Vec<MockTool> = (0..6)
        .map(|i| MockTool {
            spec: spec(&format!("t{i}")),
            read_only: true,
            action: Action::Sleep { ms: (6 - i) * 5 },
        })
        .collect();
    let names: Vec<String> = tools.iter().map(|t| t.spec.name.clone()).collect();
    let registry = registry(tools);
    let exposure = ToolExposure::from_names(names.clone());
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&registry, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert_eq!(results.len(), 6);
    for (call, result) in calls.iter().zip(results.iter()) {
        assert_eq!(call.id, result.call_id);
        assert_eq!(result.status, ToolStatus::Success);
    }
}

#[tokio::test]
async fn batch_respects_concurrency_bound() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let names: Vec<String> = (0..10).map(|i| format!("c{i}")).collect();
    let mut reg = ToolRegistry::default();
    for name in &names {
        reg.register(Box::new(MockTool {
            spec: spec(name),
            read_only: true,
            action: Action::CountSleep {
                active: active.clone(),
                max_seen: max_seen.clone(),
                ms: 20,
            },
        }))
        .expect("unique");
    }
    let exposure = ToolExposure::from_names(names.clone());
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(4);
    let results = batch.execute(&calls, &ctx).await;
    assert_eq!(results.len(), 10);
    assert!(results.iter().all(|r| r.status == ToolStatus::Success));
    assert_eq!(max_seen.load(Ordering::SeqCst), 4, "bound must be exact");
}

#[tokio::test]
async fn wildcard_mutation_runs_after_concurrent_readers() {
    // A mutating call that cannot describe its paths (no declaration)
    // is a wildcard: it conflicts with everything, but the read-only
    // calls it runs after are still concurrent with each other
    // (ADR-018).
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::default();
    let mut names: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
    for name in &names {
        reg.register(Box::new(MockTool {
            spec: spec(name),
            read_only: true,
            action: Action::Track {
                reads: vec![format!("{name}.txt")],
                writes: Vec::new(),
                declared: true,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: name.clone(),
                ms: 20,
            },
        }))
        .expect("unique");
    }
    names.push("writer".to_owned());
    reg.register(Box::new(MockTool {
        spec: spec("writer"),
        read_only: false,
        action: Action::Track {
            reads: Vec::new(),
            writes: Vec::new(),
            declared: false,
            active: active.clone(),
            max_seen: max_seen.clone(),
            log: log.clone(),
            label: "writer".to_owned(),
            ms: 1,
        },
    }))
    .expect("unique");

    let exposure = ToolExposure::from_names(names.clone());
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
    let results = batch.execute(&calls, &ctx).await;
    assert!(results.iter().all(|r| r.status == ToolStatus::Success));
    assert_eq!(max_seen.load(Ordering::SeqCst), 6);
    let entries = log.lock().expect("log lock").clone();
    assert_eq!(entries.len(), 7);
    assert_eq!(entries.last().map(String::as_str), Some("writer"));
    assert!(entries[..6].iter().all(|entry| entry != "writer"));
}

#[tokio::test]
async fn disjoint_writes_run_concurrently() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::default();
    for (name, path) in [("w0", "a.txt"), ("w1", "b.txt")] {
        reg.register(Box::new(MockTool {
            spec: spec(name),
            read_only: false,
            action: Action::Track {
                reads: Vec::new(),
                writes: vec![path.to_owned()],
                declared: true,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: name.to_owned(),
                ms: 20,
            },
        }))
        .expect("unique");
    }
    let names = ["w0", "w1"];
    let exposure = ToolExposure::from_names(names);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert!(results.iter().all(|r| r.status == ToolStatus::Success));
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        2,
        "writes to disjoint paths must overlap"
    );
}

#[tokio::test]
async fn same_path_write_then_read_keeps_model_order() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::default();
    reg.register(Box::new(MockTool {
        spec: spec("writer"),
        read_only: false,
        action: Action::Track {
            reads: Vec::new(),
            writes: vec!["a.txt".to_owned()],
            declared: true,
            active: active.clone(),
            max_seen: max_seen.clone(),
            log: log.clone(),
            label: "write".to_owned(),
            ms: 30,
        },
    }))
    .expect("unique");
    reg.register(Box::new(MockTool {
        spec: spec("reader"),
        read_only: true,
        action: Action::Track {
            reads: vec!["a.txt".to_owned()],
            writes: Vec::new(),
            declared: true,
            active: active.clone(),
            max_seen: max_seen.clone(),
            log: log.clone(),
            label: "read".to_owned(),
            ms: 1,
        },
    }))
    .expect("unique");
    let names = ["writer", "reader"];
    let exposure = ToolExposure::from_names(names);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert!(results.iter().all(|r| r.status == ToolStatus::Success));
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        1,
        "a read after a write to the same path must not overlap it"
    );
    assert_eq!(
        &*log.lock().expect("log lock"),
        &["write".to_owned(), "read".to_owned()]
    );
}

#[tokio::test]
async fn overlapping_writes_serialize_in_model_order() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::default();
    for name in ["w0", "w1"] {
        reg.register(Box::new(MockTool {
            spec: spec(name),
            read_only: false,
            action: Action::Track {
                reads: Vec::new(),
                writes: vec!["a.txt".to_owned()],
                declared: true,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: name.to_owned(),
                ms: 10,
            },
        }))
        .expect("unique");
    }
    let names = ["w0", "w1"];
    let exposure = ToolExposure::from_names(names);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert!(results.iter().all(|r| r.status == ToolStatus::Success));
    assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    assert_eq!(
        &*log.lock().expect("log lock"),
        &["w0".to_owned(), "w1".to_owned()]
    );
}

#[tokio::test]
async fn batch_failure_isolation() {
    let registry = registry(vec![
        MockTool::text("ok1", "first"),
        MockTool {
            spec: spec("bad"),
            read_only: true,
            action: Action::Fail,
        },
        MockTool::text("ok2", "second"),
    ]);
    let exposure = ToolExposure::from_names(["ok1", "bad", "ok2"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = ["ok1", "bad", "ok2"].map(make_call).to_vec();
    let batch = ToolBatch::new(&registry, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert_eq!(results[0].status, ToolStatus::Success);
    assert_eq!(results[0].content, "first");
    assert_eq!(results[1].status, ToolStatus::Error(ToolFailure::Execution));
    assert_eq!(results[2].status, ToolStatus::Success);
    assert_eq!(results[2].content, "second");
}

#[tokio::test]
async fn batch_not_exposed_mutating_keeps_concurrent() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let mut names: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
    names.push("writer".to_owned());
    let mut reg = ToolRegistry::default();
    for name in &names {
        let is_writer = name == "writer";
        reg.register(Box::new(MockTool {
            spec: spec(name),
            read_only: !is_writer,
            action: Action::CountSleep {
                active: active.clone(),
                max_seen: max_seen.clone(),
                ms: 10,
            },
        }))
        .expect("unique");
    }
    // `writer` is registered but NOT exposed: its call is rejected
    // without running and must not serialize the read-only rest.
    let exposure = ToolExposure::from_names(names[..6].iter().map(String::as_str));
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token);
    let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
    let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
    let results = batch.execute(&calls, &ctx).await;
    assert_eq!(results.len(), 7);
    assert_eq!(
        results[6].status,
        ToolStatus::Error(ToolFailure::NotExposed)
    );
    for result in &results[..6] {
        assert_eq!(result.status, ToolStatus::Success);
    }
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        6,
        "read-only calls must stay concurrent despite the rejected mutating call"
    );
}

#[tokio::test]
async fn batch_pre_cancelled_marks_all_not_started() {
    let registry = registry(vec![MockTool {
        spec: spec("hang"),
        read_only: true,
        action: Action::Hang,
    }]);
    let exposure = ToolExposure::from_names(["hang"]);
    let token = CancellationToken::new();
    token.cancel();
    let ctx = test_ctx(Duration::from_secs(30), &token);
    let calls: Vec<ToolCall> = vec![make_call("hang"); 3];
    let batch = ToolBatch::new(&registry, &exposure);
    let results = batch.execute(&calls, &ctx).await;
    assert_eq!(results.len(), 3);
    for result in &results {
        assert_eq!(result.status, ToolStatus::Cancelled { started: false });
    }
}

#[allow(clippy::wildcard_enum_match_arm)] // only the two batch events are expected
#[tokio::test]
async fn events_follow_each_call_under_the_concurrency_bound() {
    let sink = RecordingSink::default();
    let registry = registry(vec![
        MockTool::text("first", "one"),
        MockTool::text("second", "two"),
    ]);
    let exposure = ToolExposure::from_names(["first", "second"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(5), &token);
    let calls = vec![make_call("first"), make_call("second")];
    let results = ToolBatch::new(&registry, &exposure)
        .with_max_concurrency(1)
        .with_events(&sink, TaskId::new())
        .execute(&calls, &ctx)
        .await;
    assert!(
        results
            .iter()
            .all(|result| result.status == ToolStatus::Success)
    );

    let events = sink.events.lock().expect("event lock");
    let timeline: Vec<String> = events
        .iter()
        .map(|kind| match kind {
            EventKind::ToolStarted { name, .. } => format!("started {name}"),
            EventKind::ToolCompleted { name, .. } => format!("completed {name}"),
            other => panic!("unexpected batch event: {other:?}"),
        })
        .collect();
    assert_eq!(
        timeline,
        [
            "started first",
            "completed first",
            "started second",
            "completed second"
        ],
        "a bounded batch must report one call at a time"
    );
}

#[tokio::test]
async fn pre_cancelled_calls_report_only_a_completion() {
    let sink = RecordingSink::default();
    let registry = registry(vec![MockTool {
        spec: spec("hang"),
        read_only: true,
        action: Action::Hang,
    }]);
    let exposure = ToolExposure::from_names(["hang"]);
    let token = CancellationToken::new();
    token.cancel();
    let ctx = test_ctx(Duration::from_secs(30), &token);
    let results = ToolBatch::new(&registry, &exposure)
        .with_events(&sink, TaskId::new())
        .execute(&[make_call("hang")], &ctx)
        .await;
    assert_eq!(results[0].status, ToolStatus::Cancelled { started: false });

    let events = sink.events.lock().expect("event lock");
    assert_eq!(events.len(), 1, "no call was dispatched");
    assert!(matches!(
        &events[0],
        EventKind::ToolCompleted {
            status: ToolStatus::Cancelled { started: false },
            ..
        }
    ));
}

#[tokio::test]
async fn batch_cancellation_table() {
    // 20 calls, bound 8 (§6.5 table): call 0 completes fast; calls
    // 1..=8 are in flight when cancellation lands (buffered refills
    // call 0's slot with call 8); calls 9..=19 were never polled.
    // Call 0 signals `ready` as it completes, so the cancel point is
    // deterministic instead of racing a sleep window (which flaked
    // when the runner was slow to first-poll the batch).
    let ready = Arc::new(tokio::sync::Notify::new());
    let mut reg = ToolRegistry::default();
    reg.register(Box::new(MockTool {
        spec: spec("fast"),
        read_only: true,
        action: Action::SignalText {
            notify: ready.clone(),
            text: "done".to_owned(),
        },
    }))
    .expect("unique");
    reg.register(Box::new(MockTool {
        spec: spec("sleep"),
        read_only: true,
        action: Action::Sleep { ms: 100 },
    }))
    .expect("unique");
    let exposure = ToolExposure::from_names(["fast", "sleep"]);
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(30), &token);
    let mut calls = vec![make_call("fast")];
    for i in 1..20 {
        let mut call = make_call("sleep");
        call.id = ToolCallId::try_from(format!("call-sleep-{i}")).expect("valid id");
        calls.push(call);
    }

    let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
    let mut execute = std::pin::pin!(batch.execute(&calls, &ctx));
    // On the current-thread test runtime, one poll of the batch runs
    // call 0 to completion, writes its slot, and refills the free slot
    // with call 8 — so once call 0 signals, calls 1..=8 are in flight.
    let notified = std::pin::pin!(ready.notified());
    match future::select(execute.as_mut(), notified).await {
        Either::Left(_) => panic!("batch finished before call 0 signalled"),
        Either::Right(_) => {}
    }
    token.cancel();
    let results = execute.await;

    assert_eq!(results.len(), 20);
    assert_eq!(results[0].status, ToolStatus::Success);
    assert_eq!(results[0].content, "done");
    for result in &results[1..=8] {
        assert_eq!(
            result.status,
            ToolStatus::Cancelled { started: true },
            "in-flight calls must report started"
        );
    }
    for result in &results[9..] {
        assert_eq!(
            result.status,
            ToolStatus::Cancelled { started: false },
            "never-polled calls must report not started"
        );
    }
}

#[tokio::test]
async fn batch_empty_calls_return_empty() {
    let registry = registry(vec![]);
    let exposure = ToolExposure::default();
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(1), &token);
    let batch = ToolBatch::new(&registry, &exposure);
    let results = batch.execute(&[], &ctx).await;
    assert_eq!(results, Vec::new());
}

#[tokio::test]
async fn oversized_output_spills_to_an_artifact() {
    let content = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 1024);
    let tool = MockTool::text("big", &content);
    let store = Arc::new(crate::store::MemoryArtifactStore::new());
    let token = CancellationToken::new();
    let ctx = test_ctx(Duration::from_secs(10), &token).with_artifacts(Some(store.clone()));
    let registry = registry(vec![tool]);
    let exposure = ToolExposure::from_names(["big"]);

    let result = execute_call(
        &registry,
        &exposure,
        &make_call("big"),
        &ctx,
        &AtomicBool::new(false),
    )
    .await;

    assert!(result.truncated, "spilled output must be marked truncated");
    assert!(
        result.content.len() <= MAX_TOOL_OUTPUT_BYTES,
        "inline content must stay within the budget"
    );
    assert!(
        result
            .content
            .ends_with("bytes); read it with artifact_read]"),
        "content must point at the artifact: {}",
        result.content
    );
    let reference = result.artifact.expect("output should spill to an artifact");
    assert_eq!(reference.kind, ARTIFACT_KIND);
    assert_eq!(reference.bytes, content.len() as u64);
    let fetched = store.get(&reference.id).expect("get").expect("stored");
    assert_eq!(fetched.body, content.into_bytes());
}
