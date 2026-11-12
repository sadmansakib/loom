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

mod best_of_n;
mod config;
mod exclude;
mod git;
mod prefix;
mod rollout;
mod select;

pub use best_of_n::{best_of_n, provider_factory};
pub use config::{BestOfNConfig, BestOfNReport, ProviderFactory, RolloutReport, WorktreeError};
pub use exclude::exclude_state_from_git;

#[cfg(test)]
use crate::engine::EngineConfig;
#[cfg(test)]
use git::{git, git_raw};
#[cfg(test)]
use loom_providers::ReasoningEffort;
#[cfg(test)]
use rollout::vary_child_config;
#[cfg(test)]
use select::winner_index;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;

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
    async fn best_of_n_writes_local_git_excludes_for_state_dirs() {
        // Review item 2.8: the worktrees dir must never ride into the
        // operator's index (git add -A would stage a broken gitlink).
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;
        exclude_state_from_git(&repo, &[".loom/worktrees/"]);
        let git_dir = std::path::PathBuf::from(
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["rev-parse", "--path-format=absolute", "--git-dir"])
                    .output()
                    .expect("rev-parse")
                    .stdout,
            )
            .trim()
            .to_owned(),
        );
        let exclude = git_dir.join("info").join("exclude");
        let body = std::fs::read_to_string(&exclude).expect("exclude written");
        assert!(body.contains(".loom/worktrees/"), "{body}");

        // Idempotent: a second call must not duplicate the entry.
        exclude_state_from_git(&repo, &[".loom/worktrees/"]);
        let again = std::fs::read_to_string(&exclude).expect("exclude readable");
        assert_eq!(again.matches(".loom/worktrees/").count(), 1, "{again}");

        // And git actually honors it.
        let ignored = git(
            &repo,
            &["check-ignore", "-q", ".loom/worktrees/best-of-1/0"],
        )
        .await
        .is_ok();
        assert!(ignored, "git must ignore the worktrees root");
    }

    /// F-03 regression: `.git/config` naming a hooks directory
    /// (`core.hooksPath`) must not fire during loom's own git runs.
    /// `reset --hard` executes `post-checkout` when the knob is
    /// honored — exactly the shape of the CLI rewind path.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_invocations_ignore_a_planted_hooks_path() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = git_repo(base.path()).await;

        // Plant the hooks dir outside the repo with a post-checkout
        // hook whose whole job is to leave a probe behind.
        let probe = base.path().join("hook-ran");
        let hooks = base.path().join("planted-hooks");
        tokio::fs::create_dir_all(&hooks).await.expect("hooks dir");
        tokio::fs::write(
            hooks.join("post-checkout"),
            format!("#!/bin/sh\ntouch {}\n", probe.display()),
        )
        .await
        .expect("hook body");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                hooks.join("post-checkout"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("hook exec bit");
        }
        git(
            &repo,
            &[
                "config",
                "core.hooksPath",
                hooks.to_str().expect("utf8 path"),
            ],
        )
        .await
        .expect("plant hooksPath");

        // `reset --hard` fires post-checkout when the knob is honored.
        git_raw(&repo, &["reset", "--hard", "HEAD"])
            .await
            .expect("hardened reset still works");
        assert!(
            !probe.exists(),
            "planted core.hooksPath hook executed during loom's git run"
        );
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
