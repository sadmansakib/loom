//! `loom run`: one task driven through the engine.
//!
//! This is the CLI adapter, not the composition root: the engine (ADR-021)
//! owns credentials, the provider, the tools, and session durability; this
//! module builds the process around it — tokio runtime, Ctrl-C cancellation,
//! the stdout/stderr and trace consumers, and the exit code.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use loom_core::kernel::event::{ChannelEventSink, FanoutSink};
use loom_core::{Engine, EngineConfig};
use tokio_util::sync::CancellationToken;

use crate::args::RunArgs;
use crate::output::{self, Renderer};
use loom_core::engine::trace::{Outcome, TraceMeta, TraceWriter};

/// Runs one task end to end and returns the process exit code.
///
/// Terminal `TaskError`s are reported here and mapped to a non-zero code
/// (`130` for cancellation, `1` otherwise); only setup failures propagate as
/// `Err`.
pub fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    // The runtime exists before the engine opens: the provider's requests
    // (and, for OpenRouter, the catalog fetch inside `open_async`) need a
    // reactor, and Ctrl-C arrives as a tokio signal.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;

    let db_path = loom_core::paths::db_path();

    // Captured before `EngineConfig` consumes the strings: the trace
    // manifest needs the run configuration after the task ended.
    let trace_provider = args.provider.clone();
    let trace_model = args.model.clone();

    // `open_async` owns the catalog sequencing: fetch, cache beside the
    // database, fall back to a stale cache — identical for every frontend.
    let engine = runtime.block_on(Engine::open_async(EngineConfig {
        provider: args.provider,
        model: args.model,
        base_url: args.base_url,
        reasoning_effort: args.reasoning_effort,
        account: args.account,
        workspace: args.workspace,
        system_prompt: args.system_prompt,
        max_turns: args.max_turns,
        db_path,
        persist: !args.no_save,
        catalog: None,
    }))?;

    let mut session = engine.new_session();
    let input = args.task.join(" ");

    // Two independent consumers fan out from one sink (§11): the
    // renderer always owns stdout; with `--trace` a second consumer
    // appends every envelope to the task's `events.ndjson` and hands
    // the writer back so the main thread can finish the export once
    // the outcome is known. A stalled disk can delay the trace without
    // ever delaying the terminal, and vice versa.
    let (render_sink, mut render_receiver) = ChannelEventSink::new();
    let json = args.json;
    let renderer_thread = std::thread::spawn(move || {
        let mut renderer = Renderer::new(json);
        while let Some(envelope) = render_receiver.blocking_recv() {
            renderer.render(&envelope);
        }
        renderer.finish();
    });

    let (trace_sink, tracer_thread) = match args.trace.clone() {
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
        Some(trace_sink) => FanoutSink::new().with(render_sink).with(trace_sink),
        None => FanoutSink::new().with(render_sink),
    };

    // Ctrl-C cancels the task token; the provider's timeouts need the time
    // driver the runtime already enables.
    let cancellation = CancellationToken::new();
    let signal_token = cancellation.clone();
    let signal = runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_token.cancel();
        }
    });

    let outcome = runtime.block_on(engine.run_task(&mut session, input, &sink, &cancellation));
    // Dropping the fan-out (and with it both channel senders) closes
    // the consumers' `blocking_recv` loops.
    drop(sink);
    signal.abort();
    if renderer_thread.join().is_err() {
        eprintln!("warning: event renderer thread panicked");
    }
    let tracer = tracer_thread.and_then(|thread| match thread.join() {
        Ok(writer) => Some(writer),
        Err(_) => {
            eprintln!("warning: trace consumer thread panicked; trace not written");
            None
        }
    });

    // The trace export is best-effort: a failure to write files must not
    // change the run's exit code, only warn.
    if let Some(writer) = tracer {
        let (trace_outcome, summary) = match &outcome {
            Ok(summary) => (Outcome::Completed, Some(summary)),
            Err(error) => {
                let reason = error.end_reason().to_string();
                (
                    Outcome::Ended {
                        status: if error.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        },
                        end_reason: reason,
                    },
                    None,
                )
            }
        };
        match writer.finish(
            &session,
            TraceMeta {
                provider: &trace_provider,
                model: &trace_model,
                reasoning_effort: args.reasoning_effort,
                max_turns: args.max_turns,
                persisted: !args.no_save,
            },
            trace_outcome,
            summary,
        ) {
            Ok(Some(dir)) => eprintln!("trace: {}", dir.display()),
            Ok(None) => {}
            Err(error) => eprintln!("warning: trace export failed: {error}"),
        }
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
            Ok(if error.is_cancelled() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            })
        }
    }
}
