//! Verification runners: the workspace's own toolchain under the same
//! policy, sandbox, and bounds as `run_command` (`cargo check`,
//! `cargo fmt --check`, `gofmt`, `go build`, `cargo metadata`).

use std::path::Path;
use std::time::Duration;

use loom_kernel::tool::ToolContext;
use tokio_util::sync::CancellationToken;

use crate::exec::{ExecLimits, run_bounded};
use crate::policy::{CommandPolicy, PolicyVerdict};
use crate::sandbox::{Sandbox, SandboxGrants};
/// Runs `<program> <args…>` under the policy, sandbox, and bounds the
/// `run_command` tool uses for that program; green exit means verified.
/// The verdict layer applies here too: a non-allow verdict simply means
/// "cannot verify this way" — the fast path declines and the normal
/// turn flow surfaces the refusal where the model can see it.
pub(super) async fn program_ok(
    workspace: &Path,
    policy: &CommandPolicy,
    program: &str,
    args: &[&str],
) -> bool {
    if policy.verdict_for_argv(program, args) != PolicyVerdict::Allow {
        return false;
    }
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let rule = policy.rule(program);
    let limits = ExecLimits {
        stream_window: rule.and_then(|rule| rule.max_output_bytes),
        scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
        env_allow: rule.map(|rule| rule.env_allow.clone()).unwrap_or_default(),
    };
    let grants = SandboxGrants::from_rule(rule);
    let sandbox = Sandbox::resolve(workspace, grants);
    let ctx = ToolContext::new(
        workspace.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(300),
    );
    match run_bounded(program, &owned, &ctx, &limits, &sandbox).await {
        Ok(outcome) => outcome.output.status.success(),
        Err(error) => {
            tracing::debug!(error = %error, "fast path verification could not run");
            false
        }
    }
}

/// `cargo` specialization of [`program_ok`].
pub(super) async fn cargo_ok(workspace: &Path, policy: &CommandPolicy, args: &[&str]) -> bool {
    program_ok(workspace, policy, "cargo", args).await
}

/// Runs `<program> <args…>` capturing stdout, when the run exits at all.
/// Honors the verdict layer like [`program_ok`] (no output = no help).
pub(super) async fn program_output(
    workspace: &Path,
    policy: &CommandPolicy,
    program: &str,
    args: &[&str],
) -> Option<String> {
    if policy.verdict_for_argv(program, args) != PolicyVerdict::Allow {
        return None;
    }
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let rule = policy.rule(program);
    let limits = ExecLimits {
        stream_window: rule.and_then(|rule| rule.max_output_bytes),
        scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
        env_allow: rule.map(|rule| rule.env_allow.clone()).unwrap_or_default(),
    };
    let grants = SandboxGrants::from_rule(rule);
    let sandbox = Sandbox::resolve(workspace, grants);
    let ctx = ToolContext::new(
        workspace.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(300),
    );
    let outcome = run_bounded(program, &owned, &ctx, &limits, &sandbox)
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&outcome.output.stdout).into_owned())
}
