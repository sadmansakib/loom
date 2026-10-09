//! The driving API: open an engine, run one task, persist sessions.
//!
//! [`Engine::open`] resolves credentials, builds the concrete provider, and
//! assembles the built-in tools into one [`Agent`]; the frontend then calls
//! [`Engine::run_task`] with an in-process event sink and a cancellation
//! token. The event protocol never crosses a wire: the sink is a Rust trait,
//! so local frontends pay no serialization cost (ADR-021).

use std::path::PathBuf;
use std::sync::Arc;

use loom_kernel::agent::{Agent, TaskControl, TaskError, TaskSummary};
use loom_kernel::context::{
    AgentConfig, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
};
use loom_kernel::event::{EventKind, EventSink};
use loom_kernel::id::SessionId;
use loom_kernel::session::Session;
use loom_kernel::store::StoreError;
use loom_kernel::tool::ToolRegistry;
use loom_providers::{OPENROUTER_PROVIDER_NAME, ProviderCatalog, ReasoningEffort};
use loom_store::SqliteStore;
use loom_tools::policy::CommandPolicy;
use loom_tools::{
    ArtifactReadTool, EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool,
    VerifyTool, WriteFileTool,
};

use crate::auth::{self, KeychainBackend, SqliteAuthStore};
/// Everything a frontend must decide before [`Engine::open`].
///
/// `workspace` is canonicalized by the engine; the rest maps one-to-one to the
/// `loom run` flags. `system_prompt` defaults to Loom's built-in instruction.
///
/// The database location is the frontend's decision: the engine never
/// consults the environment for it.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Provider adapter to use — `deepseek` or `openrouter`. Adding one
    /// is a new match arm in [`crate::provider`], nowhere else.
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
    /// Path to the durable-state database (`loom.db`) the frontend selected.
    ///
    /// Used for credential accounts and, when `persist` is set, sessions and
    /// artifacts. The engine applies no default: a frontend that wants the
    /// conventional location resolves it before building this config.
    pub db_path: PathBuf,
    /// Whether the session is persisted to the state database (ADR-008).
    ///
    /// When false the run is entirely in memory: a frontend that wants
    /// durability for the session must opt in. `--no-save` clears it.
    pub persist: bool,
    /// Model catalog for catalog-driven adapters, handed in as data.
    ///
    /// A frontend that already holds a snapshot passes the matching
    /// [`ProviderCatalog`] variant here and uses [`Engine::open`], which
    /// stays synchronous and never performs network discovery of its own.
    /// Otherwise [`Engine::open_async`] fetches and caches one through
    /// [`crate::catalog`] first. Adapters that resolve capabilities from a
    /// built-in table ignore it; adapters that need one reject the run
    /// when it is absent.
    pub catalog: Option<ProviderCatalog>,
}

/// A minimal [`EngineConfig`] for tests that need one without a real
/// workspace or database.
#[cfg(test)]
pub(crate) fn test_config() -> EngineConfig {
    let base = std::env::temp_dir().join("loom-engine-test-config");
    EngineConfig {
        provider: "deepseek".to_owned(),
        model: "deepseek-flash".to_owned(),
        base_url: None,
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace: base.join("workspace"),
        system_prompt: None,
        max_turns: 1,
        db_path: base.join("loom.db"),
        persist: false,
        catalog: None,
    }
}

/// An opened engine: a composed [`Agent`] plus the session policy.
///
/// A frontend holds one engine and drives many tasks; it never sees the
/// provider, the credential, or the tool registry again.
pub struct Engine {
    agent: Agent,
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
        let workspace =
            std::fs::canonicalize(&config.workspace).map_err(|source| EngineError::Workspace {
                path: config.workspace.clone(),
                source,
            })?;

        // Credential resolution: --account → active saved account → env
        // (§18.3). The secret never leaves a `SecretString` wrapper.
        let store = SqliteAuthStore::open(&config.db_path).map_err(EngineError::Store)?;
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

        // Provider construction and loud capability validation: all
        // adapter-specific knowledge lives in `provider`.
        let handle = crate::provider::build(&config, credential.secret.expose())?;

        let registry = build_registry(&CommandPolicy::from_manifest(&workspace)?)?;
        let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
        // Stage-1 repository facts (2E, ADR-015): the composition root owns the
        // I/O; the loop scans once per task through this source.
        let facts = crate::repo::FsFactsSource::new(workspace.clone());
        // Token estimates use a real tokenizer — the same estimator feeds
        // the census, the budget check, and the pruning math. The family
        // follows the provider: Anthropic counts tokens differently from
        // the OpenAI-vocabulary backends, and its 1M window only holds if
        // the estimate errs high.
        let estimator: Arc<dyn loom_kernel::context::TokenEstimator> =
            crate::estimate::estimator_for(&config.provider);
        let mut agent = Agent::new(
            handle.provider,
            Box::new(
                PruningContextCompiler::new(
                    Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                    PruneConfig::default(),
                )
                .with_estimator(estimator),
            ),
            registry,
            AgentConfig::new(workspace, system_prompt),
            config.model,
        )
        .with_max_turns(config.max_turns)
        .with_facts_source(Box::new(facts));

        // Durable sessions and artifact bodies (ADR-008, ADR-012): the same
        // database the frontend chose for accounts. `--no-save` leaves the
        // run in memory only, so nothing spills and `artifact_read` has no
        // store.
        if config.persist {
            let store = SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
            agent = agent.with_store(Box::new(store));
            let artifacts =
                SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
            agent = agent.with_artifacts(Some(Arc::new(artifacts)));
        }

        Ok(Self { agent })
    }

    /// Opens the engine, fetching the OpenRouter catalog first when one is
    /// needed and not already supplied.
    ///
    /// The asynchronous counterpart of [`Engine::open`]: capabilities are
    /// resolved from the catalog, so `openrouter` runs must obtain one
    /// before the synchronous open. The sequencing — resolve the
    /// credential, fetch, cache beside the database, fall back to a stale
    /// cache — lives here ([`crate::catalog`]) so every frontend gets it
    /// identically instead of re-implementing it before `open`. A config
    /// that already carries a catalog, or names a provider that needs
    /// none, skips the network entirely.
    ///
    /// Await this from the frontend's async context; everything after the
    /// optional fetch is the same blocking work [`Engine::open`] documents.
    pub async fn open_async(config: EngineConfig) -> Result<Self, EngineError> {
        let config = if config.catalog.is_none() && config.provider == OPENROUTER_PROVIDER_NAME {
            let auth = crate::auth::Auth::open(&config.db_path)?;
            let catalog = crate::catalog::load(
                &auth,
                &config.provider,
                config.account.as_deref(),
                config.base_url.as_deref(),
                &config.db_path,
            )
            .await?;
            EngineConfig {
                catalog: Some(ProviderCatalog::OpenRouter(catalog)),
                ..config
            }
        } else {
            config
        };
        Engine::open(config)
    }

    /// Creates a fresh session handle for one run.
    ///
    /// The engine owns session *durability*; the frontend owns session
    /// *identity and rendering*. When the engine was opened with persistence,
    /// the id can later be handed to [`Engine::load_session`] to resume. A
    /// frontend that keeps the value can also pass it straight back to
    /// [`Engine::run_task`] for follow-up tasks in the same session.
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
    ///
    /// After the task's terminal event the engine emits
    /// [`EventKind::AwaitingInput`] naming `session`: the session is idle and
    /// accepts more input, whether through another `run_task` call or
    /// [`Engine::continue_session`]. Stream-only frontends key their resume
    /// loop on it.
    ///
    /// `control` carries the operator's two stop granularities:
    /// [`TaskControl::cancel`] aborts mid-flight (the session survives —
    /// every committed item was persisted), [`TaskControl::pause`] stops at
    /// the next turn boundary with the in-flight turn fully committed.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        let outcome = self.agent.run_task(session, input, sink, control).await;
        // The engine is idle again once the task's terminal event has been
        // emitted. The marker carries the ended task's id (envelope shape)
        // and the session (payload); a session whose very first user item
        // failed to persist has no task record, and no marker either.
        if let Some(record) = session.tasks.last() {
            sink.emit(
                record.id,
                EventKind::AwaitingInput {
                    session_id: session.id,
                },
            );
        }
        outcome
    }

    /// Continues a persisted session: loads it, appends `input` as its
    /// next user item, and runs the task loop to the next stopping point.
    ///
    /// The first-class resume flow (the core seam of roadmap item 15): the
    /// advanced [`Session`] and the task's outcome travel back together in a
    /// [`Continuation`]. A failed, cancelled, or paused task is **not** an
    /// `Err` here — the session has still advanced (input committed, partial
    /// turns persisted) and the caller can continue again — only loading
    /// problems are errors: an unknown id (or no persistence) is
    /// [`EngineError::SessionNotFound`], a store failure
    /// [`EngineError::Session`].
    ///
    /// Blocking up front (SQLite load); same call-off-thread guidance as
    /// [`Self::load_session`].
    pub async fn continue_session(
        &self,
        id: &SessionId,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<Continuation, EngineError> {
        let mut session = self
            .load_session(id)?
            .ok_or(EngineError::SessionNotFound { id: *id })?;
        let outcome = self.run_task(&mut session, input, sink, control).await;
        Ok(Continuation { session, outcome })
    }

    /// Loads a persisted session so a frontend can resume it.
    ///
    /// Returns `Ok(None)` when persistence is disabled or the session is
    /// unknown. The store is the durable form of the transcript (ADR-008);
    /// `run_task` continues from whatever it returns.
    ///
    /// Blocking: reads SQLite synchronously, so an event-loop frontend should
    /// call it off its render thread.
    pub fn load_session(&self, id: &SessionId) -> Result<Option<Session>, EngineError> {
        match self.agent.load_session(id) {
            Ok(session) => Ok(session),
            Err(error) => Err(EngineError::Session(error)),
        }
    }

    /// Lists the ids of every persisted session, oldest creation order.
    ///
    /// Blocking (SQLite); same call-off-thread guidance as [`Self::load_session`].
    pub fn session_ids(&self) -> Result<Vec<SessionId>, EngineError> {
        self.agent.session_ids().map_err(EngineError::Session)
    }
}

/// A continued session: the advanced transcript plus the task's outcome.
///
/// [`Engine::continue_session`] returns this even when the task failed or
/// was cancelled — the session is the durable artifact; the outcome is
/// what the frontend renders.
#[non_exhaustive]
#[derive(Debug)]
pub struct Continuation {
    /// The session after the task ran: input committed, turns persisted,
    /// ready for another continuation.
    pub session: Session,
    /// The task's terminal outcome. `Err` here is a task-level result
    /// ([`TaskError`]), not a load error — the load already succeeded.
    pub outcome: Result<TaskSummary, TaskError>,
}

/// Registers the built-in tools (§9) in their stable exposure order.
///
/// Read-only tools first, so the prompt prefix stays stable; the mutating
/// tools follow and force the tool engine's sequential mode (§10).
/// `run_command` runs under the workspace's command policy (manifest
/// `.loom/commands.toml`, else the shipped `cargo`-only default).
fn build_registry(policy: &CommandPolicy) -> Result<ToolRegistry, EngineError> {
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
    register("artifact_read", Box::new(ArtifactReadTool))?;
    register("write_file", Box::new(WriteFileTool))?;
    register("edit_file", Box::new(EditFileTool))?;
    register(
        "run_command",
        Box::new(RunCommandTool::with_policy(policy.clone())),
    )?;
    register("verify", Box::new(VerifyTool::with_policy(policy.clone())))?;
    Ok(registry)
}

/// The built-in instruction; overridable with `--system-prompt`.
/// The default system prompt: what a frontend that passes no
/// `system_prompt` runs with, and the one the eval harness must measure
/// coding tasks against, so its frozen copies cannot drift from the
/// product default.
pub fn default_system_prompt() -> String {
    String::from(
        "You are Loom, a coding agent working inside one workspace. Use the provided \
         tools to inspect and change the workspace: list and search before reading, \
         read before editing, write or edit files to make changes, and verify \
         changes with cargo. Raw commands run inside an OS sandbox — writes stay \
         in the workspace and the network is off — so run any bare program the \
         task needs. Prefer the smallest change that works. When you \
         answer, be concise and name the workspace-relative paths you relied on.\n\
         Spend context carefully: issue every independent read-only call in \
         the same turn — they run in parallel — and never mix a mutation \
         into a read batch. When a write or edit touches compilable code, \
         set that call's `verify` argument to \"check\" (or \"test\") so the \
         change is proved in the turn that made it.",
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
    #[error("unknown provider `{0}`: no adapter is registered for it")]
    UnknownProvider(String),
    /// A catalog-driven provider was selected without a catalog.
    #[error("provider `{provider}` needs a model catalog, but none was loaded")]
    CatalogMissing {
        /// The provider that requires the catalog.
        provider: String,
    },
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
    /// The workspace's command policy manifest exists but is invalid
    /// (roadmap: command execution policy). Failing the open keeps a
    /// broken manifest from silently widening execution to default.
    #[error("loading the command policy: {0}")]
    CommandPolicy(#[from] loom_tools::policy::PolicyError),
    /// No credential could be resolved (ADR-009).
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    /// The provider client could not be constructed.
    ///
    /// Boxed rather than naming `reqwest::Error` so the facade does not leak
    /// the HTTP client's type; the cause is still reachable via
    /// [`std::error::Error::source`].
    #[error("building the provider: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The model is not in the provider's capability table.
    #[error("model `{model}` is unknown to provider `{provider}`: check --model")]
    UnknownModel {
        /// Provider that was requested.
        provider: String,
        /// Model that was not found.
        model: String,
    },
    /// The model's context window is below the pruning floor.
    #[error(
        "model `{model}` has a {tokens}-token context window, below the minimum Loom needs to \
         recover context"
    )]
    ModelWindowTooSmall {
        /// The offending model.
        model: String,
        /// Its advertised context window.
        tokens: u32,
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
    /// The session store could not be opened, read, or written.
    #[error("session store: {0}")]
    Session(#[source] StoreError),
    /// The session to continue does not exist (or persistence is off).
    #[error("session {id} was not found: nothing to continue")]
    SessionNotFound {
        /// The id that did not resolve.
        id: SessionId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use loom_kernel::context::{AgentConfig, DefaultContextCompiler};
    use loom_kernel::model::{
        ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
        ModelResponse, ModelStream, StopReason, Usage,
    };
    use loom_kernel::session::TaskStatus;
    use loom_kernel::store::MemorySessionStore;
    use loom_kernel::transcript::AssistantMessage;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A provider that replays one scripted event list per `stream()` call.
    /// Enough engine coverage for the driving layer: the kernel loop's own
    /// behaviour is pinned in `loom-kernel`'s `agent_loop` tests.
    struct FakeProvider {
        scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    }

    impl FakeProvider {
        /// A provider replaying the given scripts, one per `stream()` call.
        fn scripted(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from(scripts)),
            }
        }

        /// One script: a single response that answers with `text` and ends
        /// its turn.
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

        /// Scripts for two tasks: the first ends its turn, the second fails
        /// hard with `error`.
        fn end_turn_then_error(text: &str, error: ModelError) -> Self {
            Self::scripted(vec![Self::end_turn_script(text), vec![Err(error)]])
        }
    }

    impl ModelProvider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192))
        }

        fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(stream::iter(script))
        }
    }

    /// An engine around a fake provider with an in-memory store — the
    /// session-lifecycle seams without credentials, network, or files.
    fn fake_engine(provider: FakeProvider) -> Engine {
        let agent = Agent::new(
            Box::new(provider),
            Box::new(DefaultContextCompiler::default()),
            loom_kernel::tool::ToolRegistry::new(),
            AgentConfig::new(PathBuf::from("."), "test agent"),
            "fake-model",
        )
        .with_store(Box::new(MemorySessionStore::new()));
        Engine { agent }
    }

    #[tokio::test]
    async fn continue_session_resumes_a_persisted_session() {
        let engine = fake_engine(FakeProvider::scripted(vec![
            FakeProvider::end_turn_script("first"),
            FakeProvider::end_turn_script("second"),
        ]));
        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();

        engine
            .run_task(
                &mut session,
                "do the first thing",
                &sink,
                &Default::default(),
            )
            .await
            .expect("first task");

        // Resume through the first-class flow: the id alone must suffice.
        let continuation = engine
            .continue_session(&session.id, "now the second", &sink, &Default::default())
            .await
            .expect("continue");

        let summary = continuation.outcome.expect("second task completed");
        assert_eq!(summary.turns, 1);
        assert_eq!(continuation.session.tasks.len(), 2);
        // The store is the authority: reloading the id sees both tasks.
        let reloaded = engine
            .load_session(&session.id)
            .expect("load")
            .expect("persisted");
        assert_eq!(reloaded.tasks.len(), 2);

        // Idle markers: one per task end, each naming the session, always
        // after the terminal event.
        let kinds = drain(&mut receiver);
        let idle: Vec<_> = kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::AwaitingInput { .. }))
            .collect();
        assert_eq!(idle.len(), 2, "kinds: {kinds:?}");
        assert!(matches!(
            kinds.last(),
            Some(EventKind::AwaitingInput { session_id }) if *session_id == session.id
        ));
    }

    #[tokio::test]
    async fn continue_session_reports_unknown_ids() {
        let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
            "unused",
        )]));
        let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
        let error = engine
            .continue_session(&SessionId::new(), "anything", &sink, &Default::default())
            .await
            .unwrap_err();
        assert!(matches!(error, EngineError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn continue_session_returns_the_advanced_session_on_task_failure() {
        // First task fine, second task a hard model failure: the
        // continuation still carries the advanced session.
        let engine = fake_engine(FakeProvider::end_turn_then_error(
            "first",
            ModelError::new(ModelErrorKind::Auth, "key rejected"),
        ));
        let mut session = engine.new_session();
        let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
        engine
            .run_task(&mut session, "first", &sink, &Default::default())
            .await
            .expect("first task");

        let continuation = engine
            .continue_session(&session.id, "second", &sink, &Default::default())
            .await
            .expect("continuation is not a load error");

        assert!(continuation.outcome.is_err(), "task failed");
        assert_eq!(continuation.session.tasks.len(), 2);
        // The failed task is persisted with its terminal status, so a
        // later continuation starts from honest state.
        assert_eq!(continuation.session.tasks[1].status, TaskStatus::Failed);
        // Idle is still signalled after a failed task; the marker itself
        // is pinned in the resume test above.
    }

    /// Drains every pending event kind from the channel.
    fn drain(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<loom_kernel::event::EventEnvelope>,
    ) -> Vec<EventKind> {
        let mut kinds = Vec::new();
        while let Ok(envelope) = receiver.try_recv() {
            kinds.push(envelope.kind);
        }
        kinds
    }

    #[tokio::test]
    async fn open_async_resolves_the_credential_before_fetching() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut config = test_config();
        config.provider = OPENROUTER_PROVIDER_NAME.to_owned();
        config.workspace = workspace;
        config.db_path = base.path().join("loom.db");

        // No cached catalog and no credential: the fetch path must reach
        // credential resolution (inside `catalog::load`) and report the
        // missing credential — never a network call in tests.
        let error = match Engine::open_async(config).await {
            Ok(_) => panic!("open succeeded without a credential"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                EngineError::Auth(auth::AuthError::NoCredential { .. })
            ),
            "unexpected error: {error}"
        );
    }
}
