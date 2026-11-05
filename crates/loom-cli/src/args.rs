//! Command-line surface (clap derive).
//!
//! Iteration 1 deliberately has **no config file** (plan 1H): every knob a
//! run needs is a flag with a sensible default, and secrets are managed
//! through `loom auth`. `--base-url` alone covers proxies and gateways.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use loom_core::ReasoningEffort;
use loom_core::kernel::agent::DEFAULT_MAX_TURNS;

use loom_core::engine::auth::Method;

/// `loom`: a minimal, deterministic coding agent.
#[derive(Debug, Parser)]
#[command(name = "loom", version, about, long_about = None)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task against the model.
    Run(Box<RunArgs>),
    /// Manage saved provider credentials (`loom.db` + OS keychain).
    Auth(AuthArgs),
    /// Manage named custom providers (`providers.json` beside `loom.db`).
    Provider(ProviderArgs),
    /// List persisted sessions (`loom.db`), oldest first.
    Sessions,
    /// Transcript analytics over stored sessions (or exported traces).
    Stats(StatsArgs),
}

/// Arguments for `loom run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The task text; all remaining words are joined with single spaces.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "TASK"
    )]
    pub task: Vec<String>,

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

    /// Reasoning effort, sent explicitly on every request by providers
    /// that accept one (DeepSeek); ignored elsewhere.
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
    /// file:line pointers, truncated), the question is retried once on
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

    /// Enable MCP servers (iteration 6): servers declared in
    /// `.loom/mcp.toml` attach over stdio with ingestion hygiene and
    /// definition pinning into `.loom/mcp.lock` (docs/mcp-design.md).
    /// Off by default.
    #[arg(long)]
    pub mcp: bool,

    /// Enable LSP servers (iteration 6): language servers from the
    /// built-in table (binary on PATH + language files present) and
    /// `.loom/lsp.toml` overrides attach sandboxed over stdio, adding
    /// the `lsp_*` semantic read tools (docs/lsp-design.md). Off by
    /// default.
    #[arg(long)]
    pub lsp: bool,

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
}

/// Arguments for `loom auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// The auth operation.
    #[command(subcommand)]
    pub command: AuthCommand,
}

/// Arguments for `loom stats`.
#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Aggregate exported traces under this directory instead (as
    /// written by `loom run --trace DIR`).
    #[arg(long, value_name = "DIR")]
    pub trace: Option<std::path::PathBuf>,

    /// Only sessions whose id contains this substring.
    #[arg(long, short = 's')]
    pub session: Option<String>,

    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
}

/// Credential management operations.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Save a credential and make it the provider's active account.
    #[command(alias = "add")]
    Login(LoginArgs),
    /// List saved accounts and environment fallbacks.
    List(ListArgs),
    /// Make a saved account the active one.
    #[command(alias = "switch")]
    Use(UseArgs),
    /// Remove a saved account (and its keychain secret).
    #[command(alias = "logout")]
    Remove(RemoveArgs),
}

/// Arguments for `loom auth login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Provider to authenticate (`deepseek`, `anthropic`, `openrouter`,
    /// `glm`, `nvidia`, `groq`, `custom`). Required.
    pub provider: String,

    /// How the secret is supplied: `key` (prompt, stored in the keychain)
    /// or `command` (run a shell command whose stdout is the key).
    #[arg(long, default_value = "key")]
    pub method: Method,

    /// Human label; defaults to the provider name.
    #[arg(long)]
    pub label: Option<String>,

    /// The command to run, for `--method command`.
    #[arg(long)]
    pub command: Option<String>,

    /// Read the key from stdin instead of prompting.
    #[arg(long)]
    pub stdin: bool,
}

/// Arguments for `loom auth list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Restrict the listing to one provider.
    #[arg(long)]
    pub provider: Option<String>,
}

/// Arguments for `loom auth use`.
#[derive(Debug, Args)]
pub struct UseArgs {
    /// Label or id of the account to activate.
    pub target: String,

    /// Provider the account belongs to. Required.
    #[arg(long)]
    pub provider: String,
}

/// Arguments for `loom auth remove`.
#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Label or id of the account to remove.
    pub target: String,

    /// Provider the account belongs to. Required.
    #[arg(long)]
    pub provider: String,
}

/// Arguments for `loom provider`.
#[derive(Debug, Args)]
pub struct ProviderArgs {
    /// The command to run.
    #[command(subcommand)]
    pub command: ProviderCommand,
}

/// `loom provider` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Register a named OpenAI-compatible gateway.
    Add {
        /// Provider name: lowercase letters, digits, and hyphens,
        /// starting with a letter; must not be a built-in provider.
        name: String,
        /// Chat API base URL; `chat/completions` is appended.
        #[arg(long, value_name = "URL")]
        base_url: String,
        /// Model-catalog URL; defaults to `{--base-url}/models`.
        #[arg(long, value_name = "URL")]
        models_url: Option<String>,
    },
    /// List registered providers.
    List,
    /// Remove a registered provider (its saved accounts stay).
    Remove {
        /// Provider name.
        name: String,
    },
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn run_defaults_and_collects_task_words() {
        let cli = parse(&[
            "loom",
            "run",
            "--provider",
            "nvidia",
            "--model",
            "z-ai/glm-5.3-flash",
            "find",
            "the",
            "turn",
            "loop",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected `run`");
        };
        assert_eq!(args.task, vec!["find", "the", "turn", "loop"]);
        assert_eq!(args.provider, "nvidia");
        assert_eq!(args.model, "z-ai/glm-5.3-flash");
        assert_eq!(args.reasoning_effort, ReasoningEffort::High);
        assert_eq!(args.max_turns, DEFAULT_MAX_TURNS);
        assert!(!args.json);
        assert!(!args.no_save);
        assert!(!args.no_global_instructions);
    }

    #[test]
    fn run_accepts_custom_provider_urls() {
        let cli = parse(&[
            "loom",
            "run",
            "--provider",
            "custom",
            "--model",
            "gateway-model",
            "--base-url",
            "http://localhost:4000/v1",
            "--models-url",
            "http://localhost:4000/v1/models",
            "do",
            "the",
            "thing",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected `run`");
        };
        assert_eq!(args.provider, "custom");
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:4000/v1"));
        assert_eq!(
            args.models_url.as_deref(),
            Some("http://localhost:4000/v1/models")
        );
    }

    #[test]
    fn provider_subcommands_parse() {
        let cli = parse(&[
            "loom",
            "provider",
            "add",
            "orcarouter",
            "--base-url",
            "https://api.orcarouter.ai/v1",
        ]);
        let Command::Provider(args) = cli.command else {
            panic!("expected `provider`");
        };
        match args.command {
            ProviderCommand::Add {
                name,
                base_url,
                models_url,
            } => {
                assert_eq!(name, "orcarouter");
                assert_eq!(base_url, "https://api.orcarouter.ai/v1");
                assert_eq!(models_url, None);
            }
            _ => panic!("expected `add`"),
        }

        assert!(matches!(
            parse(&["loom", "provider", "list"]).command,
            Command::Provider(ProviderArgs {
                command: ProviderCommand::List
            })
        ));
        assert!(matches!(
            parse(&["loom", "provider", "remove", "orcarouter"]).command,
            Command::Provider(ProviderArgs {
                command: ProviderCommand::Remove { name }
            }) if name == "orcarouter"
        ));
    }

    #[test]
    fn run_requires_provider_and_model() {
        let error = Cli::try_parse_from(["loom", "run", "x"]).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("--provider"), "{rendered}");
        assert!(rendered.contains("--model"), "{rendered}");

        let error = Cli::try_parse_from(["loom", "run", "--provider", "nvidia", "x"]).unwrap_err();
        assert!(error.to_string().contains("--model"));

        let error = Cli::try_parse_from(["loom", "run", "--model", "m", "x"]).unwrap_err();
        assert!(error.to_string().contains("--provider"));
    }

    #[test]
    fn auth_use_and_remove_require_their_provider() {
        let ok = Cli::try_parse_from(["loom", "auth", "switch", "work", "--provider", "glm"]);
        assert!(ok.is_ok());
        let error = Cli::try_parse_from(["loom", "auth", "switch", "work"]).unwrap_err();
        assert!(error.to_string().contains("--provider"), "{}", error);

        let ok = Cli::try_parse_from(["loom", "auth", "logout", "work", "--provider", "glm"]);
        assert!(ok.is_ok());
        let error = Cli::try_parse_from(["loom", "auth", "logout", "work"]).unwrap_err();
        assert!(error.to_string().contains("--provider"), "{}", error);

        let error = Cli::try_parse_from(["loom", "auth", "add"]).unwrap_err();
        assert!(error.to_string().contains("PROVIDER"), "{}", error);
    }

    #[test]
    fn run_parses_optional_flags() {
        let cli = parse(&[
            "loom",
            "run",
            "--json",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-v4-pro",
            "--base-url",
            "https://example.test",
            "--reasoning-effort",
            "max",
            "--account",
            "work",
            "--max-turns",
            "3",
            "--no-save",
            "--no-global-instructions",
            "go",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected `run`");
        };
        assert!(args.json);
        assert_eq!(args.base_url.as_deref(), Some("https://example.test"));
        assert_eq!(args.reasoning_effort, ReasoningEffort::Max);
        assert_eq!(args.account.as_deref(), Some("work"));
        assert_eq!(args.max_turns, 3);
        assert!(args.no_save);
        assert!(
            args.no_global_instructions,
            "--no-global-instructions opts out of the user-level file"
        );
    }

    #[test]
    fn reasoning_effort_rejects_unknown_values() {
        let error = Cli::try_parse_from(["loom", "run", "--reasoning-effort", "bogus", "x"]);
        assert!(error.is_err());
    }

    #[test]
    fn auth_aliases_map_to_the_canonical_commands() {
        let cli = parse(&["loom", "auth", "add", "glm"]);
        let Command::Auth(auth) = cli.command else {
            panic!("expected `auth`");
        };
        let AuthCommand::Login(args) = auth.command else {
            panic!("expected `login`");
        };
        assert_eq!(args.provider, "glm");
        assert_eq!(args.method, Method::Key);

        let cli = parse(&["loom", "auth", "switch", "work", "--provider", "glm"]);
        let Command::Auth(auth) = cli.command else {
            panic!("expected `auth`");
        };
        let AuthCommand::Use(args) = auth.command else {
            panic!("expected `use`");
        };
        assert_eq!(args.target, "work");
        assert_eq!(args.provider, "glm");

        let cli = parse(&["loom", "auth", "logout", "work", "--provider", "glm"]);
        let Command::Auth(auth) = cli.command else {
            panic!("expected `auth`");
        };
        let AuthCommand::Remove(args) = auth.command else {
            panic!("expected `remove`");
        };
        assert_eq!(args.target, "work");
        assert_eq!(args.provider, "glm");
    }

    #[test]
    fn stats_parses_the_trace_source() {
        let cli = parse(&["loom", "stats", "--trace", "traces/demo"]);
        let Command::Stats(args) = cli.command else {
            panic!("expected `stats`");
        };
        assert_eq!(
            args.trace.as_deref(),
            Some(std::path::Path::new("traces/demo"))
        );
        assert!(args.session.is_none());

        let cli = parse(&["loom", "stats"]);
        let Command::Stats(args) = cli.command else {
            panic!("expected `stats`");
        };
        assert!(args.trace.is_none());
    }
}
