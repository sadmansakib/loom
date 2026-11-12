//! `loom lessons` argument types (clap derive).

use clap::Args;

/// Arguments for `loom lessons`.
#[derive(Debug, Args)]
pub struct LessonsArgs {
    /// Workspace whose `.loom/lessons.json` is written (default: the
    /// current directory). The store the facts seam reads on
    /// `loom run --lessons`.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub workspace: std::path::PathBuf,

    /// Preview without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    pub json: bool,
}
