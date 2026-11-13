//! Action fusion: verification inside the mutating call.
//!
//! A write followed by a separate verify costs two turns, two model
//! passes, and a full context re-bill; fusing them — the model sets a
//! `verify` argument on the same `write_file`/`edit_file` call — proves
//! the change in the turn that made it. The ecosystem is detected from
//! the workspace's marker files (falling back to cargo when none match,
//! preserving the original behaviour), and the program comes from the
//! fixed table in [`crate::verify`], so the default posture needs no
//! allow-list decision — but an explicit operator deny (or
//! unknown=ask) still refuses, exactly like the standalone verify
//! tool (review item 2.6).

use loom_kernel::tool::{CommandEffect, ToolContext, ToolEffects, ToolError, ToolOutput};

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
///
/// `autofix` mirrors the standalone tool's opt-in (roadmap item 33):
/// after a failing local cargo run, one machine-applicable fix round
/// under a never-worse gate. Its effects — re-run command, kept writes,
/// pre-images, repair note, `AutofixEffect` — arrive in the returned
/// [`ToolEffects`] for the host call to merge into its own.
/// A deny posture (or unknown=ask without a frontend) skips the fused
/// verify with a note; Allow (the default) keeps the historical
/// auto-allow.
fn refusal_note(verdict: crate::policy::PolicyVerdict, program: &str) -> Option<ToolOutput> {
    match verdict {
        crate::policy::PolicyVerdict::Allow => None,
        crate::policy::PolicyVerdict::Deny => Some(ToolOutput::new(format!(
            "verify skipped: program \"{program}\" is denied by workspace policy \
             (.loom/commands.toml)"
        ))),
        crate::policy::PolicyVerdict::Ask => Some(ToolOutput::new(format!(
            "verify skipped: program \"{program}\" needs operator approval \
                 (policy unknown = \"ask\"); no interactive frontend is attached"
        ))),
    }
}

pub(crate) async fn run_fused(
    scope: Scope,
    ctx: &ToolContext,
    changed: &str,
    autofix: bool,
    policy: &crate::policy::CommandPolicy,
) -> Result<(ToolOutput, ToolEffects), ToolError> {
    let ecosystem =
        crate::verify::detect_ecosystem(&ctx.workspace_root).unwrap_or(Ecosystem::Cargo);
    let (program, args) = match ecosystem.command(scope) {
        Ok(plan) => plan,
        Err(err) => {
            let note = ToolOutput::new(format!("verify skipped: {err}"));
            return Ok((note, ToolEffects::none()));
        }
    };
    // Review item 2.6: the fixed table auto-allowed its programs with
    // no policy say at all. An explicit deny (or an unknown=ask
    // posture without a frontend) now refuses — fail closed exactly
    // like the standalone verify tool; an Allow verdict (the default
    // posture) keeps the historical auto-allow. Full-argv (review
    // F-09): a fixed-table command wrapping a denied program refuses
    // too, same as run_command.
    let verdict = policy.verdict_for_argv(program, &args);
    if let Some(note) = refusal_note(verdict, program) {
        return Ok((note, ToolEffects::none()));
    }
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
        // No autofix on the runner substrate (item 33 scope: local
        // sandbox runs) — the run reports exactly what the runner saw.
        let effects = ToolEffects::none().with_command(effect);
        return Ok((rendered, effects));
    }
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    let limits = crate::exec::ExecLimits::default();
    let outcome = crate::exec::run_bounded(program, &args, ctx, &limits, &sandbox).await?;
    let output = outcome.output;
    let effect = CommandEffect::new(program, args.clone(), output.status.code());
    let mut recent = ctx.recent_changes();
    recent.push(changed.to_owned());
    let mut rendered = crate::verify::render(
        ecosystem,
        program,
        &args,
        &output,
        &recent,
        &ToolBudgets::default(),
    );
    let mut effects = ToolEffects::none().with_command(effect);
    // Opt-in fix round (roadmap item 33), same contract as the
    // standalone tool: failing cargo run, machine-applicable spans
    // only, never-worse gate on the re-run.
    let fix = if autofix && matches!(ecosystem, Ecosystem::Cargo) && !output.status.success() {
        crate::autofix::attempt(
            ctx,
            program,
            &args,
            &output,
            &limits,
            &sandbox,
            &recent,
            &ToolBudgets::default(),
        )
        .await?
    } else {
        None
    };
    if let Some(fix) = fix {
        rendered = fix.merge(rendered);
        // The merged output's effects already carry the re-run
        // command and any fix effects; fold them under the first
        // run's command.
        effects = effects.merge(rendered.effects.clone());
    }
    rendered = rendered.with_effects(effects.clone());
    Ok((rendered, effects))
}
