//! `loom trust` argument types (clap derive).

use std::path::PathBuf;

use clap::Args;

/// Arguments for `loom trust`.
#[derive(Debug, Args)]
pub struct TrustArgs {
    /// The workspace to pin (default: the current directory).
    pub workspace: Option<PathBuf>,
    /// Show each manifest's trust state without pinning anything.
    #[arg(long)]
    pub status: bool,
}
