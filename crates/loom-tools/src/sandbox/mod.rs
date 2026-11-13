//! OS sandbox for executed commands (roadmap: sandboxed shell).
//!
//! The allow-list of bare programs is replaced by "any command inside an
//! OS sandbox": writes confined to the workspace plus declared cache
//! directories, network off by default. The wrapper is an external
//! program so no `unsafe` pre-exec hooks are ever needed (the workspace
//! forbids `unsafe`):
//!
//! * macOS — `sandbox-exec` with a generated Seatbelt profile
//!   (`(allow default)` then re-`deny` the deviation axes — writes
//!   outside the writable set and, unless granted, network — plus an
//!   unconditional Apple-event deny, so the
//!   default-allow surface stays broad enough for real toolchains).
//! * Linux — `bubblewrap` (`bwrap`) with read-only system roots, a
//!   tmpfs `/tmp`, `--unshare-net`, and bind-mounts for the writable
//!   set. Landlock needs in-process syscalls the `unsafe` ban rules
//!   out, so the wrapper is the Linux path.
//!
//! The command policy's manifest stays the escalation layer: a rule may
//! grant network access, extra writable directories, or unsandboxed
//! execution, alongside the shipped limits (timeout, output window,
//! env scrubbing — the credential floor applies either way).
//!
//! Sandboxing is inherited: everything a sandboxed command spawns lives
//! in the same profile/namespace, so a `bash -c` pipeline is confined
//! exactly like a bare program.

use std::path::PathBuf;

mod bubblewrap;
mod preflight;
mod resolve;
mod seatbelt;
#[cfg(test)]
mod tests;

pub use preflight::{bwrap_remediation, preflight};
pub(crate) use resolve::Sandbox;

/// The OS-sandbox baseline a rule-less command runs under, for the
/// trace's one-shot `sandbox` notice (v8): which wrapper confines
/// commands on this platform, or that none exists. Per-command
/// deviations (a policy opt-out, a platform without a wrapper) surface
/// separately as `sandbox.passthrough` notices.
#[must_use]
pub fn baseline() -> &'static str {
    if cfg!(target_os = "macos") {
        "commands run OS-confined (macOS Seatbelt profile: workspace + temp + cargo home writable, network off)"
    } else if cfg!(target_os = "linux") {
        "commands run OS-confined (bubblewrap: workspace + temp + cargo home writable, network off)"
    } else {
        "no OS sandbox wrapper on this platform: commands run unconfined"
    }
}

/// Escalation grants from the command policy, beyond the sandbox
/// default. `None` rule (previously: rejected program) means the pure
/// default: sandboxed, no network, no extra writable paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SandboxGrants {
    /// Run outside the sandbox entirely (explicit operator opt-out).
    pub enabled: bool,
    /// Allow outbound and local network access.
    pub network: bool,
    /// Extra writable directories (absolute paths, from the manifest).
    pub extra_writable: Vec<PathBuf>,
}

impl SandboxGrants {
    /// The grants a policy rule carries; no rule means the boxed
    /// default.
    pub(crate) fn from_rule(rule: Option<&crate::policy::CommandRule>) -> Self {
        match rule {
            None => Self {
                enabled: true,
                network: false,
                extra_writable: Vec::new(),
            },
            Some(rule) => Self {
                enabled: rule.sandbox,
                network: rule.network,
                extra_writable: rule.extra_writable.iter().map(PathBuf::from).collect(),
            },
        }
    }
}
