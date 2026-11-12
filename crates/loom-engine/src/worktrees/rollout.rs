//! One rollout: child engine lifecycle, patch capture, and harness variation.

use std::path::{Path, PathBuf};

use crate::engine::{Engine, EngineConfig, EngineOverrides};
use loom_kernel::agent::TaskControl;
use loom_kernel::event::{ChannelEventSink, EventKind};
use loom_kernel::id::SessionId;
use loom_providers::ReasoningEffort;

use super::config::{ProviderFactory, RolloutReport};
use super::git::{git, git_raw};
/// Captures the worktree's full diff, SWE-bench style: drop the child's
/// `.loom` state first (session database, never part of a patch), stage
/// everything, then read the cached diff. Best-effort: a git failure here
/// yields an empty patch, not a failed orchestration.
pub(super) async fn capture_patch(worktree: &Path) -> String {
    let loom_dir = worktree.join(".loom");
    if loom_dir.is_dir() {
        let _ = tokio::fs::remove_dir_all(&loom_dir).await;
    }
    if git(worktree, &["add", "-A"]).await.is_err() {
        return String::new();
    }
    // Untrimmed: a patch is byte-exact evidence, and stripping the final
    // newline corrupts it for `git apply`.
    git_raw(worktree, &["diff", "--cached"])
        .await
        .unwrap_or_default()
}

/// Harness variation for child `index` under `vary_harness` (item 24):
/// diversify rollouts without changing the model. Child 0 always keeps
/// the given config (the baseline anchor and the shared-prefix rollout);
/// later children cycle terse → verbose so a fleet never runs identical
/// configs. Deterministic in `index` alone, so reruns reproduce.
pub(super) fn vary_child_config(config: &EngineConfig, index: usize) -> EngineConfig {
    let mut child = config.clone();
    match index {
        0 => {}
        i if i % 3 == 1 => {
            let b = &mut child.budgets;
            b.read_file_head_bytes = (b.read_file_head_bytes / 2).max(1);
            b.command_head_bytes = (b.command_head_bytes / 2).max(1);
            b.command_tail_bytes = (b.command_tail_bytes / 2).max(1);
            b.edit_echo_context_lines = 1;
            child.reasoning_effort = ReasoningEffort::Low;
        }
        _ => {
            let b = &mut child.budgets;
            b.read_file_head_bytes *= 2;
            b.command_head_bytes *= 2;
            b.command_tail_bytes *= 2;
            b.edit_echo_context_lines = 6;
        }
    }
    child
}
/// One rollout: open a child engine on the worktree, run the task, count
/// failure markers from the event stream, and capture the patch whatever
/// the outcome.
#[allow(clippy::too_many_arguments)] // one flat call site per rollout kind
pub(super) async fn run_rollout(
    index: usize,
    worktree: PathBuf,
    base_config: EngineConfig,
    provider_factory: ProviderFactory,
    task: String,
    cancellation: &tokio_util::sync::CancellationToken,
    // Continuation seed for shared-prefix mode: the prefix session to
    // continue and the steering input that starts every fork (identical
    // across children so the first forked request stays byte-stable).
    resume: Option<(SessionId, String)>,
    // Whether to capture the worktree patch on return.
    capture: bool,
) -> (RolloutReport, Option<SessionId>) {
    let mut report = RolloutReport {
        index,
        worktree: worktree.clone(),
        end_reason: None,
        summary: None,
        markers: 0,
        patch: String::new(),
        cost_note: String::new(),
        suite_passed: None,
    };
    if cancellation.is_cancelled() {
        report.end_reason = Some("cancelled".to_owned());
        return (report, None);
    }
    let provider = match provider_factory(index) {
        Ok(provider) => provider,
        Err(err) => {
            report.end_reason = Some(format!("provider_error: {err}"));
            return (report, None);
        }
    };
    let mut config = base_config;
    config.workspace = worktree.clone();
    config.db_path = worktree.join(".loom").join("loom.db");
    let engine = match Engine::open_with(
        config,
        EngineOverrides {
            provider: Some(provider),
            ..EngineOverrides::default()
        },
    ) {
        Ok(engine) => engine,
        Err(err) => {
            report.end_reason = Some(format!("open_error: {err}"));
            return (report, None);
        }
    };
    let (sink, mut receiver) = ChannelEventSink::new();
    let control = TaskControl::new();
    let watcher = {
        let control = control.clone();
        let token = cancellation.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            control.cancel();
        })
    };
    // Shared-prefix forks continue the seeded session with one steering
    // input; fresh rollouts start a new session on the task itself.
    let (outcome, session) = if let Some((id, input)) = resume.as_ref() {
        match engine
            .continue_session(id, input.clone(), &sink, &control)
            .await
        {
            Ok(continuation) => (continuation.outcome, continuation.session),
            Err(err) => {
                report.end_reason = Some(format!("prefix_load_error: {err}"));
                return (report, None);
            }
        }
    } else {
        let mut session = engine.new_session();
        let outcome = engine.run_task(&mut session, &task, &sink, &control).await;
        (outcome, session)
    };
    watcher.abort();
    let session_id = session.id;
    let mut nudges = 0u64;
    let mut failed_end = 0u64;
    while let Ok(envelope) = receiver.try_recv() {
        match envelope.kind {
            EventKind::LoopNudge { .. } => nudges += 1,
            EventKind::TaskFailed { .. } => failed_end += 1,
            _ => {}
        }
    }
    match outcome {
        Ok(summary) => {
            report.markers = nudges + u64::from(summary.failed_tool_calls) + failed_end;
            report.cost_note = format!(
                "turns: {}, model requests: {}, input tokens: {}, output tokens: {}",
                summary.turns, summary.model_requests, summary.input_tokens, summary.output_tokens
            );
            report.summary = Some(summary);
        }
        Err(err) => {
            report.markers = nudges + failed_end + 1;
            report.cost_note = format!("run failed: {err}");
        }
    }
    report.end_reason = session
        .tasks
        .last()
        .and_then(|task_record| task_record.end_reason.clone())
        .or(Some("run_error".to_owned()));
    // The shared-prefix rollout skips capture: its patch is taken after the
    // session database is staged (capture removes the `.loom` directory).
    if capture && !cancellation.is_cancelled() {
        report.patch = capture_patch(&worktree).await;
    }
    (report, Some(session_id))
}
