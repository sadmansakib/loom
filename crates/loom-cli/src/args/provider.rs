//! `loom provider` argument types (clap derive).

use clap::{Args, Subcommand};

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
        /// Extra HTTP header for chat requests to this gateway
        /// (attribution, correlation), `K=V`; repeatable, and a value
        /// may carry several comma-separated pairs.
        #[arg(long = "header", value_name = "K=V", value_delimiter = ',')]
        headers: Vec<String>,
    },
    /// List registered providers.
    List,
    /// Remove a registered provider (its saved accounts stay).
    Remove {
        /// Provider name.
        name: String,
    },
}
