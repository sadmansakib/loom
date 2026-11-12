//! `loom auth` argument types (clap derive).

use clap::{Args, Subcommand};

use loom_core::engine::auth::Method;

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
