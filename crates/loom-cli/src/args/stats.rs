//! `loom stats` argument types (clap derive).

use clap::Args;

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
