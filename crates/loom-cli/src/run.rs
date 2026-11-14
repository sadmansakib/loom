//! `loom run`: one task driven through the engine.
//!
//! This is the CLI adapter, not the composition root: the engine (ADR-021)
//! owns credentials, the provider, the tools, and session durability; this
//! module builds the process around it — tokio runtime, the two-stage
//! Ctrl-C stop (pause between turns, then abort), the stdout/stderr and
//! trace consumers, and the exit code.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use loom_core::engine::TaskControl;
use loom_core::kernel::agent::{TaskError, TaskSummary};
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore as _;
use loom_core::{Engine, EngineConfig, EngineOverrides, ScoutConfig};

mod consumers;

use consumers::best_of_ctrl_c_ladder;
pub(crate) use consumers::{ctrl_c_ladder, join_consumers, spawn_consumers};

use crate::args::RunArgs;
use crate::output::{self};
use loom_core::engine::trace::{Outcome, TraceMeta, TraceWriter};
use loom_core::kernel::event::{EventKind, EventSink};
use loom_core::kernel::id::TaskId;

/// Collects the surfaced-decision events (v8) so `loom run` can end
/// with a short "decisions" footer: which authority fixed the
/// capabilities, whether a fast path was tried and declined, and any
/// harness notice (sandbox passthrough, truncated streams). The
/// renderer keeps stdout; this footer prints to stderr after the task.
#[derive(Clone, Default)]
pub(crate) struct DecisionLog {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl EventSink for DecisionLog {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        // `EventKind` is `#[non_exhaustive]`; unknown variants render
        // no line.
        #[allow(clippy::wildcard_enum_match_arm)]
        let line = match kind {
            EventKind::CapabilitiesResolved { model, source } => {
                format!("model {model}: capabilities resolved from {source}")
            }
            EventKind::FastPathDeclined { reason } => {
                format!("fast path attempted and declined ({reason}); the agent loop took over")
            }
            EventKind::HarnessNotice { category, detail } => format!("[{category}] {detail}"),
            _ => return,
        };
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line);
        }
    }
}

/// Prints the footer: first occurrence of each decision line, in
/// emission order. Silence (no footer) is itself the signal — the
/// defaults held.
pub(crate) fn print_decisions(log: &DecisionLog) {
    let lines = match log.lines.lock() {
        Ok(lines) => lines.clone(),
        Err(_) => return,
    };
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&String> = lines.iter().filter(|line| seen.insert(*line)).collect();
    if unique.is_empty() {
        return;
    }
    eprintln!("decisions:");
    for line in unique {
        eprintln!("  - {line}");
    }
}

/// Runs one task end to end and returns the process exit code.
///
/// The parallel-rollout arm (roadmap item 21): N isolated worktrees,
/// one deterministic winner, its patch applied here. The catalog
/// sequencing and credential resolution are shared with the single
/// run — one fetch, one keychain read, N cheap provider rebuilds.
/// Returns `Some(exit)` when this arm handled the task; `None` falls
/// through to the single-run path.
fn run_best_of(
    args: &RunArgs,
    config: EngineConfig,
    requested: usize,
    runtime: tokio::runtime::Runtime,
    input: &str,
    bare_arm: bool,
) -> anyhow::Result<ExitCode> {
    if bare_arm {
        anyhow::bail!("--best-of cannot combine with the LOOM_BARE arm");
    }
    if args.flags.session.is_some() {
        anyhow::bail!("--best-of starts fresh parallel rollouts; --session continues one");
    }
    let n = requested.clamp(2, 8);
    let config = runtime.block_on(Engine::resolve_catalog(config))?;
    let factory = loom_core::engine::worktrees::provider_factory(&config)?;
    let token = tokio_util::sync::CancellationToken::new();
    // The ladder awaits its first ctrl_c() immediately, so the
    // handler is installed before the rollouts start: tokio
    // registers SIGINT lazily, and the old token-then-ctrl_c
    // ordering left Ctrl-C on the default disposition — one press
    // killed the process and orphaned the best-of-* worktrees.
    let ladder_token = token.clone();
    let signal = runtime.spawn(best_of_ctrl_c_ladder(
        || async { tokio::signal::ctrl_c().await.is_ok() },
        ladder_token,
        || std::process::exit(130),
    ));
    let report = runtime.block_on(loom_core::engine::worktrees::best_of_n(
        config,
        factory,
        input,
        loom_core::engine::worktrees::BestOfNConfig {
            n,
            concurrency: 3,
            keep_worktrees: false,
            shared_prefix_turns: args.flags.best_of_prefix_turns,
            vary_harness: args.flags.best_of_vary,
            verify_selection: !args.flags.best_of_no_verify,
        },
        &token,
    ))?;
    signal.abort();
    print_best_of_n(&report, args.flags.json);
    runtime.shutdown_timeout(Duration::from_secs(5));
    let cancelled = token.is_cancelled();
    Ok(if cancelled {
        // A user stop exits 130, matching the single-run path.
        ExitCode::from(130)
    } else if report.applied {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Terminal `TaskError`s are reported here and mapped to a non-zero code
/// (`130` for a user stop — cancellation or pause — `1` otherwise); only
/// setup failures propagate as `Err`.
pub fn run(args: &RunArgs) -> anyhow::Result<ExitCode> {
    if args.tui {
        validate_tui_run_args(args)?;
        return crate::tui_command::run_task(args).map(|()| ExitCode::SUCCESS);
    }
    if args.task.is_empty() {
        anyhow::bail!("`loom run` requires a TASK unless --tui is specified");
    }
    // The sandbox preflight fails at setup, not mid-task: on a host
    // whose policy denies unprivileged user namespaces (Ubuntu 24.04+
    // by default) every sandboxed `run_command`/`verify` would fail one
    // by one with an opaque `bwrap:` line. On macOS and pass-through
    // targets this is a no-op (loom_core::tools::sandbox::preflight).
    if std::env::var_os("LOOM_SKIP_SANDBOX_PREFLIGHT").is_none()
        && let Err(error) = loom_core::tools::sandbox::preflight()
    {
        anyhow::bail!(
            "sandbox preflight failed at setup:\n{error}\n\
             (Set LOOM_SKIP_SANDBOX_PREFLIGHT=1 to attempt the run anyway.)"
        );
    }

    // The runtime exists before the engine opens: the provider's requests
    // (and, for OpenRouter, the catalog fetch inside `open_async`) need a
    // reactor, and Ctrl-C arrives as a tokio signal.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;

    let trace_provider = args.flags.provider.clone();
    let trace_model = args.flags.model.clone();

    let (config, overrides) = engine_config_from(&args.flags)?;
    // The bare arm also gates --best-of below; recompute it here (env-
    // derived, so the two reads cannot disagree).
    let bare_arm = std::env::var_os("LOOM_BARE").is_some();

    let input = args.task.join(" ");

    // The parallel-rollout arm (roadmap item 21) gets its own function:
    // it resolves the catalog and never touches the single-run engine.
    if let Some(requested) = args.flags.best_of {
        return run_best_of(args, config, requested, runtime, &input, bare_arm);
    }

    let engine = runtime.block_on(Engine::open_async_with(config, overrides))?;

    let decision_log = DecisionLog::default();
    let (sink, renderer_thread, tracer_thread) = spawn_consumers(
        args.flags.json,
        args.flags.trace.clone(),
        decision_log.clone(),
    );

    // The provider's timeouts need the time driver the runtime enables.
    let control = TaskControl::new();
    let signal = runtime.spawn(ctrl_c_ladder(control.clone()));

    let (session, outcome) = if let Some(id) = args.flags.session.as_deref() {
        // Resume (item 15): with `--session` the engine loads the persisted
        // transcript, appends the input, and runs to the next stopping point;
        // `continue_session` returns the advanced session even when the task
        // failed, so the trace export still has a transcript to write.
        let id = id
            .parse()
            .context("parsing --session (expected an id from `loom sessions`)")?;
        let continuation = runtime
            .block_on(engine.continue_session(&id, input, &sink, &control))
            .map_err(|error| anyhow::Error::new(error).context(format!("resuming session {id}")))?;
        (continuation.session, continuation.outcome)
    } else {
        let mut session = engine.new_session();
        // Tier-1 start checkpoint (`docs/rewind-design.md` §4):
        // snapshot the workspace before the task can touch the
        // tree, so `loom rewind --restore-workspace --to-start`
        // has an exact restore point. Recorded only for a new
        // session — a `--session` resume keeps the baseline of the
        // run that actually started the lineage. Best-effort: a
        // run whose checkpoint fails to record still runs, and a
        // later restore refuses (no row) rather than guessing.
        crate::run::record_start_checkpoint(&session.id, &args.flags.workspace, args.flags.no_save);
        let outcome = runtime.block_on(engine.run_task(&mut session, input, &sink, &control));
        (session, outcome)
    };
    let tracer = join_consumers(sink, &signal, renderer_thread, tracer_thread);
    print_decisions(&decision_log);

    // The trace export is best-effort: a failure to write files must not
    // change the run's exit code, only warn.
    if let Some(writer) = tracer {
        finish_trace(
            writer,
            &outcome,
            &session,
            TraceMeta {
                provider: &trace_provider,
                model: &trace_model,
                reasoning_effort: args.flags.reasoning_effort,
                max_turns: args.flags.max_turns,
                persisted: !args.flags.no_save,
            },
        );
    }

    match outcome {
        Ok(summary) => {
            output::print_summary(&summary, "completed", &session.id, args.flags.json);
            runtime.shutdown_timeout(Duration::from_secs(5));
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            output::print_failure(&error, args.flags.json);
            runtime.shutdown_timeout(Duration::from_secs(5));
            Ok(if error.is_cancelled() || error.is_paused() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// Reject flags whose behavior the interactive frontend cannot represent;
/// silently dropping these would make `loom run --tui` differ from `loom run`.
fn validate_tui_run_args(args: &RunArgs) -> anyhow::Result<()> {
    let flags = &args.flags;
    let unsupported = [
        (flags.json, "--json"),
        (flags.trace.is_some(), "--trace"),
        (flags.session.is_some(), "--session"),
        (flags.best_of.is_some(), "--best-of"),
        (
            flags.best_of_prefix_turns.is_some(),
            "--best-of-prefix-turns",
        ),
        (flags.best_of_vary, "--best-of-vary"),
        (flags.best_of_no_verify, "--best-of-no-verify"),
        (
            args.task.iter().any(|word| word == "--"),
            "-- task separator",
        ),
    ];
    if let Some((_, flag)) = unsupported.into_iter().find(|(set, _)| *set) {
        anyhow::bail!("{flag} is not supported with --tui");
    }
    Ok(())
}

/// Renders the parallel-rollout report (roadmap item 21): one block per
/// rollout with its failure markers and spend footer, then the winner
/// and the merge note. `--json` emits one machine-readable object.
fn print_best_of_n(report: &loom_core::engine::worktrees::BestOfNReport, json: bool) {
    if json {
        let runs: Vec<_> = report
            .runs
            .iter()
            .map(|run| {
                serde_json::json!({
                    "index": run.index,
                    "end_reason": run.end_reason,
                    "markers": run.markers,
                    "patch_bytes": run.patch.len(),
                    "cost": run.cost_note,
                    "suite_passed": run.suite_passed,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "runs": runs,
                "winner": report.winner,
                "applied": report.applied,
                "apply_note": report.apply_note,
                "prefix": report.prefix_note,
                "harness": report.harness_note,
                "selection": report.selection_note,
            })
        );
        return;
    }
    if let Some(note) = &report.prefix_note {
        println!("shared prefix — {note}");
    }
    if let Some(note) = &report.harness_note {
        println!("harness variation — {note}");
    }
    if let Some(note) = &report.selection_note {
        println!("execution filter — {note}");
    }
    for run in &report.runs {
        println!(
            "rollout {}: end_reason={}, markers={}, patch {} B, suite {}",
            run.index,
            run.end_reason.as_deref().unwrap_or("-"),
            run.markers,
            run.patch.len(),
            match run.suite_passed {
                Some(true) => "pass",
                Some(false) => "FAIL",
                None => "-",
            }
        );
        if !run.cost_note.is_empty() {
            println!("  {}", run.cost_note);
        }
    }
    match report.winner {
        Some(_index) => println!("winner: {}", report.apply_note),
        None => println!("{}", report.apply_note),
    }
}

/// Assembles the engine configuration from the shared run flags,
/// used verbatim by `loom run` and `loom chat` so both frontends
/// configure the engine identically. Returns the overrides too — the
/// bare-loop arm (`LOOM_BARE`) changes both, and callers need it for
/// `Engine::open_async_with`.
/// One bare-arm gate: the feature is on unless `LOOM_BARE` strips it
/// (the bare arm measures the plain loop); an ignored flag prints a
/// note instead of failing.
fn bare_gated(on: bool, flag: &str, bare_arm: bool) -> bool {
    if on && !bare_arm {
        return true;
    }
    if on {
        eprintln!("note: --{flag} is ignored in the bare-loop arm");
    }
    false
}

/// The scout arm (roadmap item 19): off unless both flags are given;
/// the bare arm never carries it (it measures the plain loop).
fn scout_config(
    flags: &crate::args::RunFlags,
    bare_arm: bool,
) -> anyhow::Result<Option<ScoutConfig>> {
    let escalation = match (
        flags.scout_escalation.as_deref(),
        flags.scout_escalation_model.as_deref(),
    ) {
        (Some(_), None) => anyhow::bail!("--scout-escalation requires --scout-escalation-model"),
        (None, Some(_)) => anyhow::bail!("--scout-escalation-model requires --scout-escalation"),
        (Some(provider), Some(model)) => {
            if flags.scout.is_none() || flags.scout_model.is_none() {
                anyhow::bail!("--scout-escalation requires --scout and --scout-model");
            }
            Some((provider.to_owned(), model.to_owned()))
        }
        (None, None) => None,
    };
    let scout = match (flags.scout.as_deref(), flags.scout_model.as_deref()) {
        (Some(provider), Some(model)) => {
            if bare_arm {
                eprintln!("note: --scout is ignored in the bare-loop arm");
                None
            } else {
                let mut config = ScoutConfig::new(provider.to_owned(), model.to_owned());
                if let Some((esc_provider, esc_model)) = escalation {
                    config = config.with_escalation(esc_provider, esc_model);
                }
                Some(config)
            }
        }
        (Some(_), None) => anyhow::bail!("--scout requires --scout-model"),
        (None, Some(_)) => anyhow::bail!("--scout-model requires --scout"),
        (None, None) => None,
    };
    Ok(scout)
}

/// Item 20.2/26: per-tool budgets, pruning policy, and the staged turn
/// budget are sweepable via env (`LOOM_*_BYTES`/`_LINES`,
/// `LOOM_PRUNE_*`, `LOOM_*_TURNS`), layered over the defaults. Presence
/// echoes (honesty: the run log records what varied); invalid values
/// warn and are ignored.
fn env_budgets(
    provider: &str,
) -> (
    loom_core::tools::ToolBudgets,
    loom_core::kernel::context::PruneConfig,
    loom_core::kernel::agent::TurnBudget,
) {
    for name in loom_core::engine::BUDGET_ENV_VARS
        .into_iter()
        .chain(loom_core::engine::PRUNE_ENV_VARS)
    {
        if let Ok(value) = std::env::var(name) {
            eprintln!("override: {name}={value}");
        }
    }
    let (budgets, budget_warnings) =
        loom_core::engine::budgets_with_env(loom_core::tools::ToolBudgets::default(), |name| {
            std::env::var(name).ok()
        });
    for warning in &budget_warnings {
        eprintln!("warning: {warning}");
    }
    for name in loom_core::engine::TURN_BUDGET_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            eprintln!("override: {name}={value}");
        }
    }
    let (prune, prune_warnings) =
        loom_core::engine::prune_with_env(provider, |name| std::env::var(name).ok());
    for warning in &prune_warnings {
        eprintln!("warning: {warning}");
    }
    let (turn_budget, turn_budget_warnings) = loom_core::engine::turn_budget_with_env(
        loom_core::kernel::agent::TurnBudget::default(),
        |name| std::env::var(name).ok(),
    );
    for warning in &turn_budget_warnings {
        eprintln!("warning: {warning}");
    }
    (budgets, prune, turn_budget)
}

pub(crate) fn engine_config_from(
    flags: &crate::args::RunFlags,
) -> anyhow::Result<(EngineConfig, EngineOverrides)> {
    let db_path = loom_core::paths::db_path();

    // Global instructions (the user-level AGENTS.md) compose once, here:
    // the CLI is the frontend that reads configuration (ADR-008) — the
    // engine itself never consults the environment, so the eval harness
    // and embedders are unaffected by whatever this machine's global
    // file says. An explicit `--system-prompt` replaces the built-in
    // instruction, and the user's standing global file still appends —
    // it exists because the user asked for it; `--no-global-instructions`
    // is the escape hatch. Composing before the engine opens keeps the
    // prompt byte-stable for the whole session (§8.1.1).
    let global_instructions_path = loom_core::paths::global_instructions_path();
    let system_prompt = if flags.no_global_instructions {
        flags.system_prompt.clone()
    } else {
        let global = crate::instructions::load_global(&global_instructions_path);
        Some(crate::instructions::merge(
            flags
                .system_prompt
                .clone()
                .unwrap_or_else(loom_core::engine::default_system_prompt),
            global.map(|body| (body, global_instructions_path)),
        ))
    };

    // The bare-loop arm (roadmap item 1): the eval harness sets
    // LOOM_BARE to measure the engine's levers against the plain
    // five-tool loop. Read here, not in the engine (ADR-008): embedders
    // assemble their own overrides, and a stray env var on a user
    // machine must not silently strip their harness.
    let bare_arm = std::env::var_os("LOOM_BARE").is_some();
    if bare_arm {
        eprintln!(
            "bare-loop arm active (LOOM_BARE): facts, pruning, completion \
             gate, failure ledger, and fast paths are off"
        );
    }

    let overrides = if bare_arm {
        EngineOverrides::default().bare_loop()
    } else {
        EngineOverrides::default()
    };
    let scout = scout_config(flags, bare_arm)?;
    // Feature seams share one bare-arm gate: the bare arm measures the
    // plain loop, so each feature is off there (flag asked for and
    // ignored prints a note).
    let symbols = bare_gated(flags.symbols, "symbols", bare_arm);
    let skills = bare_gated(flags.skills, "skills", bare_arm);
    let lessons = bare_gated(flags.lessons, "lessons", bare_arm);
    let mcp = bare_gated(flags.mcp, "mcp", bare_arm);
    let lsp = bare_gated(flags.lsp, "lsp", bare_arm);
    let git = bare_gated(flags.git, "git", bare_arm);
    let (budgets, prune, turn_budget) = env_budgets(&flags.provider);
    // Header pairs are validated once, here; the engine carries them
    // verbatim to the custom adapter.
    let headers = crate::args::parse_header_pairs(&flags.headers).map_err(anyhow::Error::msg)?;
    // Routing slice zero (item 9): tier wiring from --route-easy /
    // --route-hard (or their env fallbacks). Absent both, every task
    // runs on the run's own model — the byte-off default.
    let routing = match (&flags.route_easy, &flags.route_hard) {
        (None, None) => None,
        (easy, hard) => Some(loom_core::engine::routing::RoutingConfig {
            easy: easy.clone(),
            hard: hard.clone(),
        }),
    };
    let config = EngineConfig {
        provider: flags.provider.clone(),
        model: flags.model.clone(),
        base_url: flags.base_url.clone(),
        models_url: flags.models_url.clone(),
        headers,
        routing,
        stall_escalation_model: flags.stall_escalation_model.clone(),
        background_commands: flags.bg_commands,
        reasoning_effort: flags.reasoning_effort,
        account: flags.account.clone(),
        workspace: flags.workspace.clone(),
        system_prompt,
        max_turns: flags.max_turns,
        turn_budget,
        // Roadmap item 12: off on every CLI surface — server-side
        // context editing is a frontend-level opt-in, and the eval
        // protocol stays frozen.
        context_editing: None,
        db_path,
        persist: !flags.no_save,
        catalog: None,
        scout,
        symbols,
        git,
        skills,
        lessons,
        mcp,
        lsp,
        budgets,
        prune: Some(prune),
        autofix: flags.autofix,
    };
    Ok((config, overrides))
}

/// Records the tier-1 start checkpoint (`docs/rewind-design.md` §4) for
/// a fresh session: snapshots the workspace before the task can touch
/// the tree, so `loom rewind --restore-workspace --to-start` has an
/// exact restore point. Shared by `loom run` and `loom chat`. Best
/// effort: a run whose checkpoint fails to record still runs, and a
/// later restore refuses (no row) rather than guessing.
pub(crate) fn record_start_checkpoint(
    session_id: &loom_core::kernel::id::SessionId,
    workspace: &std::path::Path,
    no_save: bool,
) {
    if no_save {
        return;
    }
    let checkpoint = crate::workspace::snapshot_start(workspace);
    if let Some(reason) = &checkpoint.reason {
        eprintln!("note: no workspace start checkpoint ({reason})");
    }
    match loom_core::store::SqliteStore::open(loom_core::paths::db_path()) {
        Ok(store) => {
            // The engine creates the session row lazily in run_task;
            // the checkpoint's row references it, so ensure it first
            // (INSERT OR IGNORE — the engine's own ensure is idempotent).
            if let Err(error) = store
                .ensure_session(session_id)
                .and_then(|()| store.record_start_checkpoint(session_id, &checkpoint))
            {
                eprintln!("warning: recording start checkpoint: {error}");
            }
        }
        Err(error) => {
            eprintln!("warning: opening store for start checkpoint: {error}");
        }
    }
}

/// Finishes the trace export: maps the task outcome to the manifest's
/// terminal shape (completed / cancelled / paused / failed) and writes
/// the manifest and transcript beside the events. Best-effort by
/// contract — a failure warns and never changes the exit code. The
/// run-configuration fields arrive as a [`TraceMeta`] assembled where
/// the config consumed their originals.
fn finish_trace(
    writer: TraceWriter,
    outcome: &Result<TaskSummary, TaskError>,
    session: &Session,
    meta: TraceMeta<'_>,
) {
    let (trace_outcome, summary) = match outcome {
        Ok(summary) => (Outcome::Completed, Some(summary)),
        Err(error) => {
            let reason = error.end_reason().to_string();
            (
                Outcome::Ended {
                    status: if error.is_cancelled() {
                        "cancelled"
                    } else if error.is_paused() {
                        "paused"
                    } else {
                        "failed"
                    },
                    end_reason: reason,
                },
                None,
            )
        }
    };
    match writer.finish(session, meta, trace_outcome, summary) {
        Ok(Some(dir)) => eprintln!("trace: {}", dir.display()),
        Ok(None) => {}
        Err(error) => eprintln!("warning: trace export failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::consumers::best_of_ctrl_c_ladder;
    use super::{run, validate_tui_run_args};
    use crate::args::Cli;
    use clap::Parser;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn tui_run_rejects_options_the_frontend_cannot_honor() {
        let cli = Cli::try_parse_from([
            "loom",
            "run",
            "--tui",
            "--provider",
            "glm",
            "--model",
            "glm-5",
            "--trace",
            "x",
        ])
        .expect("arguments parse");
        let crate::args::Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(
            validate_tui_run_args(&args)
                .unwrap_err()
                .to_string()
                .contains("--trace")
        );
    }

    #[test]
    fn run_requires_task_unless_tui_is_enabled() {
        let cli = Cli::try_parse_from(["loom", "run", "--provider", "glm", "--model", "glm-5"])
            .expect("provider and model are present");
        let crate::args::Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        let error = run(&args).expect_err("missing task must fail in dispatch");
        assert!(error.to_string().contains("requires a TASK"));
    }

    /// A fake signal factory standing in for `tokio::signal::ctrl_c`:
    /// each produced future awaits one channel item — `true` is a
    /// pressed Ctrl-C, a closed channel is the real handler's Err arm
    /// (no signal, ladder ends).
    fn fake_signals(
        presses: tokio::sync::mpsc::Receiver<bool>,
    ) -> impl FnMut() -> Pin<Box<dyn Future<Output = bool> + Send>> {
        let presses = Arc::new(tokio::sync::Mutex::new(presses));
        move || {
            let presses = presses.clone();
            Box::pin(async move {
                let mut rx = presses.lock().await;
                rx.recv().await.unwrap_or(false)
            })
        }
    }

    #[tokio::test]
    async fn first_signal_cancels_and_second_aborts() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let token = CancellationToken::new();
        let aborted = Arc::new(AtomicBool::new(false));
        let flag = aborted.clone();
        let ladder = tokio::spawn(best_of_ctrl_c_ladder(
            fake_signals(rx),
            token.clone(),
            move || flag.store(true, Ordering::SeqCst),
        ));
        assert!(
            !token.is_cancelled() && !aborted.load(Ordering::SeqCst),
            "an idle ladder must not fire"
        );
        tx.send(true).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(200), token.cancelled()).await;
        assert!(token.is_cancelled(), "first press must cancel the rollouts");
        assert!(
            !aborted.load(Ordering::SeqCst),
            "cancel alone must not abort"
        );
        tx.send(true).await.unwrap();
        let _ = ladder.await;
        assert!(aborted.load(Ordering::SeqCst), "second press must abort");
    }

    #[tokio::test]
    async fn one_press_cancels_without_aborting() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let token = CancellationToken::new();
        let aborted = Arc::new(AtomicBool::new(false));
        let flag = aborted.clone();
        // Press once, then end the stream: the second wait sees the
        // Err arm and the ladder completes without aborting.
        tx.send(true).await.unwrap();
        drop(tx);
        best_of_ctrl_c_ladder(fake_signals(rx), token.clone(), move || {
            flag.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(token.is_cancelled());
        assert!(!aborted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_errored_first_signal_is_a_noop() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        let token = CancellationToken::new();
        best_of_ctrl_c_ladder(fake_signals(rx), token.clone(), || panic!("no abort")).await;
        assert!(!token.is_cancelled(), "signal error must not cancel");
    }
}
