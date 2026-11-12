//! The `verify` tool (2F, ADR-017 decision 4).
//!
//! Exposes the verification runtime to the model. It detects the
//! workspace's build system from root marker files — or takes an explicit
//! `ecosystem` argument — runs that ecosystem's deterministic check/test
//! command, then compresses the result through
//! [`evidence`](crate::evidence) so the model receives the failing test,
//! its assertion, the source location, and the recent change it belongs
//! to — rather than an unbounded process log. The full output still
//! overflows to an artifact when oversized (ADR-012/014), like any tool
//! result.
//!
//! Level selection here is deliberately coarse and deterministic: `check`
//! compiles (or statically verifies) without running tests, `test` runs
//! them. The command comes from a fixed per-ecosystem table — never from
//! model input — so no allow-list decision is needed; an escalation is a
//! second call, not a speculative broader run (ADR-017 decision 2).
//! Ecosystems without a build-agnostic `check` (npm) reject the scope
//! rather than guess a script.

mod ecosystem;
mod render;
mod tool;

pub(crate) use ecosystem::Scope;
pub use ecosystem::{Ecosystem, detect_ecosystem};
pub(crate) use render::render;
pub use tool::VerifyTool;

#[cfg(test)]
mod tests;
