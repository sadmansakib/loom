//! The top-level `loom` command tree (clap derive).

use clap::{Parser, Subcommand};

use super::{
    AuthArgs, ChatArgs, LessonsArgs, ProviderArgs, ReplayContextArgs, RewindArgs, RunArgs,
    StatsArgs, TrustArgs, TuiArgs,
};

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
    /// Open the interactive terminal interface or render a fixed scene.
    Tui(TuiArgs),
    /// Interactive session: tasks at the prompt continue one session;
    /// typed lines steer a running task; `/rewind`, `/stats`, and
    /// `/sessions` work in place.
    Chat(Box<ChatArgs>),
    /// Manage saved provider credentials (`loom.db` + OS keychain).
    Auth(AuthArgs),
    /// Manage named custom providers (`providers.json` beside `loom.db`).
    Provider(ProviderArgs),
    /// List persisted sessions (`loom.db`), oldest first.
    Sessions,
    /// Fork a stored session at a chosen turn, leaving the original
    /// intact (`docs/rewind-design.md`).
    Rewind(RewindArgs),
    /// Transcript analytics over stored sessions (or exported traces).
    Stats(StatsArgs),
    /// Mine failure-to-fix lessons from stored transcripts (roadmap
    /// item 20.4) and write `.loom/lessons.json`.
    Lessons(LessonsArgs),
    /// Offline counterfactual context replay: re-compile a recorded
    /// session through the real compiler under different prune knobs
    /// (`docs/replay-context-design.md`).
    ReplayContext(ReplayContextArgs),
    /// Review and pin workspace manifests (`.loom/commands.toml`,
    /// `.loom/verify.toml`, `.loom/mcp.toml`): unpinned manifests
    /// load without their escalation grants.
    Trust(TrustArgs),
}
