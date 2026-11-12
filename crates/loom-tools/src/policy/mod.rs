//! The command execution policy: what `run_command` may execute, and
//! under which limits (roadmap: command execution policy).
//!
//! A policy is *data*: an ordered rule set keyed by bare program name.
//! The shipped default allows exactly the Rust build driver (`cargo`)
//! with no extra limits — Iteration 1's shape, unchanged for existing
//! workspaces. A workspace extends it with a `.loom/commands.toml`
//! manifest:
//!
//! ```toml
//! # Extra programs run_command may execute, beyond cargo.
//! [[command]]
//! program = "git"
//! timeout_secs = 60
//!
//! [[command]]
//! program = "ripgrep"
//! max_output_bytes = 131072
//! env_scrub = ["AWS_SECRET_ACCESS_KEY"]
//! ```
//!
//! The verdict layer (iteration 4) sits in front of the rules and is
//! optional in every workspace — omit both sections and the behavior is
//! byte-identical with the policy that preceded it:
//!
//! ```toml
//! # Programs that never run, even inside the sandbox.
//! [[deny]]
//! program = "curl"
//!
//! [policy]
//! # Verdict for bare programs with no rule and no deny entry:
//! # "allow" (shipped default: sandboxed run), "ask" (operator
//! # approval; fails closed without an interactive frontend),
//! # or "deny" (strict: only ruled programs run).
//! unknown = "deny"
//! ```
//!
//! A program with a rule is always admitted (the rule *is* the
//! operator's explicit consent); a deny entry for a ruled program is a
//! manifest error, so neither silently shadows the other. The one
//! exception is the shipped `cargo` rule: `[[deny]]
//! program = "cargo"` removes it, because `build.rs` and
//! proc-macros execute arbitrary code even inside the sandbox and a
//! strict workspace must be able to lock the build driver out
//! entirely (re-ruling `cargo` afterwards is still an error).
//!
//! Each rule carries the three per-command limits:
//!
//! * `timeout_secs` — a wall-time cap. On expiry the child's whole
//!   process group is killed (the bounded-execution drop guard) and the
//!   call fails as an execution error the model can see and react to.
//!   Absent means uncapped: the engine's per-turn timeout remains the
//!   only bound, exactly as before.
//! * `max_output_bytes` — the per-stream head/tail window kept from
//!   stdout and stderr. Absent keeps the shipped constant.
//! * `env_scrub` — extra environment variable names removed from the
//!   child, on top of the credential floor below.
//!
//! The credential floor is near-non-negotiable: provider API keys and
//! other Loom credentials are scrubbed from every spawned child — the
//! fixed floor plus any present variable matching a credential suffix
//! (`_API_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`). A manifest may add
//! names to the scrub list; it can narrow the floor only through the
//! per-program `env_allow` list, an explicit, auditable opt-in for
//! programs that legitimately need a credential (for example `gh`
//! needing `GH_TOKEN`).
//!
//! Audit: every executed command is recorded as a structured
//! [`CommandEffect`](loom_kernel::tool::CommandEffect) (program, args,
//! exit status) in the session transcript, and a policy rejection or
//! timeout is recorded as the call's error status — so the durable
//! session is the audit log, with nothing to reconcile against.

mod command;
mod rule;
mod scrub;
mod verdict;

pub use command::CommandPolicy;
pub use rule::CommandRule;
pub(crate) use scrub::scrub_names;
pub use verdict::{AskRequest, AskResolver, PolicyError, PolicyVerdict};

/// Environment variables removed from every spawned child, regardless
/// of manifest: Loom's own credentials never leak into a workspace
/// process. The list covers every provider credential env name Loom
/// can read plus the common cloud/git tokens the suffix patterns
/// in [`scrub`] would otherwise only catch opportunistically.
pub(crate) const SCRUB_FLOOR: &[&str] = &[
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "ZAI_API_KEY",
    "Z_AI_API_KEY",
    "GROQ_API_KEY",
    "NVIDIA_API_KEY",
    "NIM_API_KEY",
    "CUSTOM_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
];

#[cfg(test)]
mod tests;
