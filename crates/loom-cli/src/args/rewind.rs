//! `loom rewind` argument types (clap derive).

use clap::Args;

/// Arguments for `loom rewind`.
#[derive(Debug, Args)]
pub struct RewindArgs {
    /// The session to fork: an id from `loom sessions`.
    #[arg(value_name = "SESSION")]
    pub session: String,

    /// Fork at this transcript index — a BOUNDARY row from the listing
    /// this command prints. Omit it to only list the boundaries.
    #[arg(long, value_name = "IDX")]
    pub at: Option<u32>,

    /// Restore the workspace as part of the rewind, not just the
    /// conversation. Tiered by what is provable (`docs/rewind-design.md`
    /// §4): refuses rather than guesses. With `--at IDX`: per-boundary
    /// restore from the pre-images tools captured as they wrote (tier
    /// 2). With `--to-start`: reset to the commit the session started
    /// from (tier 1).
    #[arg(long)]
    pub restore_workspace: bool,

    /// With `--restore-workspace`: reset the workspace to the commit it
    /// was at when the session started. Only possible when that start
    /// was a clean git checkout — the listing says whether a checkpoint
    /// exists.
    #[arg(long, requires = "restore_workspace")]
    pub to_start: bool,

    /// With `--restore-workspace --at`: proceed even though the dropped
    /// region ran commands Loom cannot undo (`commands_run` effects are
    /// surfaced, never silently absorbed). The provable file changes
    /// still restore; the command effects do not.
    #[arg(long, requires = "restore_workspace")]
    pub accept_unrestorable: bool,
}
