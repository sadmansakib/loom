//! `loom-acp`: serve the loom engine to one ACP client over stdio.
//!
//! The ACP spec allows nothing but protocol messages on stdout, so
//! every diagnostic this binary produces goes to stderr. Zed wires it
//! up as a custom agent:
//!
//! ```json
//! "agent_servers": {
//!   "loom": {
//!     "type": "custom",
//!     "command": "loom-acp",
//!     "args": ["--provider", "deepseek", "--model", "deepseek-chat"]
//!   }
//! }
//! ```
//!
//! Credentials resolve exactly like the CLI's (`loom auth login` or
//! the provider's environment variable); the database location is the
//! shared loom one.

use std::process::ExitCode;

use clap::Parser;
use loom_acp::{AcpState, ServerOptions, serve};

/// Serve loom over the Agent Client Protocol on stdio.
#[derive(Debug, Parser)]
#[command(about = "Serve the loom engine to one ACP client over stdio")]
struct Args {
    /// Provider adapter (`deepseek`, `anthropic`, ...), as in
    /// `loom run --provider`. Required — loom picks no provider for
    /// you. `LOOM_PROVIDER` works too.
    #[arg(long, env = "LOOM_PROVIDER")]
    provider: String,

    /// Model id, validated against the provider's capability table.
    /// `LOOM_MODEL` works too.
    #[arg(long, env = "LOOM_MODEL")]
    model: String,

    /// Override the provider base URL (proxies and gateways).
    #[arg(long, env = "LOOM_BASE_URL")]
    base_url: Option<String>,

    /// Maximum model turns per prompt.
    #[arg(long, default_value_t = loom_core::kernel::agent::DEFAULT_MAX_TURNS)]
    max_turns: u32,
}

fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
    // Stderr only: stdout is the protocol stream (ACP spec, Transports).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let options = ServerOptions::new(args.provider, args.model, loom_core::paths::db_path())
        .with_base_url(args.base_url);
    let options = if args.max_turns != loom_core::kernel::agent::DEFAULT_MAX_TURNS {
        ServerOptions {
            max_turns: args.max_turns,
            ..options
        }
    } else {
        options
    };
    let exit = runtime.block_on(async move {
        match serve(agent_client_protocol::Stdio::new(), AcpState::new(options)).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("loom-acp: connection ended: {error}");
                ExitCode::FAILURE
            }
        }
    });
    Ok(exit)
}
