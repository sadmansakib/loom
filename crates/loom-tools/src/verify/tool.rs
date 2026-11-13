use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crate::budget::ToolBudgets;
use crate::policy::{AskRequest, AskResolver, CommandPolicy, PolicyVerdict};
use futures::future::BoxFuture;
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::ecosystem::{Ecosystem, Scope, detect_ecosystem};
use super::render::{render, status_from_content};

/// The `verify` tool.
pub struct VerifyTool {
    /// Escalation rules for the build drivers (`cargo`, `go`, …) —
    /// drivers without a rule run inside the boxed default sandbox.
    policy: CommandPolicy,
    /// Output budgets (item 20.2); `Default` keeps the constants.
    budgets: ToolBudgets,
    /// Whether a failing Cargo verify attempts one machine-applicable
    /// fix round under a never-worse gate (roadmap item 33). Opt-in:
    /// the shipped default and every eval arm leave it off.
    autofix: bool,
    /// Operator approval for `ask` verdicts (review F-13): consulted
    /// with the full command line, exactly like `run_command`. The
    /// engine default is none, so `ask` fails closed.
    ask: Option<AskResolver>,
}

impl std::fmt::Debug for VerifyTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyTool")
            .field("policy", &self.policy)
            .field("budgets", &self.budgets)
            .field("autofix", &self.autofix)
            .field("ask", &self.ask.as_ref().map(|_| "<resolver>"))
            .finish()
    }
}

impl Default for VerifyTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub(super) struct VerifyInput {
    /// `check` compiles only; `test` also runs tests.
    scope: Scope,
    /// Ecosystem: cargo, go, python, npm, or make; omitted = auto-detect
    /// from marker files.
    #[serde(default)]
    ecosystem: Option<Ecosystem>,
}

impl VerifyTool {
    /// The shipped tool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy: CommandPolicy::cargo_only(),
            budgets: ToolBudgets::default(),
            autofix: false,
            ask: None,
        }
    }

    /// A tool under an explicit policy (the engine's entry point after
    /// loading a workspace manifest), so build drivers escalate exactly
    /// like `run_command` programs.
    #[must_use]
    pub fn with_policy(policy: CommandPolicy) -> Self {
        Self {
            policy,
            budgets: ToolBudgets::default(),
            autofix: false,
            ask: None,
        }
    }

    /// Attaches the operator-approval hook so `ask` verdicts reach a
    /// human (review F-13) instead of failing closed on every verify.
    #[must_use]
    pub fn with_ask_resolver(mut self, ask: AskResolver) -> Self {
        self.ask = Some(ask);
        self
    }

    /// Sets explicit output budgets (item 20.2) on this instance.
    #[must_use]
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// Enables the post-failure machine-applicable fix round (roadmap
    /// item 33). Off by default; the engine only sets it from the
    /// opt-in `EngineConfig::autofix`.
    #[must_use]
    pub fn with_autofix(mut self, autofix: bool) -> Self {
        self.autofix = autofix;
        self
    }

    /// Runs the fixed-table test suite once in `root` and reports whether
    /// it passed (roadmap item 29: execution-based selection for
    /// best-of-N). `None` = no verdict — no supported ecosystem, a policy
    /// refusal, or an execution failure. Selection treats "no verdict" as
    /// "no evidence" and never drops a candidate it could not measure.
    ///
    /// Uses the 600 s build budget `run_command` declares, not the
    /// engine's 120 s default: a suite that cannot finish cannot filter.
    pub async fn suite_verdict(
        &self,
        root: &Path,
        cancellation: &CancellationToken,
    ) -> Option<bool> {
        detect_ecosystem(root)?;
        let ctx = ToolContext::new(
            root.to_path_buf(),
            cancellation.clone(),
            Duration::from_secs(600),
        );
        let output = self
            .call(serde_json::json!({ "scope": "test" }), &ctx)
            .await
            .ok()?;
        status_from_content(&output.content)
    }
}

/// The verdict layer gates verify drivers exactly like `run_command`
/// (review F-13): `ask` consults the operator with the full command
/// line and fails closed without a frontend.
async fn gate_driver(
    policy: &CommandPolicy,
    ask: Option<&AskResolver>,
    program: &str,
    args: &[String],
) -> Result<(), ToolError> {
    match policy.verdict(program) {
        PolicyVerdict::Allow => {}
        PolicyVerdict::Deny => {
            return Err(ToolError::InvalidInput(format!(
                "program \"{program}\" is denied by workspace policy \
                 (.loom/commands.toml); cannot verify here"
            )));
        }
        PolicyVerdict::Ask => {
            let rule = policy.rule(program);
            let approved = match ask {
                Some(resolver) => {
                    resolver(AskRequest {
                        program: program.to_string(),
                        args: args.to_vec(),
                        // The driver runs the local build, which
                        // is sandboxed unless a rule escalates it.
                        sandboxed: rule.is_none_or(|rule| rule.sandbox),
                        network: rule.is_some_and(|rule| rule.network),
                    })
                    .await
                }
                None => false,
            };
            if !approved {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{program}\" needs operator approval (policy unknown = \
                     \"ask\") and was not approved{}; cannot verify here",
                    if ask.is_none() {
                        " — no interactive frontend is attached"
                    } else {
                        ""
                    }
                )));
            }
        }
    }
    Ok(())
}

/// Runs the verify command through a declared container/remote
/// runner (item 23) when one is configured; a present-but-broken
/// manifest fails closed. The evidence contract is shared with local
/// execution: the substrate moves, the rendered output does not.
/// Returns `None` when no runner is declared.
async fn run_via_runner(
    ctx: &ToolContext,
    ecosystem: Ecosystem,
    program: &str,
    args: &[String],
    budgets: &ToolBudgets,
) -> Result<Option<ToolOutput>, ToolError> {
    let Some(runner) =
        crate::verify_runner::runner_for(&ctx.workspace_root).map_err(ToolError::Execution)?
    else {
        return Ok(None);
    };
    let (outcome, notice) = runner.run(ctx, program, args).await?;
    let effects = ToolEffects::none().with_command(CommandEffect::new(
        &outcome.runner_program,
        outcome.runner_args.clone(),
        outcome.output.status.code(),
    ));
    let recent_changes = ctx.recent_changes();
    let mut rendered = render(
        ecosystem,
        &outcome.runner_program,
        &outcome.runner_args,
        &outcome.output,
        &recent_changes,
        budgets,
    )
    .with_effects(effects)
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
    Ok(Some(rendered))
}

impl Tool for VerifyTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "verify".to_owned(),
            description: "Bounded verification for the workspace's build system (cargo, \
                          go, python, npm, or make; auto-detected, overridable via \
                          `ecosystem`). Returns structured failure evidence linked \
                          to changed files; never a full log. Prefer the smallest \
                          scope that proves the change."
                .to_owned(),
            parameters: crate::schema::parameters::<VerifyInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        // Runs a command and must not be batched concurrently with mutations.
        false
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: VerifyInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let ecosystem = input
                .ecosystem
                .or_else(|| detect_ecosystem(&ctx.workspace_root))
                .ok_or_else(|| {
                    ToolError::InvalidInput(String::from(
                        "no supported build system at the workspace root; pass `ecosystem`",
                    ))
                })?;
            let (program, args) = ecosystem.command(input.scope)?;
            // The verdict layer gates drivers exactly like run_command
            // (see `gate_driver`); fails closed without a frontend.
            gate_driver(&self.policy, self.ask.as_ref(), program, &args).await?;
            // A declared container/remote runner (item 23) takes over
            // execution: same command table, same budgets, same
            // evidence contract — and a present-but-broken manifest
            // fails closed instead of silently verifying locally.
            if let Some(rendered) =
                run_via_runner(ctx, ecosystem, program, &args, &self.budgets).await?
            {
                return Ok(rendered);
            }
            // Drivers escalate through the same policy rules as
            // `run_command`; a driver without a rule runs inside the
            // boxed default sandbox (network off).
            let rule = self.policy.rule(program);
            let limits = crate::exec::ExecLimits {
                stream_window: rule.and_then(|rule| rule.max_output_bytes),
                scrub: rule.map_or_default(|rule| rule.env_scrub.clone()),
                env_allow: rule.map_or_default(|rule| rule.env_allow.clone()),
            };
            let grants = crate::sandbox::SandboxGrants::from_rule(rule);
            let sandbox = crate::sandbox::Sandbox::resolve(&ctx.workspace_root, grants);
            let outcome = crate::exec::run_bounded(program, &args, ctx, &limits, &sandbox).await?;
            let output = outcome.output;
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                program,
                args.clone(),
                output.status.code(),
            ));
            let recent_changes = ctx.recent_changes();
            let mut rendered = render(
                ecosystem,
                program,
                &args,
                &output,
                &recent_changes,
                &self.budgets,
            )
            .with_effects(effects);
            // Same trace-side facts `run_command` surfaces: an unconfined
            // verification run, a stream an escaped holder truncated.
            if !sandbox.confined() {
                rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                    "sandbox.passthrough",
                    format!("\"{program}\" (verify) ran unconfined"),
                ));
            }
            if outcome.pipe_hold_truncated {
                rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                    "stream.truncated",
                    format!("\"{program}\" (verify) output truncated: a descendant held the pipes after exit"),
                ));
            }
            // Roadmap item 33: after a failing local Cargo verify, one
            // opt-in machine-applicable fix round — plan, snapshot,
            // apply, re-run, never-worse gate. Container-runner and
            // non-Cargo paths skip honestly (they returned above / the
            // suggestion vocabulary is rustc's).
            let fix = if self.autofix
                && matches!(ecosystem, Ecosystem::Cargo)
                && !output.status.success()
            {
                crate::autofix::attempt(
                    ctx,
                    program,
                    &args,
                    &output,
                    &limits,
                    &sandbox,
                    &recent_changes,
                    &self.budgets,
                )
                .await?
            } else {
                None
            };
            if let Some(outcome) = fix {
                rendered = outcome.merge(rendered);
            }
            Ok(rendered)
        })
    }
}
