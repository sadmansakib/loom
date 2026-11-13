//! `loom trust`: review and pin workspace manifests (review
//! F-02/F-12).
//!
//! `.loom/commands.toml`, `.loom/verify.toml`, and `.loom/mcp.toml`
//! ship execution power (sandbox off, network on, host-executed
//! runners, spawned servers), and a cloned repository can carry all
//! three. Until pinned, the engine loads them *restrictively* or
//! refuses them outright. This command is the operator's review
//! surface: show the current digests, pin them, and say what each
//! pin unlocks.

use std::path::PathBuf;

use anyhow::{Context, anyhow};

use crate::args::TrustArgs;
use loom_core::tools::trust;

/// Runs `loom trust`.
pub fn run(args: TrustArgs) -> anyhow::Result<std::process::ExitCode> {
    let workspace: PathBuf = args.workspace.unwrap_or_else(|| PathBuf::from("."));
    let workspace = workspace
        .canonicalize()
        .map_err(|error| anyhow!("workspace {}: {error}", workspace.display()))
        .context("resolving the workspace root")?;

    let statuses = trust::status(&workspace);
    println!("workspace: {}", workspace.display());
    if statuses.iter().all(|status| !status.present) {
        println!("no workspace manifests found — nothing to gate, nothing to pin");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for status in &statuses {
        match (&status.digest, status.trusted) {
            (None, _) => println!("  {:<22} absent", status.relative),
            (Some(digest), true) => {
                println!("  {:<22} pinned   {}", status.relative, &digest[..12]);
            }
            (Some(digest), false) => {
                println!("  {:<22} UNPINNED {}", status.relative, &digest[..12]);
            }
        }
    }

    if args.status {
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let pinned =
        trust::pin(&workspace).map_err(|error| anyhow!("writing the trust store: {error}"))?;
    for relative in &pinned {
        println!("pinned {relative}");
    }
    println!(
        "manifest escalations load in full from the next `loom run`/`chat`; \
         re-run this command after editing any manifest"
    );
    Ok(std::process::ExitCode::SUCCESS)
}
