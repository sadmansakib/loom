//! The `run_command` tool: bounded process execution inside an OS
//! sandbox, with the workspace's
//! [`CommandPolicy`](crate::policy::CommandPolicy) as the escalation
//! layer.
//!
//! Mutating (architecture §10). There is **no shell**: the program and
//! its arguments are passed to the spawned process directly, so
//! argument content can never be re-interpreted as shell syntax (a
//! model may still run `bash` itself — the sandbox is inherited by
//! everything it spawns). Any bare executable name is accepted and
//! runs inside the OS sandbox: writes confined to the workspace (plus
//! temp and cargo-home caches), network off. A `.loom/commands.toml`
//! rule escalates a program — network access, extra writable
//! directories, or unsandboxed execution — and carries the always-on
//! limits: a wall-time cap (the whole process group is killed on
//! expiry), a per-stream output window, and extra environment
//! scrubbing. The credential floor is always applied, sandbox or not,
//! so a child can never read Loom's provider keys. The child runs with
//! the workspace root as its working directory and is killed when the
//! call future is dropped (timeout or cancellation), so a runaway
//! command cannot outlive the turn.
//!
//! Audit: every executed command is recorded as a structured
//! [`CommandEffect`](loom_kernel::tool::CommandEffect) in the session
//! transcript; escalation grants and rule timeouts land as the call's
//! error status, so the session is the audit log.
//!
//! Output filters (roadmap item 3): successful output from a few
//! well-shaped commands (`git status`/`diff`/`log`, `ls`, `tree`,
//! `grep`, `cargo test`) is reduced at the command's own unit
//! boundaries — a counted marker replaces whole commits, entries,
//! matches, or passing-test lines instead of a byte cut — and the
//! unreduced stream is teed to the artifact store so nothing is lost.
//! The failure path tees the raw log its evidence block replaced, for
//! the same reason. Both tees are best-effort: without an artifact
//! store the output is the bounded raw form it always was.
mod input;
mod output;
mod process;
mod tool;

#[cfg(test)]
mod tests;

pub(crate) use process::resolve_in_path;
pub use tool::RunCommandTool;

#[cfg(test)]
use crate::budget::{self, ToolBudgets};
#[cfg(test)]
use crate::policy::CommandPolicy;
#[cfg(test)]
use input::RunInput;
#[cfg(test)]
use loom_kernel::tool::{CommandEffect, ToolContext, ToolError, ToolOutput};
#[cfg(test)]
use output::render;
#[cfg(test)]
use process::is_executable_file;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::fmt::Write as _;
