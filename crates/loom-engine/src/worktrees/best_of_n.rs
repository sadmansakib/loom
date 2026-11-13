//! The best-of-N orchestration driver and its production provider factory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::engine::{EngineConfig, EngineError};
use tokio::sync::Semaphore;

use super::config::{BestOfNConfig, BestOfNReport, ProviderFactory, RolloutReport, WorktreeError};
use super::exclude::exclude_state_from_git;
use super::git::{git, git_with_stdin};
use super::prefix::{SHARED_PREFIX_INPUT, run_shared_prefix};
use super::rollout::{run_rollout, vary_child_config};
use super::select::{rank_key, run_selection_suites, winner_index};
/// Removes the worktrees' registrations and directories. Best-effort:
/// cleanup failures do not fail the report; the caller can re-prune.
pub(super) async fn clean_worktrees(repo_root: &Path, root: &Path, count: usize) {
    for index in 0..count {
        let dir = root.join(index.to_string());
        let dir_arg = dir.to_string_lossy().into_owned();
        let _ = git(repo_root, &["worktree", "remove", "--force", &dir_arg]).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    let _ = git(repo_root, &["worktree", "prune"]).await;
    let _ = tokio::fs::remove_dir_all(root).await;
}

/// Resolves the credential once (the same order as [`Engine::open`]) and
/// returns a [`ProviderFactory`] that builds one provider per rollout
/// from `config` — the production counterpart to the scripted test
/// factories. Catalog-driven adapters need `config.catalog` set first;
/// call [`Engine::resolve_catalog`] before this.
///
/// # Errors
///
/// Returns [`EngineError::Store`] when the account store cannot be opened
/// and [`EngineError::Auth`] when no credential resolves. Provider
/// construction failures surface later, when the returned factory is
/// invoked per rollout.
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
///
/// # Errors
///
/// Returns [`WorktreeError`] when the workspace is not a git repository,
/// a worktree cannot be created, or the shared-prefix seeding fails.
/// Worktrees created before the failure are cleaned up.
#[allow(clippy::too_many_lines)] // fixed setup→rollout→cleanup ordering; extraction would hide the sequencing
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
                "workspace {} is not inside a git repository; parallel rollouts need one HEAD to branch from",
                config.workspace.display()
            ))
        })?;
    let repo_root = PathBuf::from(repo_root);
    // Review item 2.8: worktree dirs (and their session databases) must
    // never ride into the operator's index as gitlinks.
    exclude_state_from_git(&repo_root, &[".loom/worktrees/"]);
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
                .map_or((true, u64::MAX, u32::MAX, u32::MAX), rank_key);
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
