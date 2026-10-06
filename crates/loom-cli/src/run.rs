//! `loom run`: one task driven through the engine.
//!
//! This is the CLI adapter, not the composition root: the engine (ADR-021)
//! owns credentials, the provider, the tools, and session durability; this
//! module builds the process around it — tokio runtime, Ctrl-C cancellation,
//! the stdout/stderr consumer thread, and the exit code.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use loom_core::engine::auth::Auth;
use loom_core::kernel::event::ChannelEventSink;
use loom_core::providers::{OpenRouterCatalog, OpenRouterProvider};
use loom_core::{Engine, EngineConfig};
use tokio_util::sync::CancellationToken;

use crate::args::RunArgs;
use crate::output::{self, Renderer};
use crate::paths;

/// Runs one task end to end and returns the process exit code.
///
/// Terminal `TaskError`s are reported here and mapped to a non-zero code
/// (`130` for cancellation, `1` otherwise); only setup failures propagate as
/// `Err`.
pub fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    // The runtime exists before the engine opens: fetching the OpenRouter
    // catalog is asynchronous and must complete before `Engine::open`, which
    // resolves capabilities from it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;

    let db_path = paths::db_path();
    let openrouter_catalog = if args.provider == OpenRouterProvider::PROVIDER_NAME {
        Some(load_openrouter_catalog(&runtime, &args, &db_path)?)
    } else {
        None
    };

    let engine = Engine::open(EngineConfig {
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
        openrouter_catalog,
    })?;

    let mut session = engine.new_session();
    let input = args.task.join(" ");

    // The consumer owns stdout/stderr; the sink only numbers and sends (§11).
    let (sink, mut receiver) = ChannelEventSink::new();
    let json = args.json;
    let consumer = std::thread::spawn(move || {
        let mut renderer = Renderer::new(json);
        while let Some(envelope) = receiver.blocking_recv() {
            renderer.render(&envelope);
        }
        renderer.finish();
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
    let _ = consumer.join();

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

/// Obtains the OpenRouter catalog before the engine opens.
///
/// The engine resolves capabilities from the catalog, so a missing one is a
/// setup failure, not a runtime one. Credential resolution goes through the
/// engine's account store, so `--account`, saved accounts, and the
/// environment behave exactly as they will for the provider itself.
fn load_openrouter_catalog(
    runtime: &tokio::runtime::Runtime,
    args: &RunArgs,
    db_path: &std::path::Path,
) -> anyhow::Result<OpenRouterCatalog> {
    let auth = Auth::open(db_path).context("opening the account store")?;
    runtime.block_on(crate::catalog::load(
        &auth,
        &args.provider,
        args.account.as_deref(),
        args.base_url.as_deref(),
        db_path,
    ))
}
