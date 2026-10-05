//! Command-line surface (clap derive).
//!
//! Iteration 1 deliberately has **no config file** (plan 1H): every knob a
//! run needs is a flag with a sensible default, and secrets are managed
//! through `loom auth`. `--base-url` alone covers proxies and gateways.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use loom_core::agent::DEFAULT_MAX_TURNS;
use loom_providers::ReasoningEffort;

use crate::auth::Method;

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

    /// Provider adapter to use.
    #[arg(long, default_value = "deepseek")]
    pub provider: String,

    /// Model identifier (validated against the provider's capability table).
    #[arg(long, default_value = "deepseek-flash")]
    pub model: String,

    /// Override the provider base URL (proxies and gateways).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Reasoning effort, sent explicitly on every request.
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

    /// Do not write the session JSON file at the end of the run.
    #[arg(long)]
    pub no_save: bool,
}

/// Arguments for `loom auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// The auth operation.
    #[command(subcommand)]
    pub command: AuthCommand,
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
    /// Provider to authenticate.
    #[arg(default_value = "deepseek")]
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

    /// Provider the account belongs to.
    #[arg(long, default_value = "deepseek")]
    pub provider: String,
}

/// Arguments for `loom auth remove`.
#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Label or id of the account to remove.
    pub target: String,

    /// Provider the account belongs to.
    #[arg(long, default_value = "deepseek")]
    pub provider: String,
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
        let cli = parse(&["loom", "run", "find", "the", "turn", "loop"]);
        let Command::Run(args) = cli.command else {
            panic!("expected `run`");
        };
        assert_eq!(args.task, vec!["find", "the", "turn", "loop"]);
        assert_eq!(args.provider, "deepseek");
        assert_eq!(args.model, "deepseek-flash");
        assert_eq!(args.reasoning_effort, ReasoningEffort::High);
        assert_eq!(args.max_turns, DEFAULT_MAX_TURNS);
        assert!(!args.json);
        assert!(!args.no_save);
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

        let cli = parse(&["loom", "auth", "switch", "work"]);
        let Command::Auth(auth) = cli.command else {
            panic!("expected `auth`");
        };
        let AuthCommand::Use(args) = auth.command else {
            panic!("expected `use`");
        };
        assert_eq!(args.target, "work");

        let cli = parse(&["loom", "auth", "logout", "work"]);
        let Command::Auth(auth) = cli.command else {
            panic!("expected `auth`");
        };
        let AuthCommand::Remove(args) = auth.command else {
            panic!("expected `remove`");
        };
        assert_eq!(args.target, "work");
    }
}
