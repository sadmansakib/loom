//! The deterministic fast paths (roadmap item 3).
//!
//! A narrow class of mechanical instructions — identifier renames,
//! single-site constant changes, and codebase formatting — can be
//! solved with **zero** model calls: analyze the instruction, apply
//! the edits, verify green, and hand the proof to the engine to record.
//! The contract is *never worse than the model loop*:
//!
//! 1. The analyzer is strict — anchored grammars over the whole task
//!    text, exact-case identifiers, literal values that must match the
//!    workspace's current state. Any surprise (ambiguity, a colliding
//!    name, a stale "from" value) declines instead of guessing.
//! 2. Application is snapshot-guarded — every file's original bytes are
//!    captured before the first write, and any failure (I/O, parse,
//!    verification) restores them byte-exact before falling through to
//!    the agent loop, which starts from the untouched workspace.
//! 3. Verification is the workspace's own toolchain under the same
//!    policy and sandbox as `run_command`: `cargo check` for code
//!    edits, `cargo fmt --check` for the Cargo format path, `gofmt -l`
//!    plus `go build ./...` for Go modules, and
//!    `cargo metadata --no-deps --offline` for dependency updates (the
//!    manifest-class edit: parse-validity is what a no-network sandbox
//!    can prove; lock regeneration and builds stay with the loop).
//!    Dependency updates stay Cargo-only: no other ecosystem has an
//!    offline manifest-only verifier (npm's `--package-lock-only
//!    --offline` still resolves against the registry or cache, `go
//!    list -m` demands `go mod download`, and pip's resolvers always
//!    consult an index — probed 2026-10-24), so those intents decline
//!    to the agent loop instead of guessing.
//!    Ecosystems without a deterministic workspace-local formatter
//!    (npm, Python, Make) decline rather than guess.
//!
//! The engine calls [`try_fast_path`] before spawning the agent turn
//! loop; `None` (no match, declined, or failed-and-reverted) means the
//! task runs exactly as it would have without this module.

mod constant;
mod deps;
mod format;
mod grammar;
mod rename;
mod verify;
mod workspace;

#[cfg(test)]
mod tests;

#[cfg(test)]
use constant::const_on_line;
#[cfg(test)]
use deps::{edit_manifest, rewrite_dep_line};
#[cfg(test)]
use grammar::{Literal, parse_literal};
#[cfg(test)]
use workspace::replace_word;

use std::path::Path;
use std::sync::Arc;

use crate::lsp::LspServer;
use crate::policy::CommandPolicy;

use constant::apply_constant;
use deps::apply_dependency_update;
use format::{FormatClass, apply_format};
use grammar::{Intent, analyze};
use rename::{apply_rename, apply_rename_lsp};

/// What a successful fast path proved, for the transcript and events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathOutcome {
    /// The analyzer's classification: `"rename"`, `"constant"`, or
    /// `"format"`.
    pub kind: &'static str,
    /// Files the fast path rewrote before verification.
    pub files_changed: u32,
    /// Human-readable proof recorded as the task's assistant item:
    /// what the pattern matched, what changed, what verified it green.
    pub note: String,
}

/// One fast-path attempt: the solve when every guard held, else the
/// coarse reason it fell through to the agent loop. `decline` is
/// `None` exactly when `outcome` is `Some` — the reason distinguishes
/// "no intent matched" from "matched, but a guard or the offline
/// verifier said no" without leaking solver internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathAttempt {
    /// The verified solve, when the fast path succeeded.
    pub outcome: Option<FastPathOutcome>,
    /// Decline reason: `"no-intent-match"`,
    /// `"intent-{kind}-unsolved"` (matched, but a guard or the
    /// offline verifier said no), or — for renames since schema v15
    /// — the specific guard: `intent-rename-collision`,
    /// `intent-rename-zero-occurrences`,
    /// `intent-rename-too-many-occurrences`,
    /// `intent-rename-verify-failed`, `intent-rename-write-failed`.
    pub decline: Option<&'static str>,
}

impl FastPathAttempt {
    /// A failed attempt with its reason.
    pub fn declined(reason: &'static str) -> Self {
        Self {
            outcome: None,
            decline: Some(reason),
        }
    }
}

/// Analyzes the task and, on a strict match, applies and verifies the
/// mechanical solve. `None` — no grammar match, a declined guard, or a
/// failure whose writes were reverted — means "run the agent loop".
///
/// Never-worse contract: on every `None` path the workspace is
/// byte-identical to what it was on entry.
/// Attempts a deterministic solve of `task` in `workspace`.
///
/// Returns the solve when every guard holds, or the coarse reason the
/// attempt fell through — never-worse outranks coverage, and the agent
/// loop takes over an untouched workspace either way.
pub async fn try_fast_path(
    workspace: &Path,
    task: &str,
    policy: &CommandPolicy,
    lsp_servers: &[Arc<LspServer>],
) -> FastPathAttempt {
    let Some(intent) = analyze(task) else {
        return FastPathAttempt::declined("no-intent-match");
    };
    let outcome = match &intent {
        Intent::FormatRust => apply_format(workspace, policy, FormatClass::Rust).await,
        Intent::FormatGo => apply_format(workspace, policy, FormatClass::Go).await,
        Intent::Format => apply_format(workspace, policy, FormatClass::Auto).await,
        Intent::Rename { from, to } => match apply_rename(workspace, policy, from, to).await {
            Ok(outcome) => outcome,
            // Roadmap item 34: the collision class is exactly what a
            // semantic rename addresses — the target name exists as a
            // string somewhere, but the declared symbol is distinct.
            // Every other guard (zero occurrences, budget, verify)
            // stays with the blind path.
            Err(reason @ "intent-rename-collision") if !lsp_servers.is_empty() => {
                match apply_rename_lsp(workspace, policy, from, to, lsp_servers).await {
                    Some(outcome) => Some(outcome),
                    None => return FastPathAttempt::declined(reason),
                }
            }
            Err(reason) => return FastPathAttempt::declined(reason),
        },
        Intent::Constant { name, from, to } => {
            apply_constant(workspace, policy, name, from, to).await
        }
        Intent::DependencyUpdate {
            crate_name,
            to_version,
        } => apply_dependency_update(workspace, policy, crate_name, to_version).await,
    };
    if outcome.is_some() {
        FastPathAttempt {
            outcome,
            decline: None,
        }
    } else {
        FastPathAttempt::declined(intent.decline_reason())
    }
}
