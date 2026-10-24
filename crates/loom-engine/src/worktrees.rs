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
//!   the primary selector, auditable from the report alone.
//! - **Cancellation propagates.** One `CancellationToken` fans out to every
//!   child's `TaskControl`; in-flight turns complete per the loop's
//!   cancellation contract, worktrees are cleaned up, and nothing partial
//!   is applied.
//! - **Forensics on merge failure.** If the winning patch does not apply,
//!   worktrees are kept (not cleaned) so the run can be inspected.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use crate::engine::{Engine, EngineConfig, EngineError, EngineOverrides};
use loom_kernel::agent::{TaskControl, TaskSummary};
use loom_kernel::event::{ChannelEventSink, EventKind};
use loom_kernel::model::ModelProvider;
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
}

impl Default for BestOfNConfig {
    fn default() -> Self {
        Self {
            n: 3,
            concurrency: 3,
            keep_worktrees: false,
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

/// Picks the winner: candidates need a non-empty patch; ranking is
/// completion first, then fewest markers, then fewest turns, then fewest
/// model requests, then lowest index — a total order, so selection is
/// reproducible from the report alone.
fn winner_index(runs: &[RolloutReport]) -> Option<usize> {
    runs.iter()
        .filter(|run| !run.patch.trim().is_empty())
        .min_by(|a, b| {
            // Completion signal: an `end_reason` of a successful stop.
            // `b.cmp(&a)`: done sorts first (bools order false < true).
            let a_done = matches!(
                a.end_reason.as_deref(),
                Some("end_turn") | Some("fast_path")
            );
            let b_done = matches!(
                b.end_reason.as_deref(),
                Some("end_turn") | Some("fast_path")
            );
            b_done
                .cmp(&a_done)
                .then(a.markers.cmp(&b.markers))
                .then(
                    a.summary
                        .as_ref()
                        .map(|s| s.turns)
                        .unwrap_or(u32::MAX)
                        .cmp(&b.summary.as_ref().map(|s| s.turns).unwrap_or(u32::MAX)),
                )
                .then(
                    a.summary
                        .as_ref()
                        .map(|s| s.model_requests)
                        .unwrap_or(u32::MAX)
                        .cmp(
                            &b.summary
                                .as_ref()
                                .map(|s| s.model_requests)
                                .unwrap_or(u32::MAX),
                        ),
                )
                .then(a.index.cmp(&b.index))
        })
        .map(|run| run.index)
}

/// One rollout: open a child engine on the worktree, run the task, count
/// failure markers from the event stream, and capture the patch whatever
/// the outcome.
async fn run_rollout(
    index: usize,
    worktree: PathBuf,
    base_config: EngineConfig,
    provider_factory: ProviderFactory,
    task: String,
    cancellation: &tokio_util::sync::CancellationToken,
) -> RolloutReport {
    let mut report = RolloutReport {
        index,
        worktree: worktree.clone(),
        end_reason: None,
        summary: None,
        markers: 0,
        patch: String::new(),
        cost_note: String::new(),
    };
    if cancellation.is_cancelled() {
        report.end_reason = Some("cancelled".to_owned());
        return report;
    }
    let provider = match provider_factory(index) {
        Ok(provider) => provider,
        Err(err) => {
            report.end_reason = Some(format!("provider_error: {err}"));
            return report;
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
            return report;
        }
    };
    let mut session = engine.new_session();
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
    let outcome = engine.run_task(&mut session, &task, &sink, &control).await;
    watcher.abort();
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
    if !cancellation.is_cancelled() {
        report.patch = capture_patch(&worktree).await;
    }
    report
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
    config: EngineConfig,
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
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut children = Vec::with_capacity(n);
    for (index, worktree) in worktrees.into_iter().enumerate() {
        let permit_source = semaphore.clone();
        let base_config = config.clone();
        let provider_factory = provider_factory.clone();
        let task = task.to_owned();
        let token = cancellation.clone();
        children.push(tokio::spawn(async move {
            let _permit = permit_source.acquire_owned().await;
            run_rollout(index, worktree, base_config, provider_factory, task, &token).await
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
                });
            }
        }
    }
    runs.sort_by_key(|run| run.index);
    let winner = winner_index(&runs);
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
    })
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
