//! The driving API: open an engine, run one task, persist the session.
//!
//! [`Engine::open`] resolves credentials, builds the concrete provider, and
//! assembles the built-in tools into one [`Agent`]; the frontend then calls
//! [`Engine::run_task`] with an in-process event sink and a cancellation
//! token. The event protocol never crosses a wire: the sink is a Rust trait,
//! so local frontends pay no serialization cost (ADR-021).

use std::path::PathBuf;

use loom_kernel::agent::{Agent, TaskError, TaskSummary};
use loom_kernel::context::{AgentConfig, DefaultContextCompiler};
use loom_kernel::event::EventSink;
use loom_kernel::id::SessionId;
use loom_kernel::model::ModelProvider;
use loom_kernel::session::Session;
use loom_kernel::tool::ToolRegistry;
use loom_providers::{DeepSeekConfig, DeepSeekProvider, ReasoningEffort};
use loom_tools::{
    EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool, WriteFileTool,
};
use tokio_util::sync::CancellationToken;

use crate::auth::{self, KeychainBackend, SqliteAuthStore};
use crate::paths;

/// Everything a frontend must decide before [`Engine::open`].
///
/// `workspace` is canonicalized by the engine; the rest maps one-to-one to the
/// `loom run` flags. `system_prompt` defaults to Loom's built-in instruction.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Provider adapter to use. Iteration 1 ships `deepseek`.
    pub provider: String,
    /// Model identifier, validated against the provider's capability table.
    pub model: String,
    /// Optional provider base-URL override (proxies and gateways).
    pub base_url: Option<String>,
    /// Reasoning effort, sent explicitly on every request.
    pub reasoning_effort: ReasoningEffort,
    /// Optional saved account (label or id) instead of the active account.
    pub account: Option<String>,
    /// Workspace root the tools are confined to, canonicalized on open.
    pub workspace: PathBuf,
    /// System prompt; `None` selects Loom's built-in instruction.
    pub system_prompt: Option<String>,
    /// Maximum number of model turns for one task.
    pub max_turns: u32,
    /// Whether sessions are written to the per-user sessions directory.
    pub save_sessions: bool,
}

/// An opened engine: a composed [`Agent`] plus the session policy.
///
/// A frontend holds one engine and drives many tasks; it never sees the
/// provider, the credential, or the tool registry again.
pub struct Engine {
    agent: Agent,
    save_sessions: bool,
}

impl Engine {
    /// Opens the engine: resolves the credential, builds the provider, and
    /// assembles the built-in tools.
    ///
    /// Setup failures are returned here, before any task runs, so a frontend
    /// can report them once (bad provider, missing credential, unknown model)
    /// rather than per task.
    ///
    /// This performs blocking I/O — workspace canonicalization, the SQLite
    /// account store, and the OS keychain (and, for `auth login`, the
    /// credential command) — so a frontend that runs an event loop should call
    /// it off its render thread.
    pub fn open(config: EngineConfig) -> Result<Self, EngineError> {
        if config.provider != "deepseek" {
            return Err(EngineError::UnknownProvider(config.provider));
        }

        let workspace =
            std::fs::canonicalize(&config.workspace).map_err(|source| EngineError::Workspace {
                path: config.workspace.clone(),
                source,
            })?;

        // Credential resolution: --account → active saved account → env
        // (§18.3). The secret never leaves a `SecretString` wrapper.
        let store = SqliteAuthStore::open(&paths::db_path()).map_err(EngineError::Store)?;
        let secrets = KeychainBackend::default();
        let credential = auth::resolve(
            &store,
            &secrets,
            &config.provider,
            config.account.as_deref(),
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
        let mut provider_config = DeepSeekConfig::new(credential.secret.expose().to_owned())
            .with_reasoning_effort(config.reasoning_effort);
        if let Some(base_url) = &config.base_url {
            provider_config = provider_config.with_base_url(base_url.clone());
        }
        let provider = DeepSeekProvider::new(provider_config)
            .map_err(|error| EngineError::Provider(Box::new(error)))?;
        let capabilities =
            provider
                .capabilities(&config.model)
                .ok_or_else(|| EngineError::UnknownModel {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                })?;
        if !capabilities.reasoning {
            return Err(EngineError::ModelWithoutReasoning {
                model: config.model.clone(),
            });
        }

        let registry = build_registry()?;
        let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
        let agent = Agent::new(
            Box::new(provider),
            Box::new(DefaultContextCompiler),
            registry,
            AgentConfig::new(workspace, system_prompt),
            config.model,
        )
        .with_max_turns(config.max_turns);

        Ok(Self {
            agent,
            save_sessions: config.save_sessions,
        })
    }

    /// Creates a fresh session handle for one run.
    ///
    /// The engine owns session *durability*; the frontend owns session
    /// *identity and rendering*. Today the caller keeps the value and passes
    /// it back to [`Engine::run_task`]; the handle-based API (engine-held
    /// sessions, follow-ups, a command channel) lands with persistence.
    #[must_use]
    pub fn new_session(&self) -> Session {
        Session::new(SessionId::new())
    }

    /// Runs one task against `session`, emitting events to `sink`.
    ///
    /// Cancellation is cooperative through `cancellation`. Success returns a
    /// [`TaskSummary`]; every other terminal state — including cancellation —
    /// returns a [`TaskError`], and the persisted [`Session`] carries the same
    /// outcome.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<TaskSummary, TaskError> {
        self.agent
            .run_task(session, input, sink, cancellation)
            .await
    }

    /// Writes the session as a single JSON file, unless saving is disabled.
    ///
    /// Returns the path written, or `None` when the config disabled saving.
    ///
    /// Blocking: this creates the sessions directory and writes the file
    /// synchronously, so a frontend that runs an event loop should call it off
    /// its render thread (or accept the stall at its persistence point).
    pub fn save_session(&self, session: &Session) -> Result<Option<PathBuf>, EngineError> {
        if !self.save_sessions {
            return Ok(None);
        }
        let dir = paths::sessions_dir();
        std::fs::create_dir_all(&dir).map_err(|source| EngineError::SessionDir {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(format!("{}.json", session.id));
        let bytes = serde_json::to_vec_pretty(session).map_err(EngineError::SerializeSession)?;
        std::fs::write(&path, bytes).map_err(|source| EngineError::SessionWrite {
            path: path.clone(),
            source,
        })?;
        Ok(Some(path))
    }
}

/// Registers the built-in tools (§9) in their stable exposure order.
///
/// Read-only tools first, so the prompt prefix stays stable; the mutating
/// tools follow and force the tool engine's sequential mode (§10).
fn build_registry() -> Result<ToolRegistry, EngineError> {
    let mut registry = ToolRegistry::new();
    let mut register = |name: &'static str, tool: Box<dyn loom_kernel::tool::Tool>| {
        registry
            .register(tool)
            .map_err(|error| EngineError::RegisterTool {
                tool: name,
                message: error.to_string(),
            })
    };
    register("read_file", Box::new(ReadFileTool))?;
    register("list_dir", Box::new(ListDirTool))?;
    register("search", Box::new(SearchTool))?;
    register("write_file", Box::new(WriteFileTool))?;
    register("edit_file", Box::new(EditFileTool))?;
    register("run_command", Box::new(RunCommandTool::new()))?;
    Ok(registry)
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

/// Why opening an engine, running a task, or persisting a session failed.
///
/// Terminal *task* failures are not here: they travel as
/// [`loom_kernel::agent::TaskError`] through [`Engine::run_task`], because a
/// failed task is a normal outcome a frontend renders, not a setup error.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The requested provider has no adapter in this build.
    #[error("unknown provider `{0}`: iteration 1 ships `deepseek`")]
    UnknownProvider(String),
    /// The workspace does not exist or cannot be resolved.
    #[error("workspace `{path}` does not exist or is not readable")]
    Workspace {
        /// The workspace path as supplied.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The account store could not be opened.
    #[error("opening the account store: {0}")]
    Store(#[source] auth::AuthError),
    /// No credential could be resolved (ADR-009).
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    /// The provider client could not be constructed.
    ///
    /// Boxed rather than naming `reqwest::Error` so the facade does not leak
    /// the HTTP client's type; the cause is still reachable via
    /// [`std::error::Error::source`].
    #[error("building the DeepSeek provider: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The model is not in the provider's capability table.
    #[error("model `{model}` is unknown to provider `{provider}`: check --model")]
    UnknownModel {
        /// Provider that was requested.
        provider: String,
        /// Model that was not found.
        model: String,
    },
    /// The model must support reasoning: an effort is always sent.
    #[error(
        "model `{model}` does not support reasoning, but a reasoning effort is always sent; \
         choose a reasoning model"
    )]
    ModelWithoutReasoning {
        /// The offending model.
        model: String,
    },
    /// A built-in tool could not be registered.
    #[error("registering {tool}: {message}")]
    RegisterTool {
        /// Tool name being registered.
        tool: &'static str,
        /// The registration failure.
        message: String,
    },
    /// The sessions directory could not be created.
    #[error("creating the session directory `{path}`")]
    SessionDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The session could not be serialized.
    #[error("serialising the session: {0}")]
    SerializeSession(#[source] serde_json::Error),
    /// The session file could not be written.
    #[error("writing the session to `{path}`")]
    SessionWrite {
        /// File that could not be written.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}
