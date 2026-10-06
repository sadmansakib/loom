//! Action fusion: verification inside the mutating call.
//!
//! A write followed by a separate verify costs two turns, two model
//! passes, and a full context re-bill; fusing them — the model sets a
//! `verify` argument on the same `write_file`/`edit_file` call — proves
//! the change in the turn that made it. The program is fixed (`cargo`,
//! like the `verify` tool), so this path needs no allow-list decision.

use loom_kernel::tool::{CommandEffect, ToolContext, ToolError, ToolOutput};

use crate::verify::Scope;

/// Runs `cargo` per `scope` and shapes the result for a fused tool
/// output. `changed` names the path the host call just modified, so the
/// rendered evidence links diagnostics to it exactly like the standalone
/// `verify` tool links them to recent changes.
pub(crate) async fn run_fused(
    scope: Scope,
    ctx: &ToolContext,
    changed: &str,
) -> Result<(ToolOutput, CommandEffect), ToolError> {
    let args: Vec<String> = scope.args().iter().map(|arg| arg.to_string()).collect();
    let output = crate::exec::run_bounded("cargo", &args, ctx).await?;
    let effect = CommandEffect::new("cargo", args.clone(), output.status.code());
    let mut recent = ctx.recent_changes();
    recent.push(changed.to_owned());
    let rendered = crate::verify::render("cargo", &args, &output, &recent);
    Ok((rendered, effect))
}
