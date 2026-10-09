//! Live, opt-in B1 benchmark (plan-iteration-1.md §1H).
//!
//! B1 asks the model to inspect this repository read-only: find the
//! implementation of a feature, explain it, and name the relevant files. The
//! harness runs one real task through the engine (credential resolution and
//! wiring identical to the CLI) and records the efficiency metrics the plan
//! asks for (turns, requests, tokens, tool calls by failure kind, retries,
//! wall time, and a peak/off-peak cost estimate).
//!
//! It is `#[ignore]`d so it never runs under a plain `cargo test`; it spends
//! real tokens once a credential resolves (keychain or `DEEPSEEK_API_KEY`).
//! Run it deliberately:
//!
//! ```text
//! cargo test -p loom-eval benchmark::b1 -- --ignored --nocapture
//! ```
//!
//! Override the target with `LOOM_BENCH_TARGET` and the model with
//! `LOOM_BENCH_MODEL` (any id in the capability table).
//!
//! **Never point this at the GLM Coding Plan** (docs/roadmap.md, "Current state"): it is not an
//! API endpoint and using it from a custom harness risks an account ban.

// A test harness: panicking on a setup failure is the intended behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use loom_core::engine::{Engine, EngineError, EngineOverrides};
use loom_core::kernel::agent::TaskControl;
use loom_core::kernel::event::{EventKind, EventSink};
use loom_core::kernel::id::{SessionId, TaskId};
use loom_core::kernel::session::Session;
use loom_core::kernel::transcript::{Item, ToolFailure, ToolStatus};

use super::eval::{ToolSet, cost_usd, eval_compiler, eval_config, price_multiplier, registry_for};

/// A provider tally built from the event stream: retries and the
/// `response_started` split that measures the cost of retrying after output.
#[derive(Default)]
struct Tally {
    events: u64,
    retries: u32,
    retries_after_output: u32,
    tool_started: u32,
    tool_completed: u32,
}

struct CountingSink(Mutex<Tally>);

impl EventSink for CountingSink {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        let mut tally = self.0.lock().unwrap();
        tally.events += 1;
        match kind {
            EventKind::ModelRetry {
                response_started, ..
            } => {
                tally.retries += 1;
                if response_started {
                    tally.retries_after_output += 1;
                }
            }
            EventKind::ToolStarted { .. } => tally.tool_started += 1,
            EventKind::ToolCompleted { .. } => tally.tool_completed += 1,
            _ => {}
        }
    }
}

#[tokio::test]
#[ignore = "live: spends real tokens once a credential resolves"]
async fn b1_read_only_repository_inspection() {
    let model =
        std::env::var("LOOM_BENCH_MODEL").unwrap_or_else(|_| String::from("deepseek-flash"));
    let target = std::env::var("LOOM_BENCH_TARGET")
        .unwrap_or_else(|_| String::from("the retry and cancellation handling in the agent loop"));
    let prompt = format!(
        "Inspect this repository, find the implementation of {target}, \
         explain how it works, and identify the relevant files."
    );
    eprintln!("B1 target: {target}");
    eprintln!("B1 model:  {model}");

    // The workspace is the repository root (this crate lives in crates/loom-eval).
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let system_prompt = String::from(
        "You are Loom, a minimal coding agent working read-only inside one workspace. \
         Use the provided tools to inspect the workspace: list directories and search \
         before reading large files, and prefer reading the smallest relevant range. \
         When you answer, be concise and name the workspace-relative paths you relied on.",
    );

    // Same wiring the CLI gets: the engine resolves the credential and
    // validates the model against the capability table; the harness only
    // pins the read-only registry and the bare compiler.
    let engine = match Engine::open_with(
        eval_config(workspace, "deepseek", &model, Some(system_prompt)),
        EngineOverrides::default()
            .with_registry(registry_for(ToolSet::ReadOnly))
            .with_compiler(eval_compiler("deepseek")),
    ) {
        Ok(engine) => engine,
        Err(EngineError::Auth(loom_core::engine::auth::AuthError::NoCredential { .. })) => {
            eprintln!("skipping B1: no credential resolved for deepseek");
            return;
        }
        Err(error) => panic!("opening the B1 engine: {error}"),
    };

    let mut session = Session::new(SessionId::new());
    let sink = CountingSink(Mutex::new(Tally::default()));
    let control = TaskControl::new();

    let started = Instant::now();
    let outcome = engine.run_task(&mut session, prompt, &sink, &control).await;
    let wall_time = started.elapsed();

    let tally = sink.0.into_inner().unwrap();
    let failures = tool_failure_counts(&session);
    let report = match &outcome {
        Ok(summary) => {
            let (hit, miss) = cache_split(summary);
            let (multiplier, window) = price_multiplier(SystemTime::now());
            let cost = cost_usd(
                &model,
                "deepseek",
                hit,
                miss,
                summary.output_tokens,
                multiplier,
            );
            serde_json::json!({
                "benchmark": "B1",
                "model": model,
                "success": true,
                "turns": summary.turns,
                "model_requests": summary.model_requests,
                "input_tokens": summary.input_tokens,
                "cached_input_tokens": summary.cached_input_tokens,
                "cache_hit_tokens": hit,
                "cache_miss_tokens": miss,
                "output_tokens": summary.output_tokens,
                "successful_tool_calls": summary.successful_tool_calls,
                "failed_tool_calls": summary.failed_tool_calls,
                "tool_failures_by_kind": failures,
                "tool_started_events": tally.tool_started,
                "tool_completed_events": tally.tool_completed,
                "retries": tally.retries,
                "retries_after_output": tally.retries_after_output,
                "events": tally.events,
                "events_dropped": summary.events_dropped,
                "wall_time_ms": summary.wall_time.as_millis() as u64,
                "observed_wall_time_ms": wall_time.as_millis() as u64,
                "price_window": window,
                "cost_usd": cost,
                "assistant_chars": assistant_chars(&session),
            })
        }
        Err(error) => serde_json::json!({
            "benchmark": "B1",
            "model": model,
            "success": false,
            "end_reason": error.end_reason(),
            "error": error.to_string(),
            "tool_failures_by_kind": failures,
            "retries": tally.retries,
            "wall_time_ms": wall_time.as_millis() as u64,
        }),
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert!(
        outcome.is_ok(),
        "B1 failed: {}",
        outcome.as_ref().unwrap_err()
    );
}

/// Splits input tokens into cache hits and misses.
///
/// `input_tokens` counts the whole prompt; `cached_input_tokens` is the subset
/// served from the provider cache, so the miss count is the difference.
fn cache_split(summary: &loom_core::kernel::agent::TaskSummary) -> (u64, u64) {
    let hit = summary.cached_input_tokens.unwrap_or(0);
    (hit, summary.input_tokens.saturating_sub(hit))
}

/// Counts committed tool results that failed, grouped by failure kind.
fn tool_failure_counts(session: &Session) -> BTreeMap<&'static str, u32> {
    let mut counts: BTreeMap<&'static str, u32> = BTreeMap::new();
    for item in &session.transcript {
        if let Item::ToolResult(result) = item
            && let ToolStatus::Error(failure) = &result.status
        {
            *counts.entry(failure_kind(failure)).or_default() += 1;
        }
    }
    counts
}

fn failure_kind(failure: &ToolFailure) -> &'static str {
    match failure {
        ToolFailure::UnknownTool => "unknown_tool",
        ToolFailure::NotExposed => "not_exposed",
        ToolFailure::InvalidInput => "invalid_input",
        ToolFailure::Execution => "execution",
        ToolFailure::Timeout => "timeout",
        // `ToolFailure` is `#[non_exhaustive]`.
        _ => "other",
    }
}

/// Total characters of assistant answers committed to the transcript.
fn assistant_chars(session: &Session) -> usize {
    session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(message) => Some(message.content.len()),
            _ => None,
        })
        .sum()
}
