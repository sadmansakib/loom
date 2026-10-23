//! Action fusion: verification inside the mutating call.
//!
//! A write followed by a separate verify costs two turns, two model
//! passes, and a full context re-bill; fusing them — the model sets a
//! `verify` argument on the same `write_file`/`edit_file` call — proves
//! the change in the turn that made it. The ecosystem is detected from
//! the workspace's marker files (falling back to cargo when none match,
//! preserving the original behaviour), and the program comes from the
//! fixed table in [`crate::verify`], so this path needs no allow-list
//! decision either.

use loom_kernel::tool::{CommandEffect, ToolContext, ToolError, ToolOutput};

use crate::budget::ToolBudgets;
use crate::verify::{Ecosystem, Scope};

/// Runs the detected ecosystem's verification per `scope` and shapes the
/// result for a fused tool output. `changed` names the path the host call
/// just modified, so the rendered evidence links diagnostics to it
/// exactly like the standalone `verify` tool links them to recent
/// changes.
///
/// When the detected ecosystem has no command for the scope (npm
/// `check`), the host call still succeeds: the write happened, so the
/// fused verify degrades to an explanatory note and no command effect is
/// recorded.
pub(crate) async fn run_fused(
    scope: Scope,
    ctx: &ToolContext,
    changed: &str,
) -> Result<(ToolOutput, Option<CommandEffect>), ToolError> {
    let ecosystem =
        crate::verify::detect_ecosystem(&ctx.workspace_root).unwrap_or(Ecosystem::Cargo);
    let (program, args) = match ecosystem.command(scope) {
        Ok(plan) => plan,
        Err(err) => {
            let note = ToolOutput::new(format!("verify skipped: {err}"));
            return Ok((note, None));
        }
    };
    // A declared container/remote runner (item 23) executes the fused
    // verification too — the evidence contract is shared with the
    // standalone tool, so the substrate moves but the output does not.
    // A broken manifest fails closed.
    if let Some(runner) =
        crate::verify_runner::runner_for(&ctx.workspace_root).map_err(ToolError::Execution)?
    {
        let (outcome, notice) = runner.run(ctx, program, &args).await?;
        let effect = CommandEffect::new(
            &outcome.runner_program,
            outcome.runner_args.clone(),
            outcome.output.status.code(),
        );
        let mut recent = ctx.recent_changes();
        recent.push(changed.to_owned());
        let mut rendered = crate::verify::render(
            ecosystem,
            &outcome.runner_program,
            &outcome.runner_args,
            &outcome.output,
            &recent,
            &ToolBudgets::default(),
        )
        .with_notice(notice);
        if outcome.pipe_hold_truncated {
            rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                "stream.truncated",
                format!(
                    "\"{}\" (verify) output truncated: a descendant held the pipes after exit",
                    outcome.runner_program
                ),
            ));
        }
        return Ok((rendered, Some(effect)));
    }
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    let outcome = crate::exec::run_bounded(
        program,
        &args,
        ctx,
        &crate::exec::ExecLimits::default(),
        &sandbox,
    )
    .await?;
    let output = outcome.output;
    let effect = CommandEffect::new(program, args.clone(), output.status.code());
    let mut recent = ctx.recent_changes();
    recent.push(changed.to_owned());
    let rendered = crate::verify::render(
        ecosystem,
        program,
        &args,
        &output,
        &recent,
        &ToolBudgets::default(),
    );
    Ok((rendered, Some(effect)))
}
