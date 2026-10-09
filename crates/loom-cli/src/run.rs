//! `loom run`: one task driven through the engine.
//!
//! This is the CLI adapter, not the composition root: the engine (ADR-021)
//! owns credentials, the provider, the tools, and session durability; this
//! module builds the process around it — tokio runtime, Ctrl-C cancellation,
//! the stdout/stderr consumer thread, and the exit code.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use loom_core::kernel::event::ChannelEventSink;
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

    // The consumer owns stdout/stderr; the sink only numbers and sends (§11).
    // With `--trace` it also appends every envelope to the task's
    // `events.ndjson` and hands the writer back so the main thread can
    // finish the export once the outcome is known.
    let (sink, mut receiver) = ChannelEventSink::new();
    let json = args.json;
    let tracer = args.trace.clone().map(TraceWriter::new);
    let consumer = std::thread::spawn(move || {
        let mut renderer = Renderer::new(json);
        let mut tracer = tracer;
        while let Some(envelope) = receiver.blocking_recv() {
            if let Some(writer) = tracer.as_mut() {
                writer.record(&envelope);
            }
            renderer.render(&envelope);
        }
        renderer.finish();
        tracer
    });

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
    drop(sink);
    signal.abort();
    let tracer = consumer.join().unwrap_or_else(|_| {
        eprintln!("warning: event consumer thread panicked; trace not written");
        None
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
