//! `loom run`: credential resolution, composition, and one task.
//!
//! This is the composition root (architecture §2): it is the only place that
//! knows about a concrete provider, the concrete tools, the credential seams,
//! and the runtime. Everything below it is wired through the `loom-core`
//! traits. Credentials are resolved *before* the provider is built, and the
//! secret never leaves [`loom_providers::SecretString`] wrappers (§18.3).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use loom_core::agent::Agent;
use loom_core::context::{AgentConfig, DefaultContextCompiler};
use loom_core::event::ChannelEventSink;
use loom_core::id::SessionId;
use loom_core::model::ModelProvider;
use loom_core::session::Session;
use loom_core::tool::ToolRegistry;
use loom_providers::{DeepSeekConfig, DeepSeekProvider};
use loom_tools::{
    EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool, WriteFileTool,
};
use tokio_util::sync::CancellationToken;

use crate::args::RunArgs;
use crate::auth::{self, KeychainBackend, SqliteAuthStore};
use crate::output::{self, Renderer};
use crate::paths;

/// Runs one task end to end and returns the process exit code.
///
/// Terminal `TaskError`s are reported here and mapped to a non-zero code
/// (`130` for cancellation, `1` otherwise); only setup failures propagate as
/// `Err`.
pub fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    if args.provider != "deepseek" {
        bail!(
            "unknown provider `{}`: iteration 1 ships `deepseek`",
            args.provider
        );
    }

    let workspace = std::fs::canonicalize(&args.workspace).with_context(|| {
        format!(
            "workspace `{}` does not exist or is not readable",
            args.workspace.display()
        )
    })?;

    // Credential resolution: --account → active saved account → env (§18.3).
    let store = SqliteAuthStore::open(&paths::db_path()).context("opening the account store")?;
    let secrets = KeychainBackend::default();
    let credential = auth::resolve(
        &store,
        &secrets,
        &args.provider,
        args.account.as_deref(),
        |name| std::env::var(name).ok(),
    )?;
    tracing::debug!(
        source = credential.source.as_str(),
        account = credential
            .account
            .as_ref()
            .map(|account| account.label.as_str())
            .unwrap_or("-"),
        "credential resolved"
    );

    // Provider construction and loud capability validation (§6.4).
    let mut config = DeepSeekConfig::new(credential.secret.expose().to_owned())
        .with_reasoning_effort(args.reasoning_effort);
    if let Some(base_url) = &args.base_url {
        config = config.with_base_url(base_url.clone());
    }
    let provider = DeepSeekProvider::new(config).context("building the DeepSeek provider")?;
    let capabilities = provider.capabilities(&args.model).ok_or_else(|| {
        anyhow::anyhow!(
            "model `{}` is unknown to provider `{}`: check --model",
            args.model,
            args.provider
        )
    })?;
    if !capabilities.reasoning {
        bail!(
            "model `{}` does not support reasoning, but a reasoning effort is always sent; \
             choose a reasoning model",
            args.model
        );
    }

    // Built-in tools (§9). Registration order is the exposure order: the
    // read-only tools first, so the prompt prefix stays stable.
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(ReadFileTool))
        .context("registering read_file")?;
    registry
        .register(Box::new(ListDirTool))
        .context("registering list_dir")?;
    registry
        .register(Box::new(SearchTool))
        .context("registering search")?;
    registry
        .register(Box::new(WriteFileTool))
        .context("registering write_file")?;
    registry
        .register(Box::new(EditFileTool))
        .context("registering edit_file")?;
    registry
        .register(Box::new(RunCommandTool::new()))
        .context("registering run_command")?;

    let system_prompt = args.system_prompt.unwrap_or_else(default_system_prompt);
    let agent = Agent::new(
        Box::new(provider),
        Box::new(DefaultContextCompiler),
        registry,
        AgentConfig::new(workspace, system_prompt),
        args.model.clone(),
    )
    .with_max_turns(args.max_turns);

    let mut session = Session::new(SessionId::new());
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

    // An explicit runtime with a bounded shutdown (§6.6): the provider's
    // timeouts need the time driver, and Ctrl-C must cancel the task token.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;
    let cancellation = CancellationToken::new();
    let signal_token = cancellation.clone();
    let signal = runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_token.cancel();
        }
    });

    let outcome = runtime.block_on(agent.run_task(&mut session, input, &sink, &cancellation));
    drop(sink);
    signal.abort();
    let _ = consumer.join();

    let session_path = save_session(&session, args.no_save)?;
    match outcome {
        Ok(summary) => {
            output::print_summary(&summary, "completed", session_path.as_deref(), args.json);
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

/// Writes the session as a single JSON file, unless saving is disabled.
fn save_session(session: &Session, no_save: bool) -> anyhow::Result<Option<PathBuf>> {
    if no_save {
        return Ok(None);
    }
    let dir = paths::sessions_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the session directory `{}`", dir.display()))?;
    let path = dir.join(format!("{}.json", session.id));
    let bytes = serde_json::to_vec_pretty(session).context("serialising the session")?;
    std::fs::write(&path, bytes)
        .with_context(|| format!("writing the session to `{}`", path.display()))?;
    Ok(Some(path))
}

/// The built-in instruction; overridable with `--system-prompt`.
fn default_system_prompt() -> String {
    String::from(
        "You are Loom, a coding agent working inside one workspace. Use the provided \
         tools to inspect and change the workspace: list and search before reading, \
         read before editing, write or edit files to make changes, and run allow-listed \
         commands (cargo) to verify. Prefer the smallest change that works. When you \
         answer, be concise and name the workspace-relative paths you relied on.",
    )
}
