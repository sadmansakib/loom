//! Run-scoped process plumbing: the event consumers (renderer and trace
//! writer threads) and the Ctrl-C stop ladders.

use std::future::Future;

use loom_core::engine::TaskControl;
use loom_core::kernel::event::{ChannelEventSink, FanoutSink};

use super::DecisionLog;
use crate::output::Renderer;
use loom_core::engine::trace::TraceWriter;

/// Spawns the run's event consumers (§11): the renderer always owns
/// stdout; with `--trace` a second consumer appends every envelope to
/// the task's `events.ndjson` and hands the writer back so the main
/// thread can finish the export once the outcome is known. A stalled
/// disk can delay the trace without ever delaying the terminal, and
/// vice versa.
pub(crate) fn spawn_consumers(
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
pub(crate) async fn ctrl_c_ladder(control: TaskControl) {
    if tokio::signal::ctrl_c().await.is_ok() {
        eprintln!("stopping after the current turn (Ctrl-C again to abort)…");
        control.pause();
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("aborting…");
            control.cancel();
        }
    }
}

/// The --best-of Ctrl-C ladder: the first press cancels the rollouts
/// so worktree cleanup runs, a second press aborts outright. The
/// handler must be installed (the future first polled) before the
/// rollouts start — tokio registers SIGINT lazily, so the old
/// token-then-ctrl_c ordering left the process on the default
/// disposition and one Ctrl-C killed it mid-rollout.
///
/// Parameterized on a signal-future factory so tests can drive it
/// with fakes; the real signal path is checked manually.
pub(super) async fn best_of_ctrl_c_ladder<S, F>(
    mut next_signal: F,
    token: tokio_util::sync::CancellationToken,
    abort: impl Fn(),
) where
    S: Future<Output = bool>,
    F: FnMut() -> S,
{
    if !next_signal().await {
        return;
    }
    eprintln!("cancelling rollouts (Ctrl-C again to abort)…");
    token.cancel();
    if next_signal().await {
        eprintln!("aborting…");
        abort();
    }
}

/// Closes the consumers' channels and collects their threads: dropping
/// the fan-out (and with it both channel senders) ends the
/// `blocking_recv` loops, and the trace writer — when one ran — comes
/// back for the export below.
pub(crate) fn join_consumers(
    // `sink` is moved in only to be dropped: closing the fanout's
    // sender side is the shutdown signal for the consumers.
    #[allow(clippy::needless_pass_by_value)] sink: FanoutSink,
    signal: &tokio::task::JoinHandle<()>,
    renderer_thread: std::thread::JoinHandle<()>,
    tracer_thread: Option<std::thread::JoinHandle<TraceWriter>>,
) -> Option<TraceWriter> {
    drop(sink);
    signal.abort();
    if renderer_thread.join().is_err() {
        eprintln!("warning: event renderer thread panicked");
    }
    tracer_thread.and_then(|thread| {
        if let Ok(writer) = thread.join() {
            Some(writer)
        } else {
            eprintln!("warning: trace consumer thread panicked; trace not written");
            None
        }
    })
}
