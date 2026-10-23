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
use loom_core::kernel::event::{ChannelEventSink, FanoutSink};
use loom_core::kernel::session::Session;
use loom_core::{Engine, EngineConfig, EngineOverrides};

use crate::args::RunArgs;
use crate::output::{self, Renderer};
use loom_core::engine::trace::{Outcome, TraceMeta, TraceWriter};
use loom_core::kernel::event::{EventKind, EventSink};
use loom_core::kernel::id::TaskId;

/// Collects the surfaced-decision events (v8) so `loom run` can end
/// with a short "decisions" footer: which authority fixed the
/// capabilities, whether a fast path was tried and declined, and any
/// harness notice (sandbox passthrough, truncated streams). The
/// renderer keeps stdout; this footer prints to stderr after the task.
#[derive(Clone, Default)]
struct DecisionLog {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl EventSink for DecisionLog {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
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
fn print_decisions(log: &DecisionLog) {
    let lines = match log.lines.lock() {
        Ok(lines) => lines.clone(),
        Err(_) => return,
    };
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&String> = lines
        .iter()
        .filter(|line| seen.insert((*line).clone()))
        .collect();
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
/// Terminal `TaskError`s are reported here and mapped to a non-zero code
/// (`130` for a user stop — cancellation or pause — `1` otherwise); only
/// setup failures propagate as `Err`.
pub fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
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
    let system_prompt = if args.no_global_instructions {
        args.system_prompt
    } else {
        let global = crate::instructions::load_global(&global_instructions_path);
        Some(crate::instructions::merge(
            args.system_prompt
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

    // Captured before `EngineConfig` consumes the strings: the trace
    // manifest needs the run configuration after the task ended.
    let trace_provider = args.provider.clone();
    let trace_model = args.model.clone();

    // `open_async` owns the catalog sequencing: fetch, cache beside the
    // database, fall back to a stale cache — identical for every frontend.
    let overrides = if bare_arm {
        EngineOverrides::default().bare_loop()
    } else {
        EngineOverrides::default()
    };
    let engine = runtime.block_on(Engine::open_async_with(
        EngineConfig {
            provider: args.provider,
            model: args.model,
            base_url: args.base_url,
            models_url: args.models_url,
            reasoning_effort: args.reasoning_effort,
            account: args.account,
            workspace: args.workspace,
            system_prompt,
            max_turns: args.max_turns,
            db_path,
            persist: !args.no_save,
            catalog: None,
        },
        overrides,
    ))?;

    let input = args.task.join(" ");

    let decision_log = DecisionLog::default();
    let (sink, renderer_thread, tracer_thread) =
        spawn_consumers(args.json, args.trace.clone(), decision_log.clone());

    // The provider's timeouts need the time driver the runtime enables.
    let control = TaskControl::new();
    let signal = runtime.spawn(ctrl_c_ladder(control.clone()));

    // Resume (item 15): with `--session` the engine loads the persisted
    // transcript, appends the input, and runs to the next stopping point;
    // `continue_session` returns the advanced session even when the task
    // failed, so the trace export still has a transcript to write.
    let (session, outcome) = match args.session.as_deref() {
        Some(id) => {
            let id = id
                .parse()
                .context("parsing --session (expected an id from `loom sessions`)")?;
            let continuation = runtime
                .block_on(engine.continue_session(&id, input, &sink, &control))
                .map_err(|error| {
                    anyhow::Error::new(error).context(format!("resuming session {id}"))
                })?;
            (continuation.session, continuation.outcome)
        }
        None => {
            let mut session = engine.new_session();
            let outcome = runtime.block_on(engine.run_task(&mut session, input, &sink, &control));
            (session, outcome)
        }
    };
    let tracer = join_consumers(sink, signal, renderer_thread, tracer_thread);
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
                reasoning_effort: args.reasoning_effort,
                max_turns: args.max_turns,
                persisted: !args.no_save,
            },
        );
    }

    match outcome {
        Ok(summary) => {
            output::print_summary(&summary, "completed", &session.id, args.json);
            runtime.shutdown_timeout(Duration::from_secs(5));
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            output::print_failure(&error, args.json);
            runtime.shutdown_timeout(Duration::from_secs(5));
            Ok(if error.is_cancelled() || error.is_paused() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// Spawns the run's event consumers (§11): the renderer always owns
/// stdout; with `--trace` a second consumer appends every envelope to
/// the task's `events.ndjson` and hands the writer back so the main
/// thread can finish the export once the outcome is known. A stalled
/// disk can delay the trace without ever delaying the terminal, and
/// vice versa.
fn spawn_consumers(
    json: bool,
    trace: Option<std::path::PathBuf>,
    decision_log: DecisionLog,
) -> (
    FanoutSink,
    std::thread::JoinHandle<()>,
    Option<std::thread::JoinHandle<TraceWriter>>,
) {
    let (render_sink, mut render_receiver) = ChannelEventSink::new();
    let renderer_thread = std::thread::spawn(move || {
        let mut renderer = Renderer::new(json);
        while let Some(envelope) = render_receiver.blocking_recv() {
            renderer.render(&envelope);
        }
        renderer.finish();
    });

    let (trace_sink, tracer_thread) = match trace {
        Some(dir) => {
            let (trace_sink, mut trace_receiver) = ChannelEventSink::new();
            let mut writer = TraceWriter::new(dir);
            let thread = std::thread::spawn(move || {
                while let Some(envelope) = trace_receiver.blocking_recv() {
                    writer.record(&envelope);
                }
                writer
            });
            (Some(trace_sink), Some(thread))
        }
        None => (None, None),
    };
    let sink = match trace_sink {
        Some(trace_sink) => FanoutSink::new()
            .with(render_sink)
            .with(trace_sink)
            .with(decision_log),
        None => FanoutSink::new().with(render_sink).with(decision_log),
    };
    (sink, renderer_thread, tracer_thread)
}

/// The two-stage Ctrl-C ladder: the first press pauses — the in-flight
/// turn finishes and commits, then the loop stops — and a second press
/// aborts mid-flight. Either way the session survives to be resumed.
async fn ctrl_c_ladder(control: TaskControl) {
    if tokio::signal::ctrl_c().await.is_ok() {
        eprintln!("stopping after the current turn (Ctrl-C again to abort)…");
        control.pause();
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("aborting…");
            control.cancel();
        }
    }
}

/// Closes the consumers' channels and collects their threads: dropping
/// the fan-out (and with it both channel senders) ends the
/// `blocking_recv` loops, and the trace writer — when one ran — comes
/// back for the export below.
fn join_consumers(
    sink: FanoutSink,
    signal: tokio::task::JoinHandle<()>,
    renderer_thread: std::thread::JoinHandle<()>,
    tracer_thread: Option<std::thread::JoinHandle<TraceWriter>>,
) -> Option<TraceWriter> {
    drop(sink);
    signal.abort();
    if renderer_thread.join().is_err() {
        eprintln!("warning: event renderer thread panicked");
    }
    tracer_thread.and_then(|thread| match thread.join() {
        Ok(writer) => Some(writer),
        Err(_) => {
            eprintln!("warning: trace consumer thread panicked; trace not written");
            None
        }
    })
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
