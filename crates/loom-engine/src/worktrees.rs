//! Parallel orchestration (roadmap item 21): worktree-per-agent isolation
//! with a deterministic merge path, above the engine.
//!
//! The orchestration is a sibling of the best-of-N seam item 24 names: N
//! child engines run the *same* task concurrently, each confined to its own
//! [`git worktree`], and a deterministic selector — never a judge model —
//! picks one rollout whose patch is applied to the parent workspace. The
//! design constraints come from `docs/subagents-design.md`: the parent
//! stays the only decision-maker, spend is visible at the point of use, and
//! every failure mode is data, not a panic.
//!
//! Guarantees:
//!
//! - **Fail-closed outside git.** A workspace without a repository is an
//!   error, not a degraded run: worktrees need one `HEAD` to branch from.
//! - **Isolation by construction.** Each child engine is a full
//!   [`Engine::open_with`] composition rooted at its worktree, so tool
//!   confinement, sandboxing, and the facts block all treat the worktree as
//!   the workspace — no shared mutable state between rollouts.
//! - **Patches even from failures.** Like the SWE-bench harness, a rollout
//!   that ran out of budget still yields its diff; partial work is evidence.
//! - **Deterministic selection.** Candidates rank by completion, then
//!   failure markers (loop nudges, failed tool calls, task failure), then
//!   turns, then model requests, then index — the Harness² fallback made
//! - **Execution-based pre-filter.** Before ranking, the fixed-table suite
//!   runs once per patched candidate (item 29): suite failures are dropped
//!   before selection, and unmeasurable candidates — no ecosystem, no
//!   verdict — stay ranked exactly as before. Deterministic spend only,
//!   never a model call.
//! - **Cancellation propagates.** One `CancellationToken` fans out to every
//!   child's `TaskControl`; in-flight turns complete per the loop's
//!   cancellation contract, worktrees are cleaned up, and nothing partial
//!   is applied.
//! - **Forensics on merge failure.** If the winning patch does not apply,
//!   worktrees are kept (not cleaned) so the run can be inspected.
//! - **Shared-prefix optionality.** With `shared_prefix_turns` set, one
//!   exploratory prefix rollout runs first in worktree 0; its session
//!   database and its patch are seeded into every sibling, and all N
//!   rollouts continue that session. The shared turns are paid once and
//!   the byte-stable projection makes them a provider prefix-cache hit
//!   in every child (item 28; SWE-Replay, arXiv 2601.22129). Default
//!   off — forked rollouts trade diversity for the shared spend.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use crate::engine::{Engine, EngineConfig, EngineError, EngineOverrides};
use loom_kernel::agent::{TaskControl, TaskSummary};
use loom_kernel::event::{ChannelEventSink, EventKind};
use loom_kernel::id::SessionId;
use loom_kernel::model::ModelProvider;
use loom_providers::ReasoningEffort;
use loom_tools::policy::CommandPolicy;
use loom_tools::verify::{VerifyTool, detect_ecosystem};
use tokio::sync::Semaphore;

/// Builds one provider instance per rollout. Providers are moved into
/// engines, so N rollouts need N instances; the production caller resolves
/// a credential once and reconstructs the (cheap, built-in-table) adapter
/// per child, while tests script different models per index.
pub type ProviderFactory =
    Arc<dyn Fn(usize) -> Result<Box<dyn ModelProvider>, EngineError> + Send + Sync>;

/// Knobs for [`best_of_n`]. `n` clamps to `2..=8` (below two there is
/// nothing to select; above eight the token spend dwarfs the selection
/// lift at current prices). `concurrency` clamps to `1..=n`.
#[derive(Debug, Clone)]
pub struct BestOfNConfig {
    /// Number of parallel rollouts.
    pub n: usize,
    /// Maximum rollouts in flight (semaphore permits).
    pub concurrency: usize,
    /// Keep worktrees (and the winning patch's siblings) for inspection.
    pub keep_worktrees: bool,
    /// Fork rollouts from a shared exploratory prefix of this many turns
    /// (item 28): one prefix rollout runs in worktree 0, then every
    /// rollout continues its session. `None` (default) runs every child
    /// from turn 0 as before.
    pub shared_prefix_turns: Option<u32>,
    /// Vary each rollout's harness instead of its model (item 24):
    /// child 0 keeps the config unchanged while later children cycle
    /// through terse (halved tool budgets, `Low` reasoning effort) and
    /// verbose (doubled tool budgets, wider edit echo) variants. Default
    /// off — identical children maximize prefix-cache reuse.
    pub vary_harness: bool,
    /// Run the fixed-table suite in every candidate worktree before
    /// ranking (item 29) and drop failing candidates. Deterministic
    /// spend only — no model call. Default on; opt out when the suite
    /// is too slow to run per rollout.
    pub verify_selection: bool,
}

impl Default for BestOfNConfig {
    fn default() -> Self {
        Self {
            n: 3,
            concurrency: 3,
            keep_worktrees: false,
            shared_prefix_turns: None,
            vary_harness: false,
            verify_selection: true,
        }
    }
}

/// One rollout's outcome, whether it completed, failed, or was cancelled.
#[derive(Debug, Clone)]
pub struct RolloutReport {
    /// Rollout index (`0..n`), stable across the report.
    pub index: usize,
    /// The worktree the child ran in (removed on cleanup).
    pub worktree: PathBuf,
    /// The task record's `end_reason` token (`end_turn`, `fast_path`,
    /// `max_turns`, `cancelled`, ...), or an orchestration-level label
    /// (`provider_error`, `open_error`, `run_error: ...`).
    pub end_reason: Option<String>,
    /// The loop's [`TaskSummary`] when the run returned one.
    pub summary: Option<TaskSummary>,
    /// Failure markers: loop nudges + failed tool calls + one for a failed
    /// task end. Lower is better; the Harness² all-fail selector.
    pub markers: u64,
    /// The rollout's full diff (`git diff --cached`), empty when nothing
    /// changed or the run never started.
    pub patch: String,
    /// Human-readable spend footer (turns, requests, tokens) so multi-rollout
    /// cost is visible at the point of use, scout-footer style.
    pub cost_note: String,
    /// Fixed-table suite verdict from the item 29 selection pass:
    /// `Some(false)` = the suite failed and the candidate is dropped
    /// from selection; `None` = not measured (no patch, filter off, or
    /// no verdict — never a drop).
    pub suite_passed: Option<bool>,
}

/// The whole orchestration result: per-rollout reports, the selected
/// winner, and whether its patch applied.
#[derive(Debug, Clone)]
pub struct BestOfNReport {
    /// One report per rollout, index order.
    pub runs: Vec<RolloutReport>,
    /// The winning rollout index, or `None` when no rollout produced a
    /// patch (or all were cancelled).
    pub winner: Option<usize>,
    /// Whether the winning patch was applied to the parent workspace.
    pub applied: bool,
    /// Why the winner's patch did not apply (git's stderr tail), or the
    /// no-candidate note.
    pub apply_note: String,
    /// Where the worktrees lived; removed unless kept for forensics.
    pub worktrees_root: PathBuf,
    /// The shared-prefix spend note (turns, requests, tokens, session id)
    /// when `shared_prefix_turns` produced a seeded prefix, else `None`.
    pub prefix_note: Option<String>,
    /// Set when `vary_harness` diversified the fleet's configs (item 24):
    /// describes the child cycle, else `None`.
    pub harness_note: Option<String>,
    /// The item 29 execution-filter note (verdicts, drops, rank ties)
    /// when the selection suite ran, else `None`.
    pub selection_note: Option<String>,
}

/// Errors that abort the orchestration before or between rollouts. Child
/// failures are reports, not errors.
#[derive(Debug, Clone)]
pub struct WorktreeError {
    /// The failure description.
    pub message: String,
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorktreeError {}

impl WorktreeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Runs `git` in `dir`, returning raw stdout. Non-zero exits carry the
/// stderr tail in the error.
async fn git_raw(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::new(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        )))
    }
}

/// Runs `git` in `dir`, returning trimmed stdout. Non-zero exits carry the
/// stderr tail in the error.
async fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    Ok(git_raw(dir, args).await?.trim().to_owned())
}

/// Runs `git` in `dir` feeding `stdin` to it.
async fn git_with_stdin(dir: &Path, args: &[&str], stdin: &str) -> Result<String, WorktreeError> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes())
            .await
            .map_err(|err| WorktreeError::new(format!("git stdin: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::new(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        )))
    }
}

/// Captures the worktree's full diff, SWE-bench style: drop the child's
/// `.loom` state first (session database, never part of a patch), stage
/// everything, then read the cached diff. Best-effort: a git failure here
/// yields an empty patch, not a failed orchestration.
async fn capture_patch(worktree: &Path) -> String {
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

/// Removes the worktrees' registrations and directories. Best-effort:
/// cleanup failures do not fail the report; the caller can re-prune.
async fn clean_worktrees(repo_root: &Path, root: &Path, count: usize) {
    for index in 0..count {
        let dir = root.join(index.to_string());
        let dir_arg = dir.to_string_lossy().into_owned();
        let _ = git(repo_root, &["worktree", "remove", "--force", &dir_arg]).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    let _ = git(repo_root, &["worktree", "prune"]).await;
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// The deterministic rank key (item 21): completion first, then fewest
/// markers, then fewest turns, then fewest model requests. The rollout
/// index breaks any remaining tie, so selection is reproducible from the
/// report alone.
fn rank_key(run: &RolloutReport) -> (bool, u64, u32, u32) {
    (
        !matches!(
            run.end_reason.as_deref(),
            Some("end_turn") | Some("fast_path")
        ),
        run.markers,
        run.summary.as_ref().map(|s| s.turns).unwrap_or(u32::MAX),
        run.summary
            .as_ref()
            .map(|s| s.model_requests)
            .unwrap_or(u32::MAX),
    )
}

/// Picks the winner: candidates need a non-empty patch and must not have
/// failed the item 29 selection suite; ranking is the [`rank_key`] order
/// with the rollout index as the final tie-break.
fn winner_index(runs: &[RolloutReport]) -> Option<usize> {
    runs.iter()
        .filter(|run| !run.patch.trim().is_empty() && run.suite_passed != Some(false))
        .min_by_key(|run| (rank_key(run), run.index))
        .map(|run| run.index)
}

/// Item 29 selection pass: run the fixed-table suite in every candidate
/// worktree that produced a patch, record the verdict on the report, and
/// return the note for the selection record. `Some(false)` verdicts drop
/// the candidate in [`winner_index`]; `None` verdicts never do. The
/// rollout semaphore bounds suite concurrency exactly like the rollouts.
async fn run_selection_suites(
    runs: &mut [RolloutReport],
    repo_root: &Path,
    semaphore: &Arc<Semaphore>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    // Repositories without a fixed-table suite stay on pure ranking; a
    // `.loom/verify.toml` runner is worktree-local state the parent
    // cannot rely on here, so detection is the parent's table alone.
    detect_ecosystem(repo_root)?;
    let policy = match CommandPolicy::from_manifest(repo_root) {
        Ok(policy) => policy,
        Err(_) => {
            return Some("execution filter skipped: policy manifest unreadable".to_owned());
        }
    };
    let mut tasks = Vec::new();
    for run in runs.iter() {
        if run.patch.trim().is_empty() {
            continue;
        }
        let permit_source = semaphore.clone();
        let worktree = run.worktree.clone();
        let tool = VerifyTool::with_policy(policy.clone());
        let token = cancellation.clone();
        tasks.push((
            run.index,
            tokio::spawn(async move {
                let _permit = permit_source.acquire_owned().await;
                tool.suite_verdict(&worktree, &token).await
            }),
        ));
    }
    let mut measured = 0usize;
    let mut dropped = 0usize;
    for (index, task) in tasks {
        // A join failure or missing verdict is "no evidence": the
        // candidate keeps its rank.
        let verdict = task.await.ok().flatten();
        if let Some(run) = runs.iter_mut().find(|run| run.index == index) {
            run.suite_passed = verdict;
        }
        measured += usize::from(verdict.is_some());
        dropped += usize::from(verdict == Some(false));
    }
    Some(format!(
        "execution filter: {measured} suite verdict(s), {dropped} candidate(s) dropped"
    ))
}

/// Harness variation for child `index` under `vary_harness` (item 24):
/// diversify rollouts without changing the model. Child 0 always keeps
/// the given config (the baseline anchor and the shared-prefix rollout);
/// later children cycle terse → verbose so a fleet never runs identical
/// configs. Deterministic in `index` alone, so reruns reproduce.
fn vary_child_config(config: &EngineConfig, index: usize) -> EngineConfig {
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
async fn run_rollout(
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

/// Resolves the credential once (the same order as [`Engine::open`]) and
/// returns a [`ProviderFactory`] that builds one provider per rollout
/// from `config` — the production counterpart to the scripted test
/// factories. Catalog-driven adapters need `config.catalog` set first;
/// call [`Engine::resolve_catalog`] before this.
pub fn provider_factory(config: &EngineConfig) -> Result<ProviderFactory, EngineError> {
    let store = crate::auth::SqliteAuthStore::open(&config.db_path).map_err(EngineError::Store)?;
    let secrets = crate::auth::KeychainBackend::default();
    let credential = crate::auth::resolve(
        &store,
        &secrets,
        &config.provider,
        config.account.as_deref(),
        |name| std::env::var(name).ok(),
    )?;
    let secret = credential.secret.expose().to_owned();
    let config = config.clone();
    Ok(Arc::new(move |_index| {
        crate::provider::build(&config, &secret).map(|built| built.provider)
    }))
}

/// Runs the task on N isolated worktrees and merges one winner.
///
/// Preparation is serialized (git's index lock is global per repository);
/// rollouts run concurrently under a semaphore. The winning patch is
/// applied to the repository root. On success worktrees are cleaned up
/// unless [`BestOfNConfig::keep_worktrees`] is set or the patch failed to
/// apply — in both cases the trees stay for inspection.
pub async fn best_of_n(
    mut config: EngineConfig,
    provider_factory: ProviderFactory,
    task: &str,
    cfg: BestOfNConfig,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<BestOfNReport, WorktreeError> {
    let n = cfg.n.clamp(2, 8);
    let concurrency = cfg.concurrency.clamp(1, n);
    let repo_root = git(&config.workspace, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| {
            WorktreeError::new(format!(
                "workspace {:?} is not inside a git repository; parallel rollouts need one HEAD to branch from",
                config.workspace
            ))
        })?;
    let repo_root = PathBuf::from(repo_root);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let root = repo_root.join(".loom").join("worktrees").join(format!(
        "best-of-{}-{}",
        stamp,
        std::process::id()
    ));
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|err| WorktreeError::new(format!("create {}: {err}", root.display())))?;
    // Serialized preparation: `git worktree add` takes the repository's
    // index lock, so parallel adds would serialize (or fail) anyway.
    let mut worktrees = Vec::with_capacity(n);
    for index in 0..n {
        let dir = root.join(index.to_string());
        if let Err(err) = git(
            &repo_root,
            &[
                "worktree",
                "add",
                "--detach",
                &dir.to_string_lossy(),
                "HEAD",
            ],
        )
        .await
        {
            clean_worktrees(&repo_root, &root, worktrees.len()).await;
            return Err(err);
        }
        worktrees.push(dir);
    }
    // Item 28: optionally fork every rollout from one shared exploratory
    // prefix instead of turn 0. Failures abort the whole orchestration —
    // a half-seeded fleet would silently drop the sharing we promised.
    let prefix = match cfg.shared_prefix_turns {
        Some(turns) => {
            // Seeding and continuing sessions needs the session store;
            // force persistence on for the whole fleet (item 28).
            config.persist = true;
            match run_shared_prefix(
                turns.max(1),
                &worktrees,
                &config,
                &provider_factory,
                task,
                cancellation,
                &root,
            )
            .await
            {
                Ok(prefix) => Some(prefix),
                Err(err) => {
                    clean_worktrees(&repo_root, &root, n).await;
                    return Err(err);
                }
            }
        }
        None => None,
    };
    let resume = prefix
        .as_ref()
        .map(|prefix| (prefix.session_id, SHARED_PREFIX_INPUT.to_owned()));
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut children = Vec::with_capacity(n);
    for (index, worktree) in worktrees.into_iter().enumerate() {
        let permit_source = semaphore.clone();
        let base_config = if cfg.vary_harness {
            vary_child_config(&config, index)
        } else {
            config.clone()
        };
        let provider_factory = provider_factory.clone();
        let task = task.to_owned();
        let token = cancellation.clone();
        let resume = resume.clone();
        children.push(tokio::spawn(async move {
            let _permit = permit_source.acquire_owned().await;
            run_rollout(
                index,
                worktree,
                base_config,
                provider_factory,
                task,
                &token,
                resume,
                true,
            )
            .await
            .0
        }));
    }
    let mut runs = Vec::with_capacity(n);
    for child in children {
        match child.await {
            Ok(report) => runs.push(report),
            Err(err) => {
                runs.push(RolloutReport {
                    index: runs.len(),
                    worktree: PathBuf::new(),
                    end_reason: Some(format!("join_error: {err}")),
                    summary: None,
                    markers: u64::MAX,
                    patch: String::new(),
                    cost_note: String::new(),
                    suite_passed: None,
                });
            }
        }
    }
    runs.sort_by_key(|run| run.index);
    // Item 29: execution-based selection before ranking — the fixed-table
    // suite runs once per patched candidate (deterministic spend only, no
    // model call) and failing candidates are dropped. No verdict is no
    // evidence: unmeasurable candidates stay in the ranking.
    let selection_note = if cfg.verify_selection && !cancellation.is_cancelled() {
        run_selection_suites(&mut runs, &repo_root, &semaphore, cancellation).await
    } else {
        None
    };
    let winner = winner_index(&runs);
    let selection_note = match (selection_note, winner) {
        (Some(note), Some(won)) => {
            // The winner comes from `runs`, so its key always exists; a
            // missing entry would mean the report is internally wrong.
            let key = runs
                .iter()
                .find(|run| run.index == won)
                .map(rank_key)
                .unwrap_or((true, u64::MAX, u32::MAX, u32::MAX));
            let ties = runs
                .iter()
                .filter(|run| {
                    run.index != won
                        && !run.patch.trim().is_empty()
                        && run.suite_passed != Some(false)
                        && rank_key(run) == key
                })
                .count();
            if ties > 0 {
                Some(format!(
                    "{note}; {ties} rank tie(s) broken by rollout index"
                ))
            } else {
                Some(note)
            }
        }
        (note, _) => note,
    };
    let mut keep = cfg.keep_worktrees;
    let (applied, apply_note) = if cancellation.is_cancelled() {
        (false, "cancelled before merge; nothing applied".to_owned())
    } else if let Some(index) = winner {
        let patch = runs[index].patch.clone();
        match git_with_stdin(&repo_root, &["apply", "--whitespace=nowarn"], &patch).await {
            Ok(_) => (true, format!("rollout {index} patch applied")),
            Err(err) => {
                keep = true;
                (
                    false,
                    format!("rollout {index} patch did not apply: {err}; worktrees kept"),
                )
            }
        }
    } else {
        (
            false,
            "no rollout produced a patch; nothing to merge".to_owned(),
        )
    };
    if !keep {
        clean_worktrees(&repo_root, &root, n).await;
    }
    Ok(BestOfNReport {
        runs,
        winner,
        applied,
        apply_note,
        worktrees_root: root,
        prefix_note: prefix.map(|prefix| prefix.note),
        harness_note: cfg.vary_harness.then(|| {
            "child 0 baseline; later children cycle terse (halved tool budgets, low reasoning effort) and verbose (doubled budgets, wider edit echo)".to_owned()
        }),
        selection_note,
    })
}

/// Steering input that starts every shared-prefix fork. Identical across
/// children so the first forked request stays byte-stable (item 28).
const SHARED_PREFIX_INPUT: &str = "Continue the task above to completion; the earlier turns already happened. Verify your work before finishing.";

/// A seeded shared prefix (item 28): the session every rollout continues
/// and the note the report shows for the once-paid spend.
struct SharedPrefix {
    session_id: SessionId,
    note: String,
}

/// Runs the shared exploratory prefix (item 28) in worktree 0 under a
/// hard `turns` ceiling, then seeds the fleet: the prefix patch is applied
/// to every sibling so each starts from the prefix-final tree, and the
/// session database (WAL sidecars included) is copied so each child
/// continues the same transcript. Worktree 0 keeps running in place.
///
/// Stopping at the turn ceiling is the expected ending, not a failure; only
/// open/provider errors and cancellation abort the orchestration.
async fn run_shared_prefix(
    turns: u32,
    worktrees: &[PathBuf],
    base_config: &EngineConfig,
    provider_factory: &ProviderFactory,
    task: &str,
    cancellation: &tokio_util::sync::CancellationToken,
    root: &Path,
) -> Result<SharedPrefix, WorktreeError> {
    let worktree = worktrees[0].clone();
    let mut config = base_config.clone();
    config.workspace = worktree.clone();
    config.db_path = worktree.join(".loom").join("loom.db");
    config.max_turns = turns;
    let (report, session_id) = run_rollout(
        0,
        worktree.clone(),
        config,
        provider_factory.clone(),
        task.to_owned(),
        cancellation,
        None,
        false,
    )
    .await;
    if cancellation.is_cancelled() {
        return Err(WorktreeError::new("shared prefix cancelled"));
    }
    if let Some(reason) = report.end_reason.as_deref()
        && (reason.starts_with("open_error") || reason.starts_with("provider_error"))
    {
        return Err(WorktreeError::new(format!(
            "shared prefix failed in worktree 0: {reason}"
        )));
    }
    let session_id = session_id
        .ok_or_else(|| WorktreeError::new("shared prefix produced no session to continue"))?;
    // Stage the session database before `capture_patch` deletes the
    // worktree's `.loom` directory; the WAL sidecars carry committed
    // turns that a dropped connection may not have checkpointed.
    let staged = root.join("prefix-db");
    stage_session_db(&worktree.join(".loom"), &staged).await?;
    let patch = capture_patch(&worktree).await;
    seed_worktree(&worktree, &staged, "").await?;
    for sibling in worktrees.iter().skip(1) {
        seed_worktree(sibling, &staged, &patch).await?;
    }
    Ok(SharedPrefix {
        session_id,
        note: format!(
            "session {session_id} (ceiling {turns}): {}",
            report.cost_note
        ),
    })
}

/// Copies the session database and its WAL sidecars from `loom_dir` into
/// `staged` so siblings can be seeded from one durable snapshot.
async fn stage_session_db(loom_dir: &Path, staged: &Path) -> Result<(), WorktreeError> {
    tokio::fs::create_dir_all(staged)
        .await
        .map_err(|err| WorktreeError::new(format!("create {}: {err}", staged.display())))?;
    for name in ["loom.db", "loom.db-wal", "loom.db-shm"] {
        let source = loom_dir.join(name);
        if !tokio::fs::try_exists(&source).await.unwrap_or(false) {
            // Sidecars are optional; the database itself is not.
            if name == "loom.db" {
                return Err(WorktreeError::new(format!(
                    "stage {} from {}",
                    name,
                    loom_dir.display()
                )));
            }
            continue;
        }
        let dest = staged.join(name);
        if let Err(err) = tokio::fs::copy(&source, &dest).await {
            return Err(WorktreeError::new(format!(
                "stage {} to {}: {err}",
                name,
                dest.display()
            )));
        }
    }
    Ok(())
}

/// Seeds one worktree: restores the session database (a missing `.loom`
/// in worktree 0 is expected after `capture_patch`) and applies the prefix
/// patch when there is one. Worktree 0 gets an empty patch — its tree
/// already carries the prefix changes.
async fn seed_worktree(worktree: &Path, staged: &Path, patch: &str) -> Result<(), WorktreeError> {
    let loom_dir = worktree.join(".loom");
    tokio::fs::create_dir_all(&loom_dir)
        .await
        .map_err(|err| WorktreeError::new(format!("create {}: {err}", loom_dir.display())))?;
    for name in ["loom.db", "loom.db-wal", "loom.db-shm"] {
        let source = staged.join(name);
        if tokio::fs::try_exists(&source).await.unwrap_or(false) {
            let dest = loom_dir.join(name);
            tokio::fs::copy(&source, &dest).await.map_err(|err| {
                WorktreeError::new(format!(
                    "copy {} to {}: {err}",
                    source.display(),
                    dest.display()
                ))
            })?;
        }
    }
    if !patch.is_empty() {
        git_with_stdin(worktree, &["apply", "--whitespace=nowarn"], patch)
            .await
            .map_err(|err| {
                WorktreeError::new(format!(
                    "seed prefix patch in {}: {err}",
                    worktree.display()
                ))
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use loom_kernel::model::{
        ModelCapabilities, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
        ModelStream, StopReason, Usage,
    };
    use loom_kernel::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A provider replaying one scripted event list per `stream()` call —
    /// the same shape as the engine tests' fake, local so this module's
    /// tests stay self-contained.
    struct ScriptedProvider {
        scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    }

    impl ScriptedProvider {
        fn scripted(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from(scripts)),
            }
        }

        fn end_turn_script(text: &str) -> Vec<Result<ModelEvent, ModelError>> {
            vec![Ok(ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: text.to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                },
                usage: Usage::default(),
            }))]
        }
    }

    impl ModelProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(stream::iter(script))
        }
    }

    fn test_config(workspace: &Path, db: &Path) -> EngineConfig {
        let mut config = crate::engine::test_config();
        config.workspace = workspace.to_path_buf();
        config.db_path = db.to_path_buf();
        config.max_turns = 3;
        config
    }

    #[test]
    fn vary_child_config_cycles_terse_and_verbose() {
        let workspace = std::env::temp_dir().join("loom-vary-test");
        let base = test_config(&workspace, &workspace.join("loom.db"));
        let zero = vary_child_config(&base, 0);
        assert_eq!(
            zero.budgets.read_file_head_bytes,
            base.budgets.read_file_head_bytes
        );
        assert_eq!(zero.reasoning_effort, base.reasoning_effort);

        let terse = vary_child_config(&base, 1);
        assert_eq!(
            terse.budgets.read_file_head_bytes,
            base.budgets.read_file_head_bytes / 2
        );
        assert_eq!(terse.budgets.edit_echo_context_lines, 1);
        assert_eq!(terse.reasoning_effort, ReasoningEffort::Low);

        let verbose = vary_child_config(&base, 2);
        assert_eq!(
            verbose.budgets.command_head_bytes,
            base.budgets.command_head_bytes * 2
        );
        assert_eq!(verbose.budgets.edit_echo_context_lines, 6);
        // The cycle repeats deterministically.
        assert_eq!(
            vary_child_config(&base, 4).budgets.read_file_head_bytes,
            terse.budgets.read_file_head_bytes
        );
        // The base config is never mutated.
        assert_eq!(base.budgets.edit_echo_context_lines, 3);
    }
    /// A script whose first turn writes one file, second turn ends.
    fn write_then_end_script() -> Vec<Vec<Result<ModelEvent, ModelError>>> {
        let call = ToolCall {
            id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({
                "path": "answer.txt",
                "content": "best-of-n winner\n",
            }))
            .expect("arguments"),
        };
        vec![
            vec![Ok(ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: String::new(),
                    reasoning: None,
                    tool_calls: vec![call],
                    stop_reason: StopReason::ToolCalls,
                },
                usage: Usage::default(),
            }))],
            ScriptedProvider::end_turn_script("done"),
        ]
    }

    async fn git_repo(base: &Path) -> PathBuf {
        let repo = base.join("repo");
        tokio::fs::create_dir_all(&repo).await.expect("repo dir");
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "base",
            ],
        ] {
            git(&repo, &args).await.expect("git init");
        }
        repo
    }

    /// One scripted provider per rollout index.
    fn factory_for(scripts: Vec<Vec<Vec<Result<ModelEvent, ModelError>>>>) -> ProviderFactory {
        let scripts = Arc::new(scripts);
        Arc::new(move |index| {
            let script = scripts
                .get(index)
                .cloned()
                .unwrap_or_else(|| vec![ScriptedProvider::end_turn_script("fallback")]);
            Ok(Box::new(ScriptedProvider::scripted(script)) as Box<dyn ModelProvider>)
        })
    }

    #[tokio::test]
    async fn best_of_n_applies_the_winning_patch_and_cleans_up() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;
        let token = tokio_util::sync::CancellationToken::new();
        // Rollout 0 writes a file; rollout 1 idles with no changes.
        let factory = factory_for(vec![
            write_then_end_script(),
            vec![ScriptedProvider::end_turn_script("nothing to do")],
        ]);
        let report = best_of_n(
            test_config(&repo, &base.path().join("loom.db")),
            factory,
            "write the answer file",
            BestOfNConfig {
                n: 2,
                concurrency: 2,
                keep_worktrees: false,
                shared_prefix_turns: None,
                vary_harness: false,
                verify_selection: false,
            },
            &token,
        )
        .await
        .expect("orchestration");
        assert_eq!(report.winner, Some(0), "the rollout with a patch wins");
        assert!(report.applied, "patch applied: {}", report.apply_note);
        let answer = tokio::fs::read_to_string(repo.join("answer.txt"))
            .await
            .expect("winner patch applied to parent workspace");
        assert_eq!(answer, "best-of-n winner\n");
        assert!(!report.worktrees_root.exists(), "worktrees cleaned up");
        assert!(report.runs[0].patch.contains("answer.txt"));
    }

    #[tokio::test]
    async fn execution_filter_drops_suite_failures_before_ranking() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;
        let token = tokio_util::sync::CancellationToken::new();
        // The suite fails exactly when answer.txt exists. It is committed
        // before the fleet so every candidate worktree inherits it.
        tokio::fs::write(repo.join("Makefile"), "test:\n\t@[ ! -f answer.txt ]\n")
            .await
            .expect("makefile");
        git(&repo, &["add", "-A"]).await.expect("stage makefile");
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "-m",
                "suite",
            ],
        )
        .await
        .expect("commit makefile");
        // Rollout 0 would rank first (done, no markers) but writes
        // answer.txt and fails the suite; rollout 1 writes a different
        // file and survives the filter.
        let other_call = ToolCall {
            id: ToolCallId::try_from(format!("call-other-{}", std::process::id())).unwrap(),
            name: "write_file".to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({
                "path": "other.txt",
                "content": "runner-up\n",
            }))
            .expect("arguments"),
        };
        let runner_up = vec![
            vec![Ok(ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: String::new(),
                    reasoning: None,
                    tool_calls: vec![other_call],
                    stop_reason: StopReason::ToolCalls,
                },
                usage: Usage::default(),
            }))],
            ScriptedProvider::end_turn_script("done"),
        ];
        let factory = factory_for(vec![write_then_end_script(), runner_up]);
        let report = best_of_n(
            test_config(&repo, &base.path().join("loom.db")),
            factory,
            "write a file",
            BestOfNConfig {
                n: 2,
                concurrency: 2,
                keep_worktrees: false,
                shared_prefix_turns: None,
                vary_harness: false,
                verify_selection: true,
            },
            &token,
        )
        .await
        .expect("orchestration");
        assert_eq!(
            report.runs[0].suite_passed,
            Some(false),
            "the suite measured rollout 0"
        );
        assert_eq!(report.winner, Some(1), "the suite failure drops rollout 0");
        assert!(report.applied, "runner-up applied: {}", report.apply_note);
        assert!(
            report
                .selection_note
                .as_deref()
                .unwrap_or_default()
                .contains("dropped")
        );
        let other = tokio::fs::read_to_string(repo.join("other.txt"))
            .await
            .expect("runner-up patch applied");
        assert_eq!(other, "runner-up\n");
    }

    #[tokio::test]
    async fn shared_prefix_seeds_the_fleet() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;
        let token = tokio_util::sync::CancellationToken::new();
        // Factory(0) serves both the prefix rollout and rollout 0 (script
        // re-cloned per call): write the file, then end. Rollout 1 ends
        // immediately on the seeded tree.
        let factory = factory_for(vec![
            write_then_end_script(),
            vec![ScriptedProvider::end_turn_script("done")],
        ]);
        let report = best_of_n(
            test_config(&repo, &base.path().join("loom.db")),
            factory,
            "write the answer file",
            BestOfNConfig {
                n: 2,
                concurrency: 2,
                keep_worktrees: false,
                shared_prefix_turns: Some(1),
                vary_harness: false,
                verify_selection: false,
            },
            &token,
        )
        .await
        .expect("best-of-n");
        // Every child carries the prefix write, so the winner patch lands
        // the prefix's file even when the winning child only ended the turn.
        assert!(report.applied, "no patch applied");
        assert!(report.prefix_note.is_some(), "prefix spend not reported");
        let answer = tokio::fs::read_to_string(repo.join("answer.txt"))
            .await
            .expect("winner patch applied");
        assert_eq!(answer, "best-of-n winner\n");
        assert!(!report.worktrees_root.exists(), "worktrees cleaned up");
    }

    #[tokio::test]
    async fn best_of_n_without_candidates_reports_no_winner() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;
        let token = tokio_util::sync::CancellationToken::new();
        let factory = factory_for(vec![
            vec![ScriptedProvider::end_turn_script("idle")],
            vec![ScriptedProvider::end_turn_script("idle too")],
        ]);
        let report = best_of_n(
            test_config(&repo, &base.path().join("loom.db")),
            factory,
            "do nothing",
            BestOfNConfig {
                n: 2,
                concurrency: 2,
                keep_worktrees: false,
                shared_prefix_turns: None,
                vary_harness: false,
                verify_selection: false,
            },
            &token,
        )
        .await
        .expect("orchestration");
        assert_eq!(report.winner, None);
        assert!(!report.applied);
        assert!(report.apply_note.contains("no rollout produced a patch"));
        assert!(!report.worktrees_root.exists(), "worktrees cleaned up");
    }

    #[tokio::test]
    async fn best_of_n_fails_closed_outside_a_git_repository() {
        let base = tempfile::tempdir().expect("tempdir");
        let plain = base.path().join("plain");
        tokio::fs::create_dir_all(&plain).await.expect("dir");
        let token = tokio_util::sync::CancellationToken::new();
        let factory: ProviderFactory = Arc::new(|_| {
            Ok(Box::new(ScriptedProvider::scripted(vec![
                ScriptedProvider::end_turn_script("x"),
            ])))
        });
        let err = best_of_n(
            test_config(&plain, &base.path().join("loom.db")),
            factory,
            "task",
            BestOfNConfig {
                n: 2,
                concurrency: 2,
                keep_worktrees: false,
                shared_prefix_turns: None,
                vary_harness: false,
                verify_selection: false,
            },
            &token,
        )
        .await
        .expect_err("non-git workspace");
        assert!(
            err.message.contains("git repository"),
            "message: {}",
            err.message
        );
    }

    #[test]
    fn winner_ranking_prefers_completion_then_fewest_markers() {
        let report = |index: usize, end: Option<&str>, markers: u64, patch: &str| RolloutReport {
            index,
            worktree: PathBuf::new(),
            end_reason: end.map(str::to_owned),
            summary: None,
            markers,
            patch: patch.to_owned(),
            cost_note: String::new(),
            suite_passed: None,
        };
        let runs = vec![
            report(0, Some("max_turns"), 4, "diff"),
            report(1, Some("end_turn"), 0, "diff"),
            report(2, Some("end_turn"), 2, "diff"),
            report(3, Some("end_turn"), 0, ""),
        ];
        assert_eq!(
            winner_index(&runs),
            Some(1),
            "completed, fewest markers, non-empty patch"
        );
        let ties = vec![
            report(1, Some("end_turn"), 0, "diff"),
            report(0, Some("end_turn"), 0, "diff"),
        ];
        assert_eq!(winner_index(&ties), Some(0), "index breaks full ties");
    }
}
