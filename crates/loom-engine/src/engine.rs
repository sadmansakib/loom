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
    AgentConfig, ContextCompiler, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
};
use loom_kernel::event::{EventKind, EventSink, PersistingEventSink};
use loom_kernel::id::SessionId;
use loom_kernel::model::ModelProvider;
use loom_kernel::session::Session;
use loom_kernel::store::{SharedEventStore, StoreError};
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
    /// The canonicalized workspace root, for the fast paths' file walk.
    workspace: std::path::PathBuf,
    /// The command policy the registry was built from; the fast path
    /// verifies with the same sandbox bounds the tools run under.
    policy: loom_tools::policy::CommandPolicy,
    /// Whether [`Engine::run_task`] tries a deterministic fast path
    /// before spawning the agent loop (roadmap item 3). Off in the
    /// bare-loop arm — the A/B baseline must spend its turns like a
    /// basic harness would.
    fast_paths: bool,
    /// The capability-resolution facts (source, model) to surface once
    /// per engine instance as a `CapabilitiesResolved` event (v8).
    /// `None` only for hand-built test engines.
    resolution: Option<(&'static str, String)>,
    /// Guards the one-shot emission of the resolution events.
    notices_flushed: std::sync::OnceLock<()>,
    /// The event-stream persistence seam (roadmap item 1's open tail):
    /// when the engine persists sessions, it also persists every
    /// observed event append-on-emit, so `loom stats` can attribute
    /// savings per mechanism from a stored session. `None` when
    /// `persist` was off — the stream stays in-memory only.
    events: Option<SharedEventStore>,
}

/// A function that wraps the provider an open produced — built from
/// credentials or handed in via [`EngineOverrides::provider`] — so an
/// embedder can record or shadow model traffic without duplicating the
/// build path.
pub type ProviderHook = Arc<dyn Fn(Box<dyn ModelProvider>) -> Box<dyn ModelProvider> + Send + Sync>;

/// Composition overrides for embedders that need to swap one production
/// choice without re-wiring the whole agent (loom-eval's harness is the
/// first consumer; a GUI with per-view toolsets will be the second).
///
/// Every field is optional: `None` keeps [`Engine::open`]'s production
/// choice, so an override only ever narrows the gap between an embedder
/// and the real frontend path — the drift risk the overrides exist to
/// remove. Fields that are set replace the production choice entirely;
/// the embedder owns what it replaces (an overridden compiler picks its
/// own estimator, for example).
#[derive(Default)]
pub struct EngineOverrides {
    /// A provider handed in as a boxed trait object, replacing the
    /// credential → adapter build entirely. Offline replay uses this
    /// (no secret is resolved, no network client is built).
    pub provider: Option<Box<dyn ModelProvider>>,
    /// Wraps whichever provider results — built from credentials, or
    /// supplied via [`Self::provider`] — so an embedder can record or
    /// shadow the model traffic without duplicating the build path.
    pub provider_hook: Option<ProviderHook>,
    /// A tool registry built by the embedder, replacing the
    /// manifest-driven built-in set.
    pub registry: Option<ToolRegistry>,
    /// A context compiler built by the embedder, replacing the default
    /// pruning chain.
    pub compiler: Option<Box<dyn ContextCompiler>>,
    /// The bare-loop arm (roadmap item 1): strips every harness lever
    /// the full engine adds, leaving read/edit/write/shell/list plus the
    /// plain append-only compiler — the "basic harness" baseline the
    /// G4 claim is measured against. Concretely: no repository facts,
    /// no pruning compiler, no completion gate, no failure-ledger
    /// refusal, and a registry reduced to the five primitive tools.
    /// Composition with the other overrides: an embedder-supplied
    /// registry or compiler still wins (the eval harness pins its own
    /// per-task registry); `bare_loop` only removes what the engine
    /// itself would have added.
    pub bare_loop: bool,
    /// Overrides the fast-path default (roadmap item 3). `None` keeps
    /// the production choice — fast paths on — so a mechanical
    /// instruction costs zero model calls; `Some(false)` disables them
    /// (the bare-loop arm does this implicitly). Fast paths only ever
    /// replace the loop when the workspace's own compiler verifies the
    /// edit green, and revert on failure, so disabling them is a
    /// measurement choice, not a correctness one.
    pub fast_paths: Option<bool>,
}

impl EngineOverrides {
    /// Sets a provider that replaces the credential-driven build.
    #[must_use]
    pub fn with_provider(mut self, provider: Box<dyn ModelProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Sets a hook that wraps the resulting provider.
    #[must_use]
    pub fn with_provider_hook(mut self, hook: ProviderHook) -> Self {
        self.provider_hook = Some(hook);
        self
    }

    /// Sets a registry that replaces the built-in tools.
    #[must_use]
    pub fn with_registry(mut self, registry: ToolRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Sets a compiler that replaces the default pruning chain.
    #[must_use]
    pub fn with_compiler(mut self, compiler: Box<dyn ContextCompiler>) -> Self {
        self.compiler = Some(compiler);
        self
    }

    /// Selects the bare-loop arm ([`EngineOverrides::bare_loop`]).
    #[must_use]
    pub fn bare_loop(mut self) -> Self {
        self.bare_loop = true;
        self
    }
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
        Self::open_with(config, EngineOverrides::default())
    }

    /// [`Engine::open`] with embedder overrides
    /// ([`EngineOverrides`]): a handed-in provider, a provider-wrapping
    /// hook, a custom tool registry, or a custom context compiler. Every
    /// `None` field keeps the production choice; see the overrides type
    /// for the exact semantics of each.
    pub fn open_with(
        config: EngineConfig,
        overrides: EngineOverrides,
    ) -> Result<Self, EngineError> {
        let workspace =
            std::fs::canonicalize(&config.workspace).map_err(|source| EngineError::Workspace {
                path: config.workspace.clone(),
                source,
            })?;

        // Provider: an overridden provider skips the credential build
        // entirely (offline replay needs no secret); a hook then wraps
        // whichever provider resulted, so traffic capture composes with
        // both paths.
        let provider: Box<dyn ModelProvider> = match overrides.provider {
            Some(provider) => provider,
            None => {
                // Credential resolution: --account → active saved account →
                // env (§18.3). The secret never leaves a `SecretString`.
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
                crate::provider::build(&config, credential.secret.expose())?.provider
            }
        };
        let provider = match overrides.provider_hook {
            Some(hook) => hook(provider),
            None => provider,
        };
        // The resolution facts the trace surfaces once per engine (v8):
        // which authority — catalog row, provider table, or recording —
        // fixed this model's capabilities.
        let resolution = crate::provider::resolution_source(&config, provider.as_ref());

        // One policy drives both the tool registry and the fast paths'
        // verification runs, so a manifest rule changes them together.
        let policy = CommandPolicy::from_manifest(&workspace)?;
        let registry = match overrides.registry {
            Some(registry) => registry,
            None => {
                let built = build_registry(&policy)?;
                // The bare-loop arm keeps the primitive tool set only
                // (read/list/write/edit/shell); a search index, the
                // artifact re-fetch, the fused verify, and the
                // kernel-owned todo list are harness levers the baseline
                // must run without.
                if overrides.bare_loop {
                    bare_registry(built)
                } else {
                    built
                }
            }
        };
        let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
        // Stage-1 repository facts (2E, ADR-015): the composition root owns the
        // I/O; the loop scans once per task through this source. The
        // bare-loop arm attaches no source — the empty block is the
        // baseline's point.
        let facts =
            (!overrides.bare_loop).then(|| crate::repo::FsFactsSource::new(workspace.clone()));
        let compiler: Box<dyn ContextCompiler> = match overrides.compiler {
            Some(compiler) => compiler,
            None => {
                // Token estimates use a real tokenizer — the same estimator
                // feeds the census, the budget check, and the pruning math.
                // The family follows the provider: Anthropic counts tokens
                // differently from the OpenAI-vocabulary backends, and its
                // 1M window only holds if the estimate errs high.
                let estimator: Arc<dyn loom_kernel::context::TokenEstimator> =
                    crate::estimate::estimator_for(&config.provider);
                // The bare-loop arm compiles plain append-only context —
                // no pruning, no checkpoints — so a bare run measures the
                // levers the full chain adds.
                if overrides.bare_loop {
                    Box::new(DefaultContextCompiler::with_estimator(estimator))
                } else {
                    Box::new(
                        PruningContextCompiler::new(
                            Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                            prune_config_for(&config.provider),
                        )
                        .with_estimator(estimator),
                    )
                }
            }
        };
        // The bare-loop arm also disarms the LLM handoff: the plain
        // compiler never freezes checkpoints, so a handoff attempt there
        // could only spend a call and fail. Both levers flip together.
        let mut agent_config = AgentConfig::new(workspace.clone(), system_prompt);
        if overrides.bare_loop {
            agent_config.handoff_compaction = false;
        }
        let mut agent = Agent::new(provider, compiler, registry, agent_config, config.model)
            .with_max_turns(config.max_turns)
            .with_loop_shaping(!overrides.bare_loop);
        if let Some(facts) = facts {
            agent = agent.with_facts_source(Box::new(facts));
        }

        // Durable sessions and artifact bodies (ADR-008, ADR-012): the same
        // database the frontend chose for accounts. `--no-save` leaves the
        // run in memory only, so nothing spills and `artifact_read` has no
        // store.
        if config.persist {
            let store = SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
            let artifacts =
                SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
            // The event-stream persistence seam shares the database
            // (schema v2's `events` table) and the same write cadence
            // as the transcript: append-on-emit, best-effort — a store
            // failure counts drops and never fails the task.
            let events = SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;

            // The lifecycle pass (roadmap 2.17), best-effort at open: the
            // transcript scan marks the live set, then only unreferenced
            // bodies past the backstop bounds are reclaimed. A scan or
            // collection failure warns and never blocks the open — and a
            // failed scan skips the run entirely, because an empty live
            // set would treat every body as an orphan.
            let gc =
                loom_kernel::artifact_gc::referenced_artifacts(&store).and_then(|referenced| {
                    loom_kernel::artifact_gc::collect(
                        &artifacts,
                        &referenced,
                        loom_kernel::artifact_gc::now_millis(),
                        loom_kernel::artifact_gc::GcBounds::default(),
                    )
                });
            if let Err(error) = gc {
                eprintln!("warn: artifact gc skipped: {error}");
            }

            agent = agent.with_store(Box::new(store));
            agent = agent.with_artifacts(Some(Arc::new(artifacts)));
            Ok(Self {
                agent,
                workspace,
                policy,
                // The bare-loop arm measures a basic harness, and a basic
                // harness does not short-circuit mechanical tasks — the
                // fast path is a Loom lever, so it flips with the others.
                fast_paths: overrides.fast_paths.unwrap_or(true) && !overrides.bare_loop,
                resolution: Some(resolution),
                notices_flushed: std::sync::OnceLock::new(),
                events: Some(Arc::new(events)),
            })
        } else {
            Ok(Self {
                agent,
                workspace,
                policy,
                fast_paths: overrides.fast_paths.unwrap_or(true) && !overrides.bare_loop,
                resolution: Some(resolution),
                notices_flushed: std::sync::OnceLock::new(),
                events: None,
            })
        }
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
        Self::open_async_with(config, EngineOverrides::default()).await
    }

    /// [`Engine::open_async`] with embedder overrides
    /// ([`EngineOverrides`]) — the catalog sequencing of the async open
    /// composed with the substitution points of [`Engine::open_with`].
    pub async fn open_async_with(
        config: EngineConfig,
        overrides: EngineOverrides,
    ) -> Result<Self, EngineError> {
        // The catalog is fetched for every provider, not just the
        // catalog-driven one: capability resolution is catalog-first for
        // the table adapters too, so a model the adapter's static table
        // never heard of runs after a catalog refresh with no code
        // change. The fetch always uses the OpenRouter credential of its
        // own — a run against any other provider must not have its
        // account or base URL steer the catalog request — and when the
        // fetch fails, only `openrouter` runs propagate the error (their
        // adapter cannot resolve models without a catalog); table-driven
        // adapters fall back to their built-in rows.
        let config = if config.catalog.is_none() {
            let is_openrouter = config.provider == OPENROUTER_PROVIDER_NAME;
            let auth = crate::auth::Auth::open(&config.db_path)?;
            let account = is_openrouter.then(|| config.account.clone()).flatten();
            let base_url = is_openrouter.then(|| config.base_url.clone()).flatten();
            match crate::catalog::load(
                &auth,
                OPENROUTER_PROVIDER_NAME,
                account.as_deref(),
                base_url.as_deref(),
                &config.db_path,
            )
            .await
            {
                Ok(catalog) => EngineConfig {
                    catalog: Some(ProviderCatalog::OpenRouter(catalog)),
                    ..config
                },
                Err(_) if !is_openrouter => config,
                Err(error) => return Err(error),
            }
        } else {
            config
        };
        Engine::open_with(config, overrides)
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
    /// When the engine was opened with persistence, the sink is wrapped
    /// so every event is also appended to the session's durable event
    /// stream (append-on-emit, best-effort) before `sink` sees it — the
    /// same events, the same order, one durable copy.
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
        // Event persistence (roadmap item 1's open tail): when the
        // engine persists sessions it persists the observed event
        // stream alongside them, append-on-emit, best-effort. The
        // wrapper sits above everything this method (and the loop
        // under it) emits, so the stored stream is the complete run —
        // including the engine-level markers below.
        match &self.events {
            Some(store) => {
                let persisting = PersistingEventSink::new(store.clone(), session.id, sink);
                self.drive_task(session, input, &persisting, control).await
            }
            None => self.drive_task(session, input, sink, control).await,
        }
    }

    /// [`Engine::run_task`]'s body, driven against whichever sink the
    /// persistence wrapper produced.
    async fn drive_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        let input: String = input.into();
        // The fast paths (roadmap item 3): a strict analyzer may solve a
        // mechanical instruction deterministically — edits applied,
        // compiler-verified green — with zero model calls. Any miss,
        // declined guard, or reverted failure falls through to the
        // agent loop over an untouched workspace, so the loop is never
        // worse off for trying.
        let mut fast_path_decline: Option<&'static str> = None;
        if self.fast_paths {
            let attempt =
                loom_tools::fastpath::try_fast_path(&self.workspace, &input, &self.policy).await;
            match attempt.outcome {
                Some(solve) => {
                    let summary = self
                        .agent
                        .record_fast_path(
                            session,
                            input,
                            loom_kernel::agent::FastPathSolve {
                                kind: solve.kind.to_owned(),
                                files_changed: solve.files_changed,
                                note: solve.note,
                            },
                            sink,
                        )
                        .await?;
                    // Same idle marker the loop-driven path emits: the
                    // session accepts follow-up input either way.
                    if let Some(record) = session.tasks.last() {
                        sink.emit(
                            record.id,
                            EventKind::AwaitingInput {
                                session_id: session.id,
                            },
                        );
                    }
                    return Ok(summary);
                }
                None => fast_path_decline = attempt.decline,
            }
        }
        let outcome = self.agent.run_task(session, input, sink, control).await;
        // v8 surfaced decisions, attributed to the task that just ran:
        // the fast-path attempt that fell through (post-hoc — the
        // attempt happens before a task record exists), then the
        // engine's one-shot resolution facts.
        if let Some(record) = session.tasks.last() {
            if let Some(reason) = fast_path_decline {
                sink.emit(
                    record.id,
                    EventKind::FastPathDeclined {
                        reason: reason.to_owned(),
                    },
                );
            }
            if self.notices_flushed.set(()).is_ok()
                && let Some((source, model)) = &self.resolution
            {
                sink.emit(
                    record.id,
                    EventKind::CapabilitiesResolved {
                        model: model.clone(),
                        source: (*source).to_owned(),
                    },
                );
                sink.emit(
                    record.id,
                    EventKind::HarnessNotice {
                        category: String::from("sandbox"),
                        detail: loom_tools::sandbox::baseline().to_owned(),
                    },
                );
            }
        }
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
/// `.loom/commands.toml`, else the shipped `cargo`-only default). The
/// kernel-owned `todo` tool (roadmap item 8 phase B) registers last: it
/// mutates kernel session state, not files, and joining here — rather
/// than being re-registered by embedders — keeps one wiring for every
/// frontend. The bare-loop arm filters it back out ([`bare_registry`]).
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
    register("todo", Box::new(loom_kernel::tool::TodoTool::new()))?;
    Ok(registry)
}

/// The bare-loop registry: the primitive tools a basic harness offers
/// (read/list/write/edit/shell), filtered out of the built set. Anything
/// else the full engine registers — search, the artifact refetch, the
/// fused verify, the kernel-owned `todo` list — is a harness lever the
/// baseline must run without.
fn bare_registry(built: ToolRegistry) -> ToolRegistry {
    const PRIMITIVES: [&str; 5] = [
        "read_file",
        "list_dir",
        "write_file",
        "edit_file",
        "run_command",
    ];
    ToolRegistry::from_tools(
        built
            .into_tools()
            .filter(|tool| PRIMITIVES.contains(&tool.spec().name.as_str())),
    )
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
         answer, be concise and name the workspace-relative paths you relied on. \
         Never repeat a call that already failed with the same arguments: change \
         the arguments or the approach, or the identical retry is refused.\n\
         Spend context carefully: issue every independent read-only call in \
         the same turn — they run in parallel — and never mix a mutation \
         into a read batch. When a write or edit touches compilable code, \
         set that call's `verify` argument to \"check\" (or \"test\") so the \
         change is proved in the turn that made it.",
    )
}

/// Pruning config for a provider — the one place reasoning elision is
/// provider-gated (roadmap item 12, deterministic half).
///
/// Replaying stored reasoning verbatim is a provider contract, not an
/// optimisation, on the OpenAI wire: DeepSeek rejects a tool-bearing
/// request whose prior assistant turns lack `reasoning_content`
/// (live-verified 2026-10-03), so reasoning elision stays off for the
/// OpenAI-vocabulary backends. GLM's Preserved Thinking merely gets
/// *better* cache hits when the client echoes reasoning — dropping old
/// traces forfeits that benefit for the affected turns but never fails
/// the request — and Anthropic never forwards reasoning on replay at
/// all, so for both the elision is a pure context win behind the same
/// episodic checkpoint freeze (one cache miss per aging batch).
fn prune_config_for(provider: &str) -> PruneConfig {
    let mut config = PruneConfig::default();
    if provider == loom_providers::GLM_PROVIDER_NAME
        || provider == loom_providers::ANTHROPIC_PROVIDER_NAME
    {
        config.elide_reasoning = Some(8);
    }
    config
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
    use loom_kernel::context::{AgentConfig, DefaultContextCompiler, PruneConfig};
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
            // Tool-calling on: scripts emit tool calls, and the default
            // compiler exposes tools only to a model that can call them
            // (§8.2) — with this off, every scripted call would bounce
            // back `NotExposed` instead of running.
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
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
        Engine {
            agent,
            workspace: PathBuf::from("."),
            policy: CommandPolicy::default(),
            fast_paths: false,
            resolution: None,
            notices_flushed: std::sync::OnceLock::new(),
            events: None,
        }
    }

    /// A one-file cargo fixture the fast paths can rename inside.
    fn rename_fixture(base: &std::path::Path) -> std::path::PathBuf {
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("src")).expect("dirs");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fp_rename\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn parse_opts() -> u32 {\n    7\n}\n\n#[cfg(test)]\nmod tests {\n    \
             use super::*;\n\n    #[test]\n    fn calls_the_helper() {\n        assert_eq!(parse_opts(), 7);\n    }\n}\n",
        )
        .expect("lib");
        workspace
    }

    #[tokio::test]
    async fn fast_path_solves_a_rename_with_zero_model_requests() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        // A scripted provider proves the negative: the queue is never
        // drained, and the summary reports zero requests.
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("should never be asked"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Rename parse_opts to parse_options",
                &sink,
                &Default::default(),
            )
            .await
            .expect("fast path task");

        assert_eq!(summary.model_requests, 0, "no model calls were needed");
        assert_eq!(summary.turns, 0);
        let kinds = drain(&mut receiver);
        assert!(kinds.contains(&EventKind::FastPathHit {
            kind: "rename".to_owned(),
            files_changed: 1
        }));
        assert!(kinds.contains(&EventKind::TaskCompleted));
        assert_eq!(
            session
                .tasks
                .last()
                .and_then(|task| task.end_reason.as_deref()),
            Some("fast_path")
        );
        let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
        assert!(lib.contains("pub fn parse_options() -> u32"));
        assert!(!lib.contains("parse_opts"));
    }

    /// A minimal cargo fixture whose manifest pins one dependency the
    /// fast path can bump.
    fn dependency_fixture(base: &std::path::Path) -> std::path::PathBuf {
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("src")).expect("dirs");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fp_deps\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\ntiny-dep = \"1.0.0\"\n",
        )
        .expect("manifest");
        std::fs::write(
            workspace.join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .expect("main");
        workspace
    }

    #[tokio::test]
    async fn fast_path_solves_a_dependency_update_with_zero_model_requests() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = dependency_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("should never be asked"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Update the dependency tiny-dep to 2.0.0.",
                &sink,
                &Default::default(),
            )
            .await
            .expect("fast path task");

        assert_eq!(summary.model_requests, 0, "no model calls were needed");
        assert_eq!(summary.turns, 0);
        let kinds = drain(&mut receiver);
        assert!(kinds.contains(&EventKind::FastPathHit {
            kind: "dependency".to_owned(),
            files_changed: 1
        }));
        assert!(kinds.contains(&EventKind::TaskCompleted));
        let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("tiny-dep = \"2.0.0\""));
        assert!(!manifest.contains("1.0.0"));
    }

    #[tokio::test]
    async fn unmatched_instructions_fall_through_to_the_agent_loop() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("done"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Say hello and stop.",
                &sink,
                &Default::default(),
            )
            .await
            .expect("agent task");

        assert_eq!(summary.model_requests, 1, "the agent loop ran normally");
        let kinds = drain(&mut receiver);
        assert!(
            !kinds
                .iter()
                .any(|kind| matches!(kind, EventKind::FastPathHit { .. }))
        );
        assert!(kinds.contains(&EventKind::TaskCompleted));
        // The attempted-and-declined side of the fast path is surfaced,
        // not swallowed (v8): no intent matched, the loop took over.
        assert!(kinds.contains(&EventKind::FastPathDeclined {
            reason: "no-intent-match".to_owned()
        }));
        // So is the authority that fixed the fake provider's
        // capabilities: its own table.
        assert!(kinds.contains(&EventKind::CapabilitiesResolved {
            model: "deepseek-flash".to_owned(),
            source: "provider-table".to_owned()
        }));
    }

    #[tokio::test]
    async fn a_persisted_run_also_persists_its_event_stream() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace;
        config.db_path = base.path().join("loom.db");
        config.persist = true;
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("done"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Say hello and stop.",
                &sink,
                &Default::default(),
            )
            .await
            .expect("task");
        assert_eq!(summary.model_requests, 1);
        let _ = drain(&mut receiver);

        // The durable stream carries the same run: task lifecycle,
        // authoritative completion, and the engine's idle marker, in
        // emission order with contiguous per-run sequences.
        use loom_kernel::store::EventStore;
        let store = loom_store::SqliteStore::open(base.path().join("loom.db")).expect("store");
        let events = store.load_events(&session.id).expect("events");
        assert!(events.len() >= 4, "expected a full stream, got {events:?}");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match &event.kind {
                EventKind::TaskStarted => "task_started",
                EventKind::ModelCompleted { .. } => "model_completed",
                EventKind::TaskCompleted => "task_completed",
                EventKind::AwaitingInput { .. } => "awaiting_input",
                EventKind::FastPathDeclined { .. } => "fast_path_declined",
                EventKind::CapabilitiesResolved { .. } => "capabilities_resolved",
                EventKind::HarnessNotice { .. } => "harness_notice",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds.first(), Some(&"task_started"));
        assert!(kinds.contains(&"model_completed"));
        assert!(kinds.contains(&"task_completed"));
        assert_eq!(kinds.last(), Some(&"awaiting_input"));
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, index as u64, "one run numbers contiguously");
        }
    }

    #[tokio::test]
    async fn capability_resolution_surfaces_once_per_engine() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("done"),
                    FakeProvider::end_turn_script("done again"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        engine
            .run_task(&mut session, "Say hello.", &sink, &Default::default())
            .await
            .expect("first task");
        engine
            .run_task(&mut session, "Say it again.", &sink, &Default::default())
            .await
            .expect("second task");

        let kinds = drain(&mut receiver);
        let resolutions = kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::CapabilitiesResolved { .. }))
            .count();
        assert_eq!(
            resolutions, 1,
            "one engine, one capability resolution — not one per task"
        );
    }

    #[tokio::test]
    async fn a_solver_decline_names_the_intent() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        // The analyzer matches a rename intent, but the target symbol
        // does not exist: the solver declines, and the decline carries
        // the intent kind rather than a bare no-match.
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("done"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Rename missing_symbol to renamed_symbol",
                &sink,
                &Default::default(),
            )
            .await
            .expect("agent task");

        assert_eq!(summary.model_requests, 1, "the loop took over");
        let kinds = drain(&mut receiver);
        assert!(kinds.contains(&EventKind::FastPathDeclined {
            reason: "intent-rename-unsolved".to_owned()
        }));
        // The fixture is untouched — a declined fast path never edits.
        let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
        assert!(lib.contains("pub fn parse_opts() -> u32"));
    }

    /// One script: an assistant turn that asks for two identical
    /// `run_command` calls (same program, same arguments).
    fn two_identical_commands_script() -> Vec<Result<ModelEvent, ModelError>> {
        use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
        let call = || ToolCall {
            id: ToolCallId::try_from(format!("call-{}", std::process::id())).unwrap(),
            name: "run_command".to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({
                "program": "echo",
                "args": ["hi"],
            }))
            .expect("arguments"),
        };
        vec![Ok(ModelEvent::Completed(ModelResponse {
            message: AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call(), call()],
                stop_reason: StopReason::ToolCalls,
            },
            usage: Usage::default(),
        }))]
    }

    #[tokio::test]
    async fn harness_notices_dedupe_within_a_batch() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        // The manifest opts `echo` out of the OS sandbox: the loudest
        // escalation a rule can grant, and exactly the under-the-hood
        // deviation the v8 notices exist to surface.
        std::fs::create_dir_all(workspace.join(".loom")).expect("loom dir");
        std::fs::write(
            workspace.join(".loom/commands.toml"),
            "[[command]]\nprogram = \"echo\"\nsandbox = false\n",
        )
        .expect("manifest");
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        // A tool turn plus the closing turn.
        config.max_turns = 2;
        // Two identical unconfined commands in one turn: the
        // sandbox-passthrough observation is a property of the batch,
        // not of each call — one event, not two.
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    two_identical_commands_script(),
                    FakeProvider::end_turn_script("done"),
                ]))),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Check the toolchain twice.",
                &sink,
                &Default::default(),
            )
            .await
            .expect("agent task");

        assert_eq!(summary.model_requests, 2, "tool turn plus final turn");
        let kinds = drain(&mut receiver);
        let completed = kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::ToolCompleted { .. }))
            .count();
        assert_eq!(completed, 2, "both calls ran");
        let notices = kinds
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::HarnessNotice { category, .. } if category == "sandbox.passthrough"
                )
            })
            .count();
        assert_eq!(
            notices, 1,
            "identical notices collapse to one event per batch"
        );
    }

    #[tokio::test]
    async fn fast_paths_can_be_disabled_by_override() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = rename_fixture(base.path());
        let mut config = test_config();
        config.workspace = workspace.clone();
        config.db_path = base.path().join("loom.db");
        let engine = Engine::open_with(
            config,
            EngineOverrides {
                provider: Some(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("done"),
                ]))),
                fast_paths: Some(false),
                ..EngineOverrides::default()
            },
        )
        .expect("engine");

        let (sink, mut receiver) = loom_kernel::event::ChannelEventSink::new();
        let mut session = engine.new_session();
        let summary = engine
            .run_task(
                &mut session,
                "Rename parse_opts to parse_options",
                &sink,
                &Default::default(),
            )
            .await
            .expect("agent task");

        // The same instruction the fast path would solve for free runs
        // the loop instead: the override is honored.
        assert_eq!(summary.model_requests, 1);
        let kinds = drain(&mut receiver);
        assert!(
            !kinds
                .iter()
                .any(|kind| matches!(kind, EventKind::FastPathHit { .. }))
        );
        // And the workspace was left for the agent to edit.
        let lib = std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib");
        assert!(lib.contains("parse_opts"));
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
    async fn recorder_observes_the_engine_lifecycle_through_a_fanout() {
        // B5's consumer shape: fan a StatusRecorder clone out next to a
        // rendering channel; polling the retained handle must see the
        // task run and the engine's idle marker flip it back to Idle.
        let engine = fake_engine(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
            "done",
        )]));
        let recorder = loom_kernel::event::StatusRecorder::new();
        let (channel, _receiver) = loom_kernel::event::ChannelEventSink::new();
        let sink = loom_kernel::event::FanoutSink::new()
            .with(recorder.clone())
            .with(channel);
        let mut session = engine.new_session();

        engine
            .run_task(&mut session, "anything", &sink, &Default::default())
            .await
            .expect("task completes");

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.phase, loom_kernel::event::TaskPhase::Idle);
        assert_eq!(snapshot.session_id, Some(session.id));
        assert_eq!(
            snapshot.task_id,
            session.tasks.last().map(|record| record.id),
            "the idle marker's envelope names the ended task"
        );
        assert_eq!(snapshot.turn, Some(1));
        assert_eq!(snapshot.attempt, None);
        assert_eq!(snapshot.in_flight_tools, Vec::<String>::new());
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

    #[tokio::test]
    async fn open_with_skips_credential_resolution_for_an_overridden_provider() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut config = test_config();
        config.provider = "replay".to_owned(); // no adapter, no credential
        config.workspace = workspace;
        config.db_path = base.path().join("loom.db");

        // Only the override path can succeed here: building "replay"
        // would fail with UnknownProvider before any credential work.
        let engine = Engine::open_with(
            config,
            EngineOverrides::default().with_provider(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("overridden"),
            ]))),
        )
        .expect("no credential is resolved when the provider is overridden");
        let mut session = engine.new_session();
        let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
        let summary = engine
            .run_task(&mut session, "task", &sink, &Default::default())
            .await
            .expect("task runs on the overridden provider");
        assert_eq!(summary.turns, 1);
    }

    #[tokio::test]
    async fn a_provider_hook_wraps_whichever_provider_results() {
        let base = tempfile::tempdir().expect("tempdir");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut config = test_config();
        config.provider = "replay".to_owned();
        config.workspace = workspace;
        config.db_path = base.path().join("loom.db");

        let engine = Engine::open_with(
            config,
            EngineOverrides::default()
                .with_provider(Box::new(FakeProvider::scripted(vec![
                    FakeProvider::end_turn_script("original"),
                ])))
                .with_provider_hook(Arc::new(|_built: Box<dyn ModelProvider>| {
                    // The hook decides what runs: a different scripted
                    // answer proves the replacement took effect.
                    Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
                        "hooked",
                    )]))
                })),
        )
        .expect("open");
        let mut session = engine.new_session();
        let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
        engine
            .run_task(&mut session, "task", &sink, &Default::default())
            .await
            .expect("task runs on the hooked provider");
        let answer = session.transcript.iter().rev().find_map(|item| match item {
            loom_kernel::transcript::Item::Assistant(message) => Some(message.content.clone()),
            _ => None,
        });
        assert_eq!(answer.as_deref(), Some("hooked"));
    }

    /// The provider gate for reasoning elision (roadmap item 12): on
    /// for GLM (drops only a cache benefit) and Anthropic (never
    /// forwards reasoning on replay), off for the OpenAI-vocabulary
    /// backends where a tool-bearing request must echo
    /// `reasoning_content` verbatim (DeepSeek 400s otherwise).
    #[test]
    fn prune_config_gates_reasoning_elision_by_provider() {
        let glm = prune_config_for(loom_providers::GLM_PROVIDER_NAME);
        assert_eq!(glm.elide_reasoning, Some(8));

        let anthropic = prune_config_for(loom_providers::ANTHROPIC_PROVIDER_NAME);
        assert_eq!(anthropic.elide_reasoning, Some(8));

        let deepseek = prune_config_for(loom_providers::DEEPSEEK_PROVIDER_NAME);
        assert_eq!(
            deepseek.elide_reasoning, None,
            "DeepSeek rejects replay without reasoning_content"
        );

        let openrouter = prune_config_for(loom_providers::OPENROUTER_PROVIDER_NAME);
        assert_eq!(
            openrouter.elide_reasoning, None,
            "unverified OpenAI-wire behavior stays off"
        );

        // Everything else keeps the kernel defaults.
        assert_eq!(glm.elide_turns, PruneConfig::default().elide_turns);
        assert_eq!(glm.trigger_percent, PruneConfig::default().trigger_percent);
    }

    /// Roadmap item 8 phase B: the kernel-owned `todo` tool is a
    /// full-registry lever. The full engine exposes it; the bare-loop
    /// arm — the basic-harness baseline — must keep excluding it
    /// (bare = read/list/write/edit/shell).
    #[test]
    fn todo_tool_is_exposed_in_the_full_registry_and_excluded_from_bare() {
        let built = build_registry(&CommandPolicy::default()).expect("built-in registry");
        let full: Vec<&str> = built.specs().map(|spec| spec.name.as_str()).collect();
        assert!(
            full.contains(&"todo"),
            "the full registry must expose the kernel todo tool: {full:?}"
        );

        let bare_registry = bare_registry(built);
        let bare: Vec<&str> = bare_registry
            .specs()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(!bare.contains(&"todo"), "the bare arm must not: {bare:?}");
        assert_eq!(
            bare,
            [
                "read_file",
                "list_dir",
                "write_file",
                "edit_file",
                "run_command"
            ],
            "the bare arm is exactly the five primitives, in order"
        );
    }
}
