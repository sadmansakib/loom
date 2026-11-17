//! Engine configuration: provider, model, workspace, tool, and pruning
//! knobs that shape [`crate::engine::Engine::open`].

use std::path::PathBuf;

use loom_kernel::agent::TurnBudget;
use loom_kernel::context::{PruneConfig, ReasoningSchedule};
use loom_kernel::model::ReasoningEffort as KernelReasoningEffort;
use loom_providers::{ProviderCatalog, ReasoningEffort};
use loom_tools::ToolBudgets;

use crate::scout::ScoutConfig;

/// Maps the operator's provider-level effort flag onto the kernel's
/// per-request intent (roadmap item 9). `None` sends nothing — the
/// provider default, and the byte-off behavior.
pub(crate) fn kernel_effort(effort: ReasoningEffort) -> Option<KernelReasoningEffort> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Low => Some(KernelReasoningEffort::Low),
        ReasoningEffort::High | ReasoningEffort::Max => Some(KernelReasoningEffort::High),
    }
}

/// `LOOM_REASONING_SCHEDULE` seam (roadmap item 9 via item 20.2's
/// sweepable-env pattern): `fixed` (the default, byte-identical) or
/// `progressive`, with `LOOM_REASONING_OPEN_TURNS` (default 2)
/// setting how many opening turns run at high effort. Unset keeps the
/// default; an invalid value warns and falls back rather than failing
/// a run over a knob.
pub(crate) fn reasoning_schedule_from_env() -> ReasoningSchedule {
    let Some(raw) = std::env::var("LOOM_REASONING_SCHEDULE")
        .ok()
        .filter(|raw| !raw.is_empty())
    else {
        return ReasoningSchedule::Fixed;
    };
    match raw.as_str() {
        "fixed" => ReasoningSchedule::Fixed,
        "progressive" => {
            let open_turns = std::env::var("LOOM_REASONING_OPEN_TURNS")
                .ok()
                .and_then(|raw| raw.parse::<u16>().ok())
                .unwrap_or(2);
            ReasoningSchedule::Progressive { open_turns }
        }
        other => {
            tracing::warn!(
                value = other,
                "unknown LOOM_REASONING_SCHEDULE; using fixed"
            );
            ReasoningSchedule::Fixed
        }
    }
}
/// Everything a frontend must decide before [`crate::engine::Engine::open`].
///
/// `workspace` is canonicalized by the engine; the rest maps one-to-one to the
/// `loom run` flags. `system_prompt` defaults to Loom's built-in instruction.
///
/// The database location is the frontend's decision: the engine never
/// consults the environment for it.
#[allow(clippy::struct_excessive_bools)] // flat operator config; each bool is an independent CLI flag
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Provider adapter to use — `deepseek` or `openrouter`. Adding one
    /// is a new match arm in [`crate::provider`], nowhere else.
    pub provider: String,
    /// Model identifier, validated against the provider's capability table.
    pub model: String,
    /// Optional provider base-URL override (proxies and gateways).
    pub base_url: Option<String>,
    /// Optional model-catalog URL for the `custom` provider.
    ///
    /// Defaults to `{base_url}/models` when absent. Other providers
    /// ignore it: only `custom` is described entirely by user-supplied
    /// URLs.
    pub models_url: Option<String>,
    /// Extra HTTP headers for the custom adapter's chat requests —
    /// attribution (`X-Title`) and correlation (`X-Loom-Run-Id`) pairs
    /// a gateway can log. Explicit pairs override same-name pairs from
    /// the provider's registry entry, mirroring how an explicit
    /// `--base-url` overrides the recorded endpoint; adapters other
    /// than `custom` (and named registry gateways, which route through
    /// it) ignore them.
    pub headers: Vec<(String, String)>,
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
    /// Staged turn budget (roadmap item 26). The default is inert, so
    /// `max_turns` governs alone and the frozen eval protocol is
    /// unchanged until a probe opts in.
    pub turn_budget: TurnBudget,
    /// Server-side context editing hint (roadmap item 12): `None`
    /// (the default) sends no context-management hint — the kernel's
    /// own projection and compaction stay the only context authority,
    /// and the frozen SWE-bench protocol is unchanged. When set, the
    /// hint rides every `ModelRequest`; adapters that support
    /// server-side clearing translate it (Anthropic
    /// `clear_tool_uses_20250919` + its beta header), others ignore
    /// it. No CLI surface sets it in this slice: a frontend opts in
    /// programmatically, and a default-off knob nothing sets is dead
    /// code.
    pub context_editing: Option<loom_kernel::model::ContextEditingHint>,
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
    /// Restrict the run to read-only tools (roadmap item 40): the
    /// registry keeps only tools whose [`loom_kernel::tool::Tool::is_read_only`]
    /// is true, so writes, edits, shell commands, and deferred MCP tools
    /// are never exposed. Defaults off; the bare-loop baseline ignores it.
    pub read_only: bool,
    /// Model catalog for catalog-driven adapters, handed in as data.
    ///
    /// A frontend that already holds a snapshot passes the matching
    /// [`ProviderCatalog`] variant here and uses [`crate::engine::Engine::open`], which
    /// stays synchronous and never performs network discovery of its own.
    /// Otherwise [`crate::engine::Engine::open_async`] fetches and caches one through
    /// [`crate::catalog`] first. Adapters that resolve capabilities from a
    /// built-in table ignore it; adapters that need one reject the run
    /// when it is absent.
    pub catalog: Option<ProviderCatalog>,
    /// Read-only scout delegation (roadmap item 19): `None` keeps the
    /// tool out of the registry — the default, so every existing
    /// frontend (and the frozen SWE-bench protocol) is unchanged until
    /// a frontend deliberately opts in. When set, the engine composes a
    /// child [`loom_kernel::agent::Agent`] on the cheap model and registers `scout` in the
    /// parent registry.
    pub scout: Option<ScoutConfig>,
    /// Routing slice zero (roadmap item 9): tier → model wiring for
    /// task-start selection. `None` or an inactive config keeps every
    /// task on `model`; a wired `easy` can only route down (cheaper),
    /// and `hard` is a separate opt-in.
    pub routing: Option<crate::routing::RoutingConfig>,
    /// Escalation-by-fork model (roadmap item 36): when set, a task
    /// that exhausts max turns or budget *after* a loop nudge (the
    /// deterministic stall signature) is retried on a fork of the
    /// session with this model. Default-off and never-worse: the
    /// original session is untouched, and an escalated failure still
    /// reports the original error.
    pub stall_escalation_model: Option<String>,
    /// Background commands (roadmap item 13): opt-in interactive-mode
    /// lever. When true the registry grows `run_background` and
    /// `command_status`, long commands run detached instead of blocking
    /// a turn, and their completions ride the steering queue at the
    /// next turn boundary. Headless eval runs keep it off — a detached
    /// poll adds bytes the frozen protocol does not have.
    pub background_commands: bool,
    /// Tree-sitter symbol tools (roadmap item 18): when false (the
    /// default) `symbols_overview`/`replace_symbol` stay out of the
    /// registry, so the frozen SWE-bench protocol and every existing
    /// frontend are unchanged until a frontend opts in. The bare-loop
    /// baseline never carries them.
    pub symbols: bool,
    /// Structured git tools (iteration 4): when false (the default)
    /// `git_status`/`git_diff`/`git_log` stay out of the registry — the
    /// frozen SWE-bench protocol and every existing frontend are
    /// unchanged until a frontend opts in. Read-only porcelain over the
    /// same bounded sandbox exec path `run_command` uses.
    pub git: bool,
    /// On-demand skills index (iteration 5): when false (the default)
    /// the facts block carries no skills menu and `load_skill` stays out
    /// of the registry — the frozen SWE-bench protocol and every
    /// existing frontend are unchanged until a frontend opts in. When
    /// set, `.agents/skills/*.md` is indexed into the facts block and
    /// `load_skill` serves bodies on demand. The bare-loop baseline
    /// never carries it.
    pub skills: bool,
    /// Evidence-gated failure-to-fix lessons (roadmap item 20.4): when
    /// false (the default) the facts block carries no lessons and no
    /// `.loom/lessons.json` is read — the frozen SWE-bench protocol and
    /// every existing frontend are unchanged until a frontend opts in.
    /// When set, lessons mined by `loom lessons` ride the facts block.
    /// The bare-loop baseline never carries them.
    pub lessons: bool,
    /// MCP servers (iteration 6, slice D): when false (the default) no
    /// manifest is read, no server spawns, and the registry is
    /// byte-identical. When set, servers declared in
    /// `.loom/mcp.toml` attach over stdio (spawn, handshake,
    /// `tools/list`) with ingestion hygiene and SHA-256 pinning into
    /// `.loom/mcp.lock` (docs/mcp-design.md). The bare-loop baseline
    /// never carries them.
    pub mcp: bool,
    /// LSP servers (iteration 6, slice E): when false (the default)
    /// nothing spawns and the registry is byte-identical. When set,
    /// language servers from the built-in table (binary on PATH +
    /// language markers present) and `.loom/lsp.toml` overrides attach
    /// sandboxed over stdio, and the `lsp_*` read tools enter the
    /// registry (docs/lsp-design.md). The bare-loop baseline never
    /// carries them.
    pub lsp: bool,
    /// Per-tool output budgets (roadmap item 20.2): the one struct a
    /// sweep varies instead of editing constants and rebuilding.
    /// [`ToolBudgets::default`] reproduces today's byte-exact behavior —
    /// the frozen SWE-bench protocol is unchanged until a frontend
    /// deliberately overrides.
    pub budgets: ToolBudgets,
    /// Full frontend override for the context-pruning policy (roadmap
    /// item 20.2). `None` keeps the per-provider default
    /// ([`crate::engine::budgets::prune_config_for`]), including the GLM/Anthropic
    /// elide-reasoning behavior.
    pub prune: Option<PruneConfig>,
    /// Machine-applicable compiler fixes after a failing verify (roadmap
    /// item 33, docs/autofix-design.md): when false (the default) the
    /// registry is byte-identical and a failing verify reports exactly as
    /// before — the frozen SWE-bench protocol and the bare-loop baseline
    /// never set it. When set, a failing local cargo verify (standalone
    /// or fused into `edit_file`/`write_file`) attempts one fix round:
    /// `MachineApplicable` spans only, workspace-confined, snapshotted,
    /// re-run, kept only if the diagnostic count does not increase.
    pub autofix: bool,
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
        models_url: None,
        headers: Vec::new(),
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace: base.join("workspace"),
        system_prompt: None,
        max_turns: 1,
        turn_budget: TurnBudget::default(),
        context_editing: None,
        db_path: base.join("loom.db"),
        persist: false,
        read_only: false,
        catalog: None,
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
        budgets: ToolBudgets::default(),
        prune: None,
        // Tests construct the default registry; autofix stays opt-in.
        autofix: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_effort_maps_provider_levels_onto_intent() {
        // None is "send nothing" (byte-off); High and Max both land on
        // the kernel's High — dialects without a max tier snap up.
        assert_eq!(kernel_effort(ReasoningEffort::None), None);
        assert_eq!(
            kernel_effort(ReasoningEffort::Low),
            Some(KernelReasoningEffort::Low)
        );
        assert_eq!(
            kernel_effort(ReasoningEffort::High),
            Some(KernelReasoningEffort::High)
        );
        assert_eq!(
            kernel_effort(ReasoningEffort::Max),
            Some(KernelReasoningEffort::High)
        );
    }
}
