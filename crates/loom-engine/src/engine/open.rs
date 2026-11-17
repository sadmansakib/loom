//! Opening an engine: credential resolution, provider construction, the
//! built-in tool registry, scout and routing wiring, and the async open
//! paths.

use std::sync::Arc;

use loom_kernel::agent::Agent;
use loom_kernel::context::{
    AgentConfig, ContextCompiler, DefaultContextCompiler, PruningContextCompiler,
};
use loom_kernel::model::ModelProvider;
use loom_kernel::tool::ToolRegistry;
use loom_providers::{CUSTOM_PROVIDER_NAME, OPENROUTER_PROVIDER_NAME, ProviderCatalog};
use loom_store::SqliteStore;
use loom_tools::policy::CommandPolicy;
use loom_tools::{BackgroundJobs, CommandStatusTool, RunBackgroundTool};

use crate::auth::{self, KeychainBackend, SqliteAuthStore};
use crate::scout::{ScoutConfig, ScoutTool};

use super::assembly::{
    bare_registry, build_registry, build_stall_escalation_agent, build_tier_agents, compose_scout,
    default_system_prompt, read_only_registry,
};
use super::budgets::prune_config_for;
use super::config::{kernel_effort, reasoning_schedule_from_env};
use super::{Engine, EngineConfig, EngineError, EngineOverrides, ProviderHook, RoutedAgent};

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
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] under the same conditions as
    /// [`Engine::open_with`].
    pub fn open(config: EngineConfig) -> Result<Self, EngineError> {
        Self::open_with(config, EngineOverrides::default())
    }

    /// [`Engine::open`] with embedder overrides
    /// ([`EngineOverrides`]): a handed-in provider, a provider-wrapping
    /// hook, a custom tool registry, or a custom context compiler. Every
    /// `None` field keeps the production choice; see the overrides type
    /// for the exact semantics of each.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] under the same conditions as
    /// [`Engine::open_with_extra_tools`].
    pub fn open_with(
        config: EngineConfig,
        overrides: EngineOverrides,
    ) -> Result<Self, EngineError> {
        Self::open_with_extra_tools(
            config,
            overrides,
            loom_tools::mcp::McpAttachment::default(),
            loom_tools::lsp::LspAttachment::default(),
        )
    }

    /// [`Engine::open_with`] plus an MCP attachment (slice D) and an LSP
    /// attachment (slice E): eager meta-tools and `always_load` pins
    /// register after every built-in opt-in surface; the deferred
    /// surface and facts menus ride along. Same failure semantics as
    /// any other registration.
    ///
    /// Public for embedders that own server discovery and lifecycle —
    /// loom-acp's editor-forwarded servers (roadmap item 37 tail)
    /// attach here after running them through the same hygiene
    /// pipeline (`loom_tools::mcp::attach_forwarded`).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Workspace`] when the workspace cannot be
    /// canonicalized, [`EngineError::Store`] or [`EngineError::Auth`] when
    /// credentials cannot be resolved, [`EngineError::UnknownProvider`],
    /// [`EngineError::CatalogMissing`], or [`EngineError::MissingBaseUrl`]
    /// when the provider selection is incomplete, [`EngineError::Provider`],
    /// [`EngineError::UnknownModel`], [`EngineError::ModelWindowTooSmall`],
    /// or [`EngineError::ModelWithoutReasoning`] when the provider/model
    /// fails loud validation, [`EngineError::CommandPolicy`] when the
    /// command-policy manifest is invalid, [`EngineError::RegisterTool`]
    /// when a built-in tool cannot register, and [`EngineError::Session`]
    /// when the session store cannot be opened.
    #[allow(clippy::too_many_lines)] // linear, order-sensitive construction; splitting hides the sequencing
    pub fn open_with_extra_tools(
        mut config: EngineConfig,
        overrides: EngineOverrides,
        mcp: loom_tools::mcp::McpAttachment,
        lsp: loom_tools::lsp::LspAttachment,
    ) -> Result<Self, EngineError> {
        let workspace =
            std::fs::canonicalize(&config.workspace).map_err(|source| EngineError::Workspace {
                path: config.workspace.clone(),
                source,
            })?;
        // The engine keeps the live server handles for the fast-path
        // rename fallback (item 34); tools and menu flow on below.
        let lsp_servers = lsp.servers;

        let provider = resolve_provider(
            &config,
            overrides.provider,
            overrides.credential,
            overrides.provider_hook,
        )?;
        // The resolution facts the trace surfaces once per engine (v8):
        // which authority — catalog row, provider table, or recording —
        // fixed this model's capabilities.
        let resolution = crate::provider::resolution_source(&config, provider.as_ref());

        // One policy drives both the tool registry and the fast paths'
        // verification runs, so a manifest rule changes them together.
        let policy = CommandPolicy::from_manifest(&workspace)?;
        let registry = if let Some(registry) = overrides.registry {
            registry
        } else {
            let built = build_registry(
                &policy,
                config.budgets,
                config.autofix,
                overrides.ask.as_ref(),
                overrides.write_approval.as_ref(),
            )?;
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
        };
        // Roadmap item 19: the read-only scout. Opt-in via
        // `EngineConfig::scout` (or injected through `overrides.scout`
        // for offline tests); the bare-loop baseline never carries it,
        // and a scout is never composed for a bare arm.
        let scout_agent: Option<Arc<Agent>> = match overrides.scout {
            Some(agent) => Some(agent),
            None => match config.scout {
                Some(ref cfg) if !overrides.bare_loop => {
                    Some(Arc::new(compose_scout(cfg, &config, &workspace, &policy)?))
                }
                _ => None,
            },
        };
        let mut registry = registry;
        if let Some(agent) = scout_agent {
            let cfg = config.scout.as_ref();
            let timeout = cfg.map_or(120, |cfg| cfg.timeout_secs);
            let mut tool = ScoutTool::new(agent, timeout);
            if let Some(cfg) = cfg {
                tool = tool.with_model_label(cfg.model.clone());
                // Escalation target (multi-model cascade): a second child
                // agent on the strong model, same turns/timeout budgets,
                // no escalation of its own — one hop, never a chain.
                if let Some(esc) = cfg.escalation.as_ref() {
                    let mut esc_cfg = ScoutConfig::new(esc.provider.clone(), esc.model.clone());
                    esc_cfg.max_turns = cfg.max_turns;
                    esc_cfg.timeout_secs = cfg.timeout_secs;
                    let esc_agent =
                        Arc::new(compose_scout(&esc_cfg, &config, &workspace, &policy)?);
                    tool = tool.with_escalation(esc_agent, esc.model.clone());
                }
            }
            register_tool(&mut registry, "scout", Box::new(tool))?;
        }
        // Roadmap item 13: background commands. Opt-in via
        // `EngineConfig::background_commands`; the bare-loop arm never
        // carries them. The job registry is engine-owned — detached jobs
        // outlive single tasks, and the last drop reaps whatever still
        // runs (the same process-group kill a foreground timeout
        // performs). Completion notices wait in the registry until a
        // running task's watcher steers them in at a turn boundary.
        let background_jobs = if config.background_commands && !overrides.bare_loop {
            let jobs = BackgroundJobs::default();
            register_tool(
                &mut registry,
                "run_background",
                Box::new(RunBackgroundTool::new(policy.clone(), jobs.clone())),
            )?;
            register_tool(
                &mut registry,
                "command_status",
                Box::new(CommandStatusTool::new(jobs.clone())),
            )?;
            Some(jobs)
        } else {
            None
        };
        // Roadmap item 18: tree-sitter symbol tools. Opt-in via
        // `EngineConfig::symbols`; the bare-loop arm never carries them.
        // Roadmap item 30: the symbol tools serve Rust, Python, and
        // TypeScript/TSX only, so an opted-in flag still prunes them from
        // a workspace whose deterministic facts scan finds none of those
        // languages — schema bytes saved with zero wrong-tool risk (the
        // tools cannot serve absent languages). The scan here is the same
        // one the loop performs per task; this is the menu decision, made
        // once at open before turn 1.
        let symbol_languages = ["Rust", "Python", "TypeScript"];
        let symbols_served = crate::repo::scan(&workspace)
            .languages
            .iter()
            .any(|language| symbol_languages.contains(&language.as_str()));
        if config.symbols && !overrides.bare_loop && symbols_served {
            register_tool(
                &mut registry,
                "symbols_overview",
                Box::new(loom_tools::SymbolsOverviewTool::default().with_budgets(config.budgets)),
            )?;
            register_tool(
                &mut registry,
                "replace_symbol",
                Box::new(loom_tools::ReplaceSymbolTool::default().with_budgets(config.budgets)),
            )?;
        }
        // Iteration 4: structured git tools. Same opt-in shape as the
        // symbol tools; the bare-loop arm never carries them.
        if config.git && !overrides.bare_loop {
            for (name, tool) in [
                (
                    "git_status",
                    Box::new(loom_tools::GitStatusTool) as Box<dyn loom_kernel::tool::Tool>,
                ),
                (
                    "git_diff",
                    Box::new(loom_tools::GitDiffTool) as Box<dyn loom_kernel::tool::Tool>,
                ),
                (
                    "git_log",
                    Box::new(loom_tools::GitLogTool) as Box<dyn loom_kernel::tool::Tool>,
                ),
            ] {
                register_tool(&mut registry, name, tool)?;
            }
        }
        // Iteration 5: on-demand skills. Same opt-in shape as the symbol
        // tools; the facts block gains the index menu only when the tool
        // that serves the bodies is also present.
        if config.skills && !overrides.bare_loop {
            register_tool(
                &mut registry,
                "load_skill",
                Box::new(loom_tools::LoadSkillTool::default().with_budgets(config.budgets)),
            )?;
        }
        // Slice D (iteration 6): MCP servers over stdio. Opt-in via
        // `EngineConfig::mcp`; the bare-loop arm never carries them.
        // Attach is eager in D1 and happens in the async open path
        // (`open_async_with`): spawning a server and handshaking is
        // async I/O, so this sync constructor receives the already-
        // attached, hygiene-checked, hash-pinned tool set. Attach never
        // fails the engine open — a bad manifest, lockfile, or server
        // degrades that server to zero tools (fail closed), and the
        // frozen default registry is untouched while the flag is off.
        for tool in mcp.tools {
            registry
                .register_arc(tool)
                .map_err(|error| EngineError::RegisterTool {
                    tool: "mcp",
                    message: error.to_string(),
                })?;
        }
        // Slice E (iteration 6): LSP servers over stdio. Opt-in via
        // `EngineConfig::lsp`; the bare-loop arm never carries them.
        // Like MCP, spawning and handshaking is async I/O, so the async
        // open path computes the attachment and hands it in. Attach
        // never fails the engine open — a missing binary or a failed
        // spawn degrades that server to nothing (warn + skip), and an
        // invalid `.loom/lsp.toml` degrades all of LSP (fail closed).
        for tool in lsp.tools {
            register_tool(&mut registry, "lsp", tool)?;
        }
        if config.read_only && !overrides.bare_loop {
            registry = read_only_registry(registry);
        }
        let system_prompt = config
            .system_prompt
            .take()
            .unwrap_or_else(default_system_prompt);
        // Stage-1 repository facts (2E, ADR-015): the composition root owns the
        // I/O; the loop scans once per task through this source. The
        // bare-loop arm attaches no source — the empty block is the
        // baseline's point.
        // Menus advertise callable tools, so a read-only run hides the MCP
        // and LSP menus rather than naming tools its registry removed.
        let read_only = config.read_only && !overrides.bare_loop;
        let facts = (!overrides.bare_loop).then(|| {
            crate::repo::FsFactsSource::new(workspace.clone())
                .with_skills(config.skills && !overrides.bare_loop)
                .with_lessons(config.lessons && !overrides.bare_loop)
                .with_mcp_menu(if read_only { None } else { mcp.menu.clone() })
                .with_lsp_menu(if read_only { None } else { lsp.menu.clone() })
        });
        let mut operator_ops: Option<loom_kernel::context::PruneOpsHandle> = None;
        let compiler: Box<dyn ContextCompiler> = if let Some(compiler) = overrides.compiler {
            compiler
        } else {
            // Token estimates use a real tokenizer — the same estimator
            // feeds the census, the budget check, and the pruning math.
            // The family follows the provider: Anthropic counts tokens
            // differently from the OpenAI-vocabulary backends, and its
            // 1M window only holds if the estimate errs high.
            let estimator: Arc<dyn loom_kernel::context::TokenEstimator> =
                crate::estimate::estimator_for(&config.provider);
            // The bare-loop arm compiles plain append-only context —
            // no pruning, no checkpoints — so a bare run measures the
            // levers the full chain adds. It also has no operator
            // pin/drop sets: there is nothing to prune.
            if overrides.bare_loop {
                Box::new(DefaultContextCompiler::with_estimator(estimator))
            } else {
                let pruning = PruningContextCompiler::new(
                    Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                    config
                        .prune
                        .unwrap_or_else(|| prune_config_for(&config.provider)),
                )
                .with_estimator(estimator);
                operator_ops = Some(pruning.operator_handle());
                Box::new(pruning)
            }
        };
        // The bare-loop arm also disarms the LLM handoff: the plain
        // compiler never freezes checkpoints, so a handoff attempt there
        // could only spend a call and fail. Both levers flip together.
        let mut agent_config = AgentConfig::new(workspace.clone(), system_prompt);
        if overrides.bare_loop {
            agent_config.handoff_compaction = false;
        }
        // Roadmap item 12: the operator's context-editing hint rides
        // the agent config verbatim (default None sends nothing).
        agent_config.context_editing = config.context_editing.take();
        // Roadmap item 9: per-turn reasoning effort. The fixed level
        // maps from the operator's provider-level flag; the schedule
        // is env-seamed so funded sweeps vary it without touching the
        // frozen container script (the item 20.2 seam pattern).
        agent_config.reasoning_effort = kernel_effort(config.reasoning_effort);
        agent_config.reasoning_schedule = reasoning_schedule_from_env();
        // Routing slice zero (roadmap item 9): build the tier agents
        // before the main agent consumes the registry. Injection wins
        // over composition exactly like the scout seam; the bare-loop
        // arm is a baseline and never carries routing. Production
        // composition resolves the (same-provider) credential once and
        // builds per-tier agents sharing this registry and agent
        // config — the effort schedule and system prompt ride along.
        let (routing_easy, routing_hard) = match overrides.routed {
            Some(pairs) => {
                let mut easy: Option<RoutedAgent> = None;
                let mut hard: Option<RoutedAgent> = None;
                for (tier, agent) in pairs {
                    let label = format!("injected-{}", tier.as_str());
                    match tier {
                        crate::routing::Tier::Easy if easy.is_none() => {
                            easy = Some(RoutedAgent {
                                model: label,
                                agent,
                            });
                        }
                        crate::routing::Tier::Hard if hard.is_none() => {
                            hard = Some(RoutedAgent {
                                model: label,
                                agent,
                            });
                        }
                        crate::routing::Tier::Easy
                        | crate::routing::Tier::Medium
                        | crate::routing::Tier::Hard => {}
                    }
                }
                (easy, hard)
            }
            None => match config.routing.as_ref().filter(|routing| routing.active()) {
                Some(routing) if !overrides.bare_loop => {
                    build_tier_agents(routing, &config, &registry, &agent_config)?
                }
                _ => (None, None),
            },
        };
        // Escalation-by-fork (roadmap item 36): composed like a tier
        // agent — same registry, same agent config, one model. The
        // bare-loop arm is a baseline and never carries escalation.
        let stall_escalation = match overrides.stall_escalation {
            Some((model, agent)) => Some(RoutedAgent { agent, model }),
            None => match config.stall_escalation_model.as_deref() {
                Some(model) if !overrides.bare_loop => Some(build_stall_escalation_agent(
                    model,
                    &config,
                    &registry,
                    &agent_config,
                )?),
                _ => None,
            },
        };
        let mut agent = Agent::new(provider, compiler, registry, agent_config, config.model)
            .with_max_turns(config.max_turns)
            .with_turn_budget(config.turn_budget)
            .with_loop_shaping(!overrides.bare_loop);
        if let Some(facts) = facts {
            agent = agent.with_facts_source(Box::new(facts));
        }
        // The deferred MCP surface has no per-tool read-only filter, so a
        // read-only run must not attach it: a queued mutating tool would
        // bypass the registry filter above.
        if let Some(surface) = mcp.surface
            && !config.read_only
        {
            agent = agent.with_surface(surface);
        }

        // Durable sessions and artifact bodies (ADR-008, ADR-012): the same
        // database the frontend chose for accounts. `--no-save` leaves the
        // run in memory only, so nothing spills and `artifact_read` has no
        // store.
        let (db_path, events) = if config.persist {
            let store = SqliteStore::open(&config.db_path).map_err(EngineError::Session)?;
            let artifacts = SqliteStore::open(&config.db_path).map_err(EngineError::Session)?;
            // The event-stream persistence seam shares the database
            // (schema v2's `events` table) and the same write cadence
            // as the transcript: append-on-emit, best-effort — a store
            // failure counts drops and never fails the task.
            let events = SqliteStore::open(&config.db_path).map_err(EngineError::Session)?;

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
                tracing::warn!(%error, "artifact gc skipped");
            }

            agent = agent.with_store(Box::new(store));
            agent = agent.with_artifacts(Some(Arc::new(artifacts)));
            (
                Some(config.db_path.clone()),
                Some(Arc::new(events) as Arc<dyn loom_kernel::store::EventStore>),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            agent,
            background: background_jobs,
            lsp_servers,
            routing_easy,
            routing_hard,
            stall_escalation,
            db_path,
            workspace,
            policy,
            // The bare-loop arm measures a basic harness, and a basic
            // harness does not short-circuit mechanical tasks — the
            // fast path is a Loom lever, so it flips with the others.
            fast_paths: overrides.fast_paths.unwrap_or(true) && !overrides.bare_loop,
            resolution: Some(resolution),
            notices_flushed: std::sync::OnceLock::new(),
            events,
            mcp_servers: mcp.servers,
            prune_ops: operator_ops,
        })
    }

    /// Per-server MCP attach outcome from open time (manifest order):
    /// `(server name, connected)`. A `false` row means that server
    /// failed spawn, handshake, or `tools/list` and contributed no
    /// tools; other servers proceeded. Empty when MCP is off or no
    /// manifest declares servers.
    #[must_use]
    pub fn mcp_servers(&self) -> &[(String, bool)] {
        &self.mcp_servers
    }

    /// Pins a tool result (`/context` `p`): no later prune pass stubs
    /// it. Takes effect at the next task's first decision pass. A no-op
    /// when the loop has no pruning compiler.
    pub fn pin_context_call(&self, call_id: impl Into<String>) {
        if let Some(ops) = &self.prune_ops {
            ops.pin(call_id);
        }
    }

    /// Drops a tool result (`/context` `x`): the next decision pass
    /// stubs it even below the window trigger. A no-op when the loop has
    /// no pruning compiler.
    pub fn drop_context_call(&self, call_id: impl Into<String>) {
        if let Some(ops) = &self.prune_ops {
            ops.drop(call_id);
        }
    }

    /// Snapshot of the operator's pin/drop sets, for the `/context`
    /// inspector. `None` when the loop has no pruning compiler.
    #[must_use]
    pub fn context_ops(&self) -> Option<loom_kernel::context::PruneOps> {
        self.prune_ops
            .as_ref()
            .map(loom_kernel::context::PruneOpsHandle::snapshot)
    }

    /// Resolves the config's model catalog exactly the way
    /// [`Engine::open_async`] does — the sequencing every frontend (and the
    /// worktree orchestrator, which opens N engines from one config) should
    /// reuse instead of re-implementing. A config that already carries a
    /// catalog, or names a provider that needs none, skips the network.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when a catalog-driven provider's catalog
    /// cannot be fetched or parsed.
    pub async fn resolve_catalog(config: EngineConfig) -> Result<EngineConfig, EngineError> {
        Self::resolve_catalog_with_credential(config, None).await
    }

    /// Resolves model metadata, optionally reusing a credential already
    /// obtained by a frontend during model discovery.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when required provider metadata cannot be
    /// resolved or a catalog cannot be loaded.
    pub async fn resolve_catalog_with_credential(
        config: EngineConfig,
        credential: Option<&loom_providers::SecretString>,
    ) -> Result<EngineConfig, EngineError> {
        // `custom` is described entirely by user-supplied URLs: the
        // chat base URL is mandatory and the catalog comes from the
        // models URL (explicit or defaulted from the base). Its cache
        // is keyed to the URL, and a fetch failure is fatal — the
        // adapter has no built-in table to fall back to.
        let config = if config.catalog.is_none() {
            // `custom` is described entirely by user-supplied URLs; a
            // registry name (`loom provider add`) carries them in the
            // registry instead, with explicit flags winning over both.
            let registry = crate::registry::ProviderRegistry::load(&config.db_path);
            let named = registry.get(&config.provider);
            let is_custom = config.provider == CUSTOM_PROVIDER_NAME
                || named.is_some()
                || config.base_url.is_some();
            if is_custom {
                let Some(base_url) = config
                    .base_url
                    .clone()
                    .or_else(|| named.map(|entry| entry.base_url.clone()))
                else {
                    return Err(EngineError::MissingBaseUrl {
                        provider: config.provider.clone(),
                    });
                };
                let models_url = config
                    .models_url
                    .clone()
                    .or_else(|| named.and_then(|entry| entry.models_url.clone()))
                    .unwrap_or_else(|| format!("{}/models", base_url.trim_end_matches('/')));
                let catalog_credential = credential
                    .filter(|_| config.provider == CUSTOM_PROVIDER_NAME || named.is_some());
                let catalog = crate::catalog::load_custom_with_credential(
                    &config.db_path,
                    &config.provider,
                    config.account.as_deref(),
                    &models_url,
                    catalog_credential,
                )
                .await?;
                EngineConfig {
                    catalog: Some(ProviderCatalog::Custom(catalog)),
                    ..config
                }
            } else {
                config
            }
        } else {
            config
        };
        // A fresh OpenRouter catalog is fetched for an OpenRouter run;
        // other providers may use an existing cache for capability metadata,
        // but must not resolve OpenRouter credentials as a side effect.
        let config = if config.catalog.is_none() {
            let is_openrouter = config.provider == OPENROUTER_PROVIDER_NAME;
            if is_openrouter {
                let account = config.account.clone();
                let base_url = config.base_url.clone();
                match crate::catalog::load_with_credential(
                    &config.db_path,
                    OPENROUTER_PROVIDER_NAME,
                    account.as_deref(),
                    base_url.as_deref(),
                    credential,
                )
                .await
                {
                    Ok(catalog) => EngineConfig {
                        catalog: Some(ProviderCatalog::OpenRouter(catalog)),
                        ..config
                    },
                    Err(error) => return Err(error),
                }
            } else if let Some(catalog) = crate::catalog::cached_openrouter(&config.db_path) {
                // Capability metadata from another provider's catalog is
                // useful when already cached, but opening this provider must
                // never resolve the unrelated OpenRouter keychain entry.
                EngineConfig {
                    catalog: Some(ProviderCatalog::OpenRouter(catalog)),
                    ..config
                }
            } else {
                config
            }
        } else {
            config
        };
        Ok(config)
    }
}
impl Engine {
    /// Opens the engine, fetching the `OpenRouter` catalog first when one is
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
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] under the same conditions as
    /// [`Engine::open_with`].
    pub async fn open_async(config: EngineConfig) -> Result<Self, EngineError> {
        Self::open_async_with(config, EngineOverrides::default()).await
    }

    /// [`Engine::open_async`] with embedder overrides
    /// ([`EngineOverrides`]) — the catalog sequencing of the async open
    /// composed with the substitution points of [`Engine::open_with`].
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when catalog resolution fails (see
    /// [`Engine::resolve_catalog`]) or under the conditions of
    /// [`Engine::open_with_extra_tools`].
    pub async fn open_async_with(
        config: EngineConfig,
        mut overrides: EngineOverrides,
    ) -> Result<Self, EngineError> {
        if overrides.provider.is_none() && overrides.credential.is_none() {
            let store = SqliteAuthStore::open(&config.db_path).map_err(EngineError::Store)?;
            let secrets = KeychainBackend::default();
            let credential = auth::resolve(
                &store,
                &secrets,
                &config.provider,
                config.account.as_deref(),
                |name| std::env::var(name).ok(),
            )?;
            overrides.credential = Some(credential.secret);
        }
        let config =
            Self::resolve_catalog_with_credential(config, overrides.credential.as_ref()).await?;
        // Slice D2: deferred MCP attach. Spawning stdio servers and
        // handshaking is async I/O, so it lives here — the sync
        // constructor receives the attachment (eager meta-tools +
        // always_load pins, facts menu, session surface). A missing
        // manifest attaches nothing; a bad server degrades to zero tools
        // for that server (fail closed), never a failed open.
        let mcp = if config.mcp && !overrides.bare_loop {
            let workspace = std::fs::canonicalize(&config.workspace)
                .unwrap_or_else(|_| config.workspace.clone());
            loom_tools::mcp::attach_mcp(&workspace, config.budgets).await
        } else {
            loom_tools::mcp::McpAttachment::default()
        };
        // Slice E1: LSP attach. Same async-open reasoning as MCP; a
        // missing binary skips that server, an invalid manifest fails
        // all of LSP closed, and the frozen default registry is
        // untouched while the flag is off.
        let lsp = if config.lsp && !overrides.bare_loop {
            let workspace = std::fs::canonicalize(&config.workspace)
                .unwrap_or_else(|_| config.workspace.clone());
            loom_tools::lsp::attach_lsp(&workspace, config.budgets).await
        } else {
            loom_tools::lsp::LspAttachment::default()
        };
        Engine::open_with_extra_tools(config, overrides, mcp, lsp)
    }

    /// [`Engine::open_async_with`] plus operator-forwarded MCP servers
    /// (roadmap item 37 tail; see `docs/acp-tails-design.md`).
    ///
    /// The forwarded servers run through the same spawn/hygiene
    /// pipeline as the manifest (`attach_forwarded`) with every
    /// surviving tool loaded eagerly — the editor is the tool picker,
    /// so there is no deferred catalog for them. They compose with,
    /// never replace, the manifest attachment. A caller is expected to
    /// gate this on an explicit operator choice; the engine does not
    /// second-guess the list it is handed.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when catalog resolution fails (see
    /// [`Engine::resolve_catalog`]) or under the conditions of
    /// [`Engine::open_with_extra_tools`].
    pub async fn open_async_with_forwarded_mcp(
        config: EngineConfig,
        overrides: EngineOverrides,
        forwarded: std::collections::BTreeMap<String, loom_tools::mcp::McpServerDecl>,
    ) -> Result<Self, EngineError> {
        let config = Self::resolve_catalog(config).await?;
        let mut mcp = if config.mcp && !overrides.bare_loop {
            let workspace = std::fs::canonicalize(&config.workspace)
                .unwrap_or_else(|_| config.workspace.clone());
            loom_tools::mcp::attach_mcp(&workspace, config.budgets).await
        } else {
            loom_tools::mcp::McpAttachment::default()
        };
        if !forwarded.is_empty() {
            let workspace = std::fs::canonicalize(&config.workspace)
                .unwrap_or_else(|_| config.workspace.clone());
            let extra =
                loom_tools::mcp::attach_forwarded(&forwarded, &workspace, config.budgets).await;
            // Forwarded servers are eager-only: no menu or surface of
            // their own to merge, just the tools.
            mcp.tools.extend(extra.tools);
        }
        // Slice E1: LSP attach. Same async-open reasoning as MCP; a
        // missing binary skips that server, an invalid manifest fails
        // all of LSP closed, and the frozen default registry is
        // untouched while the flag is off.
        let lsp = if config.lsp && !overrides.bare_loop {
            let workspace = std::fs::canonicalize(&config.workspace)
                .unwrap_or_else(|_| config.workspace.clone());
            loom_tools::lsp::attach_lsp(&workspace, config.budgets).await
        } else {
            loom_tools::lsp::LspAttachment::default()
        };
        Engine::open_with_extra_tools(config, overrides, mcp, lsp)
    }
}

/// Registers one tool, mapping a registration failure to the engine error
/// that names it. Every opt-in tool in `open_with_extra_tools` goes through
/// here so the error shape stays uniform.
fn register_tool(
    registry: &mut ToolRegistry,
    name: &'static str,
    tool: Box<dyn loom_kernel::tool::Tool>,
) -> Result<(), EngineError> {
    registry
        .register(tool)
        .map_err(|error| EngineError::RegisterTool {
            tool: name,
            message: error.to_string(),
        })
}

/// Resolves the provider for an engine: an injected provider skips the
/// credential build entirely, a supplied credential builds directly, and
/// otherwise the saved account or environment resolves one. The hook then
/// wraps whichever provider resulted.
fn resolve_provider(
    config: &EngineConfig,
    provider: Option<Box<dyn ModelProvider>>,
    credential: Option<loom_providers::SecretString>,
    hook: Option<ProviderHook>,
) -> Result<Box<dyn ModelProvider>, EngineError> {
    // Provider: an overridden provider skips the credential build
    // entirely (offline replay needs no secret); a hook then wraps
    // whichever provider resulted, so traffic capture composes with
    // both paths.
    let provider: Box<dyn ModelProvider> = if let Some(provider) = provider {
        provider
    } else if let Some(credential) = credential {
        crate::provider::build(config, credential.expose())?.provider
    } else {
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
                .map_or("-", |account| account.label.as_str()),
            "credential resolved"
        );
        // Provider construction and loud capability validation: all
        // adapter-specific knowledge lives in `provider`.
        crate::provider::build(config, credential.secret.expose())?.provider
    };
    let provider = match hook {
        Some(hook) => hook(provider),
        None => provider,
    };
    Ok(provider)
}
