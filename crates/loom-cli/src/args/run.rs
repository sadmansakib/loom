//! Argument types for `loom run` and `loom chat` (clap derive).

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use loom_core::ReasoningEffort;
use loom_core::kernel::agent::DEFAULT_MAX_TURNS;

/// Arguments for `loom run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The task text; all remaining words are joined with single spaces.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "TASK"
    )]
    pub task: Vec<String>,

    /// Use the interactive terminal frontend for this task.
    #[arg(long)]
    pub tui: bool,

    /// Every knob a one-shot run and an interactive chat share: the
    /// engine assembly reads these verbatim, so `loom run` and
    /// `loom chat` configure the engine identically.
    #[command(flatten)]
    pub flags: RunFlags,
}

/// Arguments for `loom chat`: an optional opening task plus the shared
/// run flags. Lines typed at the prompt continue the session; while a
/// task runs they steer it at the next turn boundary (roadmap item 25).
#[derive(Debug, Args)]
pub struct ChatArgs {
    /// Optional first task; all remaining words are joined with single
    /// spaces. The prompt accepts further tasks afterwards.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "TASK"
    )]
    pub task: Vec<String>,

    #[command(flatten)]
    pub flags: RunFlags,
}

/// Arguments for `loom tui`.
#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Render one static prototype scene instead of opening the interface.
    #[arg(long, value_enum)]
    pub scene: Option<TuiScene>,
}

/// Fixed scenes available to `loom tui --scene`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TuiScene {
    Main,
    Cancel,
    Retry,
    Complete,
    Picker,
    Help,
    Spec,
    Narrow,
    CompactA,
    CompactB,
}

/// The engine-configuration flags shared by `loom run` and `loom chat`.
// One bool per CLI toggle is the point of a flags struct; the fields
// are independent options, not state dimensions.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Args)]
pub struct RunFlags {
    /// Emit one NDJSON event per line instead of human-readable output.
    #[arg(long)]
    pub json: bool,

    /// Provider adapter to use (`deepseek`, `anthropic`, `openrouter`,
    /// `glm`, `nvidia`, `groq`, `custom`) or a name registered with
    /// `loom provider add`. Required — Loom picks no provider for you.
    /// `custom` connects to any OpenAI-compatible endpoint and
    /// additionally requires `--base-url` (plus an API key via
    /// `loom auth login custom` or `CUSTOM_API_KEY`); named providers
    /// carry their URLs in the registry.
    #[arg(long)]
    pub provider: String,

    /// Model identifier (validated against the provider catalog when
    /// reachable, else the provider's capability table). Required.
    #[arg(long)]
    pub model: String,

    /// Override the provider base URL (proxies and gateways). For
    /// `--provider custom` this is the chat API base URL and is
    /// required; for a named provider it overrides the registry
    /// default.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Model-catalog URL for `--provider custom` and named providers:
    /// the endpoint whose `GET` lists the gateway's models. Defaults to
    /// `{--base-url}/models`. Ignored by other providers.
    #[arg(long, value_name = "URL")]
    pub models_url: Option<String>,

    /// Extra HTTP header on every chat request to `custom` and named
    /// gateways — attribution (`--header 'X-Title=loom-eval'`) or
    /// correlation. Repeatable; a value may also carry several
    /// comma-separated `K=V` pairs, which is the `LOOM_HEADERS` format
    /// the eval containers forward. Explicit pairs override same-name
    /// headers from the provider's registry entry; other providers
    /// ignore them.
    #[arg(
        long = "header",
        value_name = "K=V",
        env = "LOOM_HEADERS",
        value_delimiter = ','
    )]
    pub headers: Vec<String>,

    /// Route unambiguous read-only questions to a cheaper model on
    /// the same provider (routing slice zero). This flag alone can
    /// only reduce spend — tasks never route up without
    /// `--route-hard`.
    #[arg(long, value_name = "MODEL", env = "LOOM_ROUTE_EASY")]
    pub route_easy: Option<String>,
    /// Route long build-shaped tasks to a stronger model on the same
    /// provider. Opt-in: without it every task that is not Easy runs
    /// on the run's own model.
    #[arg(long, value_name = "MODEL", env = "LOOM_ROUTE_HARD")]
    pub route_hard: Option<String>,
    /// Escalation-by-fork model (roadmap item 36): a task that stalls
    /// deterministically (max turns or budget exhausted after a loop
    /// nudge) is retried on a fork of the session with this model.
    /// Opt-in and never-worse: the original session and error survive.
    #[arg(long, value_name = "MODEL", env = "LOOM_STALL_ESCALATION_MODEL")]
    pub stall_escalation_model: Option<String>,

    /// Background commands (item 13): register `run_background` and
    /// `command_status`, let long commands run detached instead of
    /// blocking a turn, and steer their completion notices in at the
    /// next turn boundary. Interactive lever — headless eval runs keep
    /// it off.
    #[arg(long, env = "LOOM_BG_COMMANDS")]
    pub bg_commands: bool,

    /// Reasoning effort, sent explicitly on every request by providers
    /// that accept one (`DeepSeek`); ignored elsewhere.
    #[arg(long, default_value = "high")]
    pub reasoning_effort: ReasoningEffort,

    /// Use a saved account by label or id instead of the active account.
    #[arg(long)]
    pub account: Option<String>,

    /// Workspace root the tools are confined to.
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,

    /// Maximum number of model turns for the task.
    #[arg(long, default_value_t = DEFAULT_MAX_TURNS)]
    pub max_turns: u32,

    /// System prompt; defaults to Loom's built-in instruction.
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Do not persist the session to the state database (`--no-save`).
    #[arg(long)]
    pub no_save: bool,

    /// Restrict the run to read-only tools (roadmap item 40): writes,
    /// edits, shell commands, and deferred MCP tools are never exposed.
    #[arg(long)]
    pub read_only: bool,

    /// Leave the named tool out of the registry. Repeatable. Ignored
    /// under `--read-only` and the bare-loop arm, which already filter.
    #[arg(long = "deny-tool", value_name = "NAME")]
    pub deny_tool: Vec<String>,

    /// Ignore the global AGENTS.md instructions file for this run.
    #[arg(long)]
    pub no_global_instructions: bool,

    /// Export this task's events, transcript, and a manifest under
    /// `DIR/<task-id>/` (roadmap item 20.1).
    #[arg(long, value_name = "DIR")]
    pub trace: Option<std::path::PathBuf>,

    /// Continue a persisted session instead of starting a new one
    /// (roadmap item 15): the task input is appended to that session's
    /// transcript and the loop runs to its next stopping point.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Enable the read-only scout tool (roadmap item 19): a child agent
    /// on this cheap provider answers focused workspace questions with
    /// a bounded (~1 KB) capsule, saving parent context. Off by default.
    #[arg(long, value_name = "PROVIDER")]
    pub scout: Option<String>,

    /// Model for `--scout`; required together with it. The parent's
    /// `--provider`/`--model` stay fixed (mid-task selection is the
    /// child's only).
    #[arg(long, value_name = "MODEL")]
    pub scout_model: Option<String>,

    /// Escalation target for the scout (multi-model cascade): when the
    /// cheap model's answer looks weak (timeout, failure, empty, missing
    /// `file:line` pointers, truncated), the question is retried once on
    /// this stronger provider and both answers are returned, labeled.
    /// Requires `--scout`/`--scout-model`.
    #[arg(long, value_name = "PROVIDER")]
    pub scout_escalation: Option<String>,

    /// Model for `--scout-escalation`; required together with it.
    #[arg(long, value_name = "MODEL")]
    pub scout_escalation_model: Option<String>,

    /// Enable the tree-sitter symbol tools (roadmap item 18):
    /// `symbols_overview` and `replace_symbol` join the registry.
    /// Off by default.
    #[arg(long)]
    pub symbols: bool,

    /// Enable the structured git tools (iteration 4): `git_status`,
    /// `git_diff`, and `git_log` join the registry. Off by default.
    #[arg(long)]
    pub git: bool,

    /// Enable the skills seam (iteration 5): `.agents/skills/*.md`
    /// names appear in the facts block and `load_skill` joins the
    /// registry. Off by default.
    #[arg(long)]
    pub skills: bool,

    /// Enable evidence-gated lessons (roadmap item 20.4): lessons mined
    /// by `loom lessons` from `.loom/lessons.json` appear in the facts
    /// block. Off by default.
    #[arg(long)]
    pub lessons: bool,

    /// Enable MCP servers (iteration 6): servers declared in
    /// `.loom/mcp.toml` attach over stdio with ingestion hygiene and
    /// definition pinning into `.loom/mcp.lock` (docs/mcp-design.md).
    /// Off by default.
    #[arg(long)]
    pub mcp: bool,

    /// Enable LSP servers (iteration 6): language servers from the
    /// built-in table (binary on PATH + language files present) and
    /// `.loom/lsp.toml` overrides attach sandboxed over stdio, adding
    /// the `lsp_*` semantic tools (reads, rename, format;
    /// docs/lsp-design.md). Off by default.
    #[arg(long)]
    pub lsp: bool,

    /// Enable machine-applicable compiler fixes (roadmap item 33):
    /// after a failing cargo verify, apply rustc's
    /// `MachineApplicable` suggestions (workspace-confined, snapshotted)
    /// and re-run the same command, keeping the edits only when the
    /// diagnostic count does not increase (docs/autofix-design.md).
    /// Off by default.
    #[arg(long)]
    pub autofix: bool,

    /// Parallel rollouts (roadmap item 21): run the task on N isolated
    /// git worktrees concurrently, select one rollout deterministically
    /// (completion, then fewest failure markers, then fewest turns), and
    /// apply its patch to this workspace. Requires a git repository;
    /// clamps to 2..=8. Off by default.
    #[arg(long, value_name = "N")]
    pub best_of: Option<usize>,

    /// Shared-prefix fork (roadmap item 28): with --best-of, run one
    /// exploratory prefix of this many turns first, then fork all rollouts
    /// from its session so the shared turns are paid once (and hit the
    /// provider prefix cache in every child). Off by default.
    #[arg(long, value_name = "TURNS")]
    pub best_of_prefix_turns: Option<u32>,

    /// Harness variation (roadmap item 24): with --best-of, child 0 keeps
    /// the config while later children cycle terse/verbose tool budgets and
    /// reasoning effort — diversity without a second model. Off by default.
    #[arg(long)]
    pub best_of_vary: bool,

    /// Execution-based selection (roadmap item 29): with --best-of, run
    /// the fixed-table suite (cargo test / npm test / ...) in every
    /// candidate worktree and drop failing candidates before ranking.
    /// Deterministic spend only — no model call. On by default; pass
    /// this flag to opt out when the suite is too slow per rollout.
    #[arg(long)]
    pub best_of_no_verify: bool,
}
