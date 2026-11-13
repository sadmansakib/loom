//! Agent and registry assembly: the scout child, routed tier agents, the
//! stall-escalation child, and the built-in tool registries.

use std::sync::Arc;

use loom_kernel::agent::{Agent, TurnBudget};
use loom_kernel::context::{
    AgentConfig, ContextCompiler, DefaultContextCompiler, PruningContextCompiler,
};
use loom_kernel::tool::ToolRegistry;
use loom_store::SqliteStore;
use loom_tools::policy::{AskResolver, CommandPolicy};
use loom_tools::{
    ArtifactReadTool, EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool,
    ToolBudgets, VerifyTool, WriteFileTool,
};

use crate::auth::{self, KeychainBackend, SqliteAuthStore};
use crate::scout::ScoutConfig;

use super::budgets::prune_config_for;
use super::{EngineConfig, EngineError, RoutedAgent};

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
/// Composes the scout child agent (roadmap item 19) from the parent
/// config: a second provider build on the cheap model, the read-only
/// subset of the built-in registry (recursion is structurally
/// impossible — `scout` is not in the child's registry), the plain
/// append-only compiler, and loop shaping off (a never-mutating scout
/// must not be nudged for zero progress or untested finishes).
pub(crate) fn compose_scout(
    cfg: &ScoutConfig,
    parent: &EngineConfig,
    workspace: &std::path::Path,
    policy: &CommandPolicy,
) -> Result<Agent, EngineError> {
    let child = EngineConfig {
        provider: cfg.provider.clone(),
        model: cfg.model.clone(),
        base_url: parent.base_url.clone(),
        models_url: None,
        // Same inheritance as `base_url`: the child's requests keep
        // the run's gateway identity and correlation headers.
        headers: parent.headers.clone(),
        reasoning_effort: parent.reasoning_effort,
        account: parent.account.clone(),
        workspace: workspace.to_path_buf(),
        system_prompt: None,
        max_turns: cfg.max_turns,
        // Scout children keep a static ceiling: the staged budget is a
        // parent-loop policy, and a bounded read-only scout never has
        // unverified work to fund.
        turn_budget: TurnBudget::default(),
        // Read-only one-question scout: no tool-result bulk worth
        // clearing, and the parent's context-editing choice must not
        // leak into the child's requests.
        context_editing: None,
        db_path: parent.db_path.clone(),
        persist: false,
        catalog: parent.catalog.clone(),
        scout: None,
        routing: None,
        stall_escalation_model: None,
        background_commands: false,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        mcp: false,
        lsp: false,
        budgets: parent.budgets,
        prune: None,
        // No autofix: the scout child is read-only (verify is filtered
        // out), and the one-shot path never enters a workspace loop.
        autofix: false,
    };
    // Credential resolution mirrors the parent's (§18.3) — the scout
    // may live on a different provider with its own secret.
    let store = SqliteAuthStore::open(&child.db_path).map_err(EngineError::Store)?;
    let secrets = KeychainBackend::default();
    let credential = auth::resolve(
        &store,
        &secrets,
        &child.provider,
        child.account.as_deref(),
        |name| std::env::var(name).ok(),
    )?;
    let provider = crate::provider::build(&child, credential.secret.expose())?.provider;
    let registry = ToolRegistry::from_arc_tools(
        build_registry(policy, child.budgets, false, None)?
            .into_arc_tools()
            .filter(|tool| tool.is_read_only()),
    );
    let estimator: Arc<dyn loom_kernel::context::TokenEstimator> =
        crate::estimate::estimator_for(&child.provider);
    let compiler: Box<dyn ContextCompiler> =
        Box::new(DefaultContextCompiler::with_estimator(estimator));
    let agent_config = AgentConfig::new(
        workspace.to_path_buf(),
        "You are loom's read-only scout: answer one focused question about \
         the workspace using read/list/search only, concisely, with \
         file:line pointers for every claim."
            .to_owned(),
    );
    Ok(Agent::new(
        provider,
        compiler,
        registry,
        agent_config,
        cfg.model.clone(),
    )
    .with_max_turns(cfg.max_turns)
    .with_loop_shaping(false))
}

/// Builds the tier agents for routing slice zero: one agent per
/// wired tier model, on the same provider, sharing the registry (with
/// every dynamic registration the run made — scout included), the
/// agent config (system prompt, effort schedule), and the production
/// compiler shape. Only the model id — and the catalog row it resolves
/// to — differs, so capabilities validation fires at open, loudly.
pub(crate) fn build_tier_agents(
    routing: &crate::routing::RoutingConfig,
    config: &EngineConfig,
    registry: &ToolRegistry,
    agent_config: &loom_kernel::context::AgentConfig,
) -> Result<(Option<RoutedAgent>, Option<RoutedAgent>), EngineError> {
    // Credential resolution mirrors the parent's (§18.3) once: every
    // tier lives on the same provider, so one secret serves all.
    let store = SqliteAuthStore::open(&config.db_path).map_err(EngineError::Store)?;
    let secrets = KeychainBackend::default();
    let credential = auth::resolve(
        &store,
        &secrets,
        &config.provider,
        config.account.as_deref(),
        |name| std::env::var(name).ok(),
    )?;
    let mut easy: Option<RoutedAgent> = None;
    let mut hard: Option<RoutedAgent> = None;
    for (tier, model) in [
        (crate::routing::Tier::Easy, routing.easy.as_ref()),
        (crate::routing::Tier::Hard, routing.hard.as_ref()),
    ] {
        let Some(model) = model else { continue };
        let routed = build_child_agent(
            model,
            config,
            registry,
            agent_config,
            credential.secret.expose(),
        )?;
        match tier {
            crate::routing::Tier::Easy => easy = Some(routed),
            crate::routing::Tier::Medium | crate::routing::Tier::Hard => hard = Some(routed),
        }
    }
    Ok((easy, hard))
}

/// Builds one child agent for a second model on the same provider
/// (routing tiers, item 36 escalation): the child inherits everything
/// that carries the run's gateway identity — registry, agent config,
/// system prompt, effort schedule — and persists through the same
/// database the main agent does. Escalation callers resolve the
/// credential themselves; `secret` is the parent's.
fn build_child_agent(
    model: &str,
    config: &EngineConfig,
    registry: &ToolRegistry,
    agent_config: &loom_kernel::context::AgentConfig,
    secret: &str,
) -> Result<RoutedAgent, EngineError> {
    let child = EngineConfig {
        model: model.to_string(),
        ..config.clone()
    };
    let provider = crate::provider::build(&child, secret)?.provider;
    let tier_registry = ToolRegistry::from_arc_tools(registry.arc_tools());
    let estimator: Arc<dyn loom_kernel::context::TokenEstimator> =
        crate::estimate::estimator_for(&child.provider);
    let compiler: Box<dyn ContextCompiler> = Box::new(
        PruningContextCompiler::new(
            Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
            child
                .prune
                .unwrap_or_else(|| prune_config_for(&child.provider)),
        )
        .with_estimator(estimator),
    );
    let agent = Agent::new(
        provider,
        compiler,
        tier_registry,
        agent_config.clone(),
        model.to_string(),
    )
    .with_max_turns(child.max_turns)
    .with_turn_budget(child.turn_budget)
    .with_loop_shaping(true);
    // Child agents persist through the same database the main
    // agent does — a routed or escalated task must leave the same
    // durable transcript any other task would (separate handle,
    // same file, exactly how the engine opens its session/artifact/
    // event stores). Without this, routing or escalation would
    // silently produce sessions that cannot be resumed.
    let agent = if config.persist {
        let store = SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
        let artifacts = SqliteStore::open(config.db_path.clone()).map_err(EngineError::Session)?;
        agent
            .with_store(Box::new(store))
            .with_artifacts(Some(Arc::new(artifacts)))
    } else {
        agent
    };
    Ok(RoutedAgent {
        agent: Arc::new(agent),
        model: model.to_string(),
    })
}

/// Escalation-by-fork (roadmap item 36) builder: resolves the shared
/// credential once and delegates to [`build_child_agent`].
pub(crate) fn build_stall_escalation_agent(
    model: &str,
    config: &EngineConfig,
    registry: &ToolRegistry,
    agent_config: &loom_kernel::context::AgentConfig,
) -> Result<RoutedAgent, EngineError> {
    let store = SqliteAuthStore::open(&config.db_path).map_err(EngineError::Store)?;
    let secrets = KeychainBackend::default();
    let credential = auth::resolve(
        &store,
        &secrets,
        &config.provider,
        config.account.as_deref(),
        |name| std::env::var(name).ok(),
    )?;
    build_child_agent(
        model,
        config,
        registry,
        agent_config,
        credential.secret.expose(),
    )
}

pub(crate) fn build_registry(
    policy: &CommandPolicy,
    budgets: ToolBudgets,
    autofix: bool,
    ask: Option<&AskResolver>,
) -> Result<ToolRegistry, EngineError> {
    let mut registry = ToolRegistry::new();
    let mut register = |name: &'static str, tool: Box<dyn loom_kernel::tool::Tool>| {
        registry
            .register(tool)
            .map_err(|error| EngineError::RegisterTool {
                tool: name,
                message: error.to_string(),
            })
    };
    register(
        "read_file",
        Box::new(ReadFileTool::default().with_budgets(budgets)),
    )?;
    register("list_dir", Box::new(ListDirTool))?;
    register("search", Box::new(SearchTool))?;
    register(
        "artifact_read",
        Box::new(ArtifactReadTool::default().with_budgets(budgets)),
    )?;
    register(
        "write_file",
        Box::new(
            WriteFileTool::default()
                .with_autofix(autofix)
                .with_policy(policy.clone()),
        ),
    )?;
    register(
        "edit_file",
        Box::new(
            EditFileTool::default()
                .with_budgets(budgets)
                .with_autofix(autofix)
                .with_policy(policy.clone()),
        ),
    )?;
    let mut run_command = RunCommandTool::with_policy(policy.clone()).with_budgets(budgets);
    let mut verify = VerifyTool::with_policy(policy.clone())
        .with_budgets(budgets)
        .with_autofix(autofix);
    if let Some(ask) = ask {
        run_command = run_command.with_ask_resolver(Arc::clone(ask));
        verify = verify.with_ask_resolver(Arc::clone(ask));
    }
    register("run_command", Box::new(run_command))?;
    register("verify", Box::new(verify))?;
    register("todo", Box::new(loom_kernel::tool::TodoTool::new()))?;
    Ok(registry)
}

/// The bare-loop registry: the primitive tools a basic harness offers
/// (read/list/write/edit/shell), filtered out of the built set. Anything
/// else the full engine registers — search, the artifact refetch, the
/// fused verify, the kernel-owned `todo` list — is a harness lever the
/// baseline must run without.
pub(crate) fn bare_registry(built: ToolRegistry) -> ToolRegistry {
    const PRIMITIVES: [&str; 5] = [
        "read_file",
        "list_dir",
        "write_file",
        "edit_file",
        "run_command",
    ];
    ToolRegistry::from_arc_tools(
        built
            .into_arc_tools()
            .filter(|tool| PRIMITIVES.contains(&tool.spec().name.as_str())),
    )
}

/// The replay arm's tool registry: the production registry with default
/// policy and budgets, used only so the replay compiler sees a real
/// `ToolSpec` set (the census's `tool_specs` source). No tool is ever
/// executed during replay.
pub(crate) fn replay_registry() -> Result<ToolRegistry, EngineError> {
    build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
    )
}

#[must_use]
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
