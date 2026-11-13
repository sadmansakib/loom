//! The `run_command` tool proper: the spec, the policy verdict gate, and
//! the orchestration from parsed input to rendered output.

use std::fmt::Write as _;
use std::time::Duration;

use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde_json::Value;

use crate::budget::ToolBudgets;
use crate::policy::{AskRequest, AskResolver, CommandPolicy, PolicyVerdict};

use super::input::RunInput;
use super::output::render;
use super::process::resolve_in_path;

/// Runs a program in the workspace root inside an OS sandbox and
/// returns its combined output and exit status.
#[derive(Clone)]
pub struct RunCommandTool {
    policy: CommandPolicy,
    spec: ToolSpec,
    budgets: ToolBudgets,
    ask: Option<AskResolver>,
}

impl std::fmt::Debug for RunCommandTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCommandTool")
            .field("policy", &self.policy)
            .field("spec", &self.spec)
            .field("budgets", &self.budgets)
            .field("ask", &self.ask.as_ref().map(|_| "<resolver>"))
            .finish()
    }
}

impl Default for RunCommandTool {
    fn default() -> Self {
        Self::under(CommandPolicy::cargo_only())
    }
}

impl RunCommandTool {
    fn under(policy: CommandPolicy) -> Self {
        // The escalation grants are policy data, so the spec closes
        // over this instance's policy — stable within an engine, since
        // the policy is fixed at open.
        let escalated = policy
            .allowed_names()
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let description = format!(
            "Runs any bare program in the workspace inside an OS sandbox \
             (writes confined to the workspace, network off); returns \
             combined stdout/stderr and exit status. No shell: pass the \
             program and each argument separately (`bash` is fine — its \
             children stay sandboxed). Well-shaped command output (git, \
             ls, tree, grep, cargo test) is compacted; the full stream is \
             stored as an artifact (artifact_read). Escalated, \
             grants beyond the sandbox default: {escalated}.{admission}",
            admission = admission_sentence(&policy),
        );
        Self {
            spec: ToolSpec {
                name: "run_command".to_owned(),
                description,
                parameters: crate::schema::parameters::<RunInput>(),
            },
            policy,
            budgets: ToolBudgets::default(),
            ask: None,
        }
    }

    /// The shipped tool: the workspace's manifest policy resolved by
    /// the engine, or `cargo`-only when opened directly.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets explicit output budgets (item 20.2) on this instance.
    #[must_use]
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// A tool under an explicit policy (the engine's entry point after
    /// loading a workspace manifest).
    #[must_use]
    pub fn with_policy(policy: CommandPolicy) -> Self {
        Self::under(policy)
    }

    /// Attaches the operator-approval hook for `ask` verdicts
    /// (interactive frontends; without one, `ask` fails closed).
    #[must_use]
    pub fn with_ask_resolver(mut self, ask: AskResolver) -> Self {
        self.ask = Some(ask);
        self
    }

    /// A tool allowing a different program set with shipped-default
    /// limits (tests, embedders).
    pub fn with_allowed<I, S>(allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::under(CommandPolicy::allowing(allowed))
    }
}

/// The verdict-layer sentence appended to the tool description. Empty
/// for the shipped default policy (allow-unknown, nothing denied), so
/// the model-visible spec — and the cached prompt prefix — stay
/// byte-identical until an operator actually narrows the policy.
fn admission_sentence(policy: &CommandPolicy) -> String {
    let denied = policy.denied_names();
    let unknown = policy.unknown_verdict();
    if denied.is_empty() && unknown == PolicyVerdict::Allow {
        return String::new();
    }
    let unknown = match unknown {
        PolicyVerdict::Allow => "run sandboxed",
        PolicyVerdict::Ask => "need operator approval (fails closed without a frontend)",
        PolicyVerdict::Deny => "are refused",
    };
    let mut sentence = format!(" Unrecognized programs {unknown}.");
    if !denied.is_empty() {
        let names = denied
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(sentence, " Never run: {names}.");
    }
    sentence
}
/// A path-qualified program is normally the same program the model
/// means, so keep the bare name when it resolves on PATH
/// (`/usr/bin/cargo` behaves like `cargo`). Only a basename that is
/// not on PATH stays an error. Returns the original spelling when a
/// normalization happened.
fn normalize_program(program: &mut String) -> Result<Option<String>, ToolError> {
    let mut normalized = None;
    if program.contains('/') || program.contains('\\') {
        let original = program.clone();
        match std::path::Path::new(&original)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
        {
            Some(base) if resolve_in_path(base).is_some() => {
                base.clone_into(program);
                normalized = Some(original);
            }
            _ => {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{program}\" must be a bare executable name, not a path",
                )));
            }
        }
    }
    Ok(normalized)
}

/// The verdict layer gates execution before anything runs: denied
/// programs never execute (wrappers carrying a denied program in
/// their arguments are refused too — review F-09), and `ask` verdicts
/// need operator approval (failing closed without a frontend).
async fn gate_program(
    policy: &CommandPolicy,
    ask: Option<&AskResolver>,
    program: &str,
    args: &[String],
) -> Result<(), ToolError> {
    match policy.verdict_for_argv(program, args) {
        PolicyVerdict::Allow => {}
        PolicyVerdict::Deny => {
            let wrapped = policy.wrapped_denied_name(program, args);
            return Err(ToolError::InvalidInput(format!(
                "program \"{program}\" is denied by workspace policy (.loom/commands.toml){}; \
                 pick another approach",
                wrapped.map_or_else(String::new, |name| format!(
                    " — it wraps the denied program \"{name}\""
                )),
            )));
        }
        PolicyVerdict::Ask => {
            // Full-argv approval (review F-13): the operator sees
            // exactly what will run and under which escalation, not
            // just the program name.
            let rule = policy.rule(program);
            let approved = match ask {
                Some(resolver) => {
                    resolver(AskRequest {
                        program: program.to_owned(),
                        args: args.to_vec(),
                        sandboxed: rule.is_none_or(|rule| rule.sandbox),
                        network: rule.is_some_and(|rule| rule.network),
                    })
                    .await
                }
                None => false,
            };
            if !approved {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{program}\" needs operator approval (policy unknown = \"ask\") \
                     and was not approved{}; pick another approach",
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

impl Tool for RunCommandTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        false
    }

    /// A parse failure here is overwhelmingly a wrong argument shape
    /// rather than a wrong value, so name the shape the model should send.
    /// A build or test run outlives the engine's default 120 s budget:
    /// 7 of the 124 tool failures in the pre-fix sweep were this tool
    /// timing out there, each one a turn spent on a half-finished
    /// `cargo test`. A `[[command]]` policy rule that names a
    /// `timeout_secs` still caps that program more tightly.
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        // ToolFailure is #[non_exhaustive]: the wildcard arm is required.
        #[allow(clippy::wildcard_enum_match_arm)]
        match failure {
            loom_kernel::transcript::ToolFailure::InvalidInput => Some(
                "expected fields: program (bare executable name), args (array of strings)"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let mut input: RunInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            // A path-qualified program is normalized to its bare name
            // (see `normalize_program`), then gated: denied programs
            // never execute and `ask` verdicts need operator approval
            // before anything runs.
            let normalized = normalize_program(&mut input.program)?;
            gate_program(&self.policy, self.ask.as_ref(), &input.program, &input.args).await?;
            // The sandbox wrapper reports a missing program as an opaque
            // non-zero exit; resolve bare names on PATH first so the
            // model gets the same "cannot run" either way.
            if resolve_in_path(&input.program).is_none() {
                return Err(ToolError::Execution(format!(
                    "cannot run \"{}\": not found on PATH (no shell here: pass an \
                     executable name and separate `args`; there is no `cd`, so use \
                     workspace-relative paths)",
                    input.program
                )));
            }
            // No rule is the sandboxed default; a rule escalates.
            let rule = self.policy.rule(&input.program);
            let limits = crate::exec::ExecLimits {
                stream_window: rule.and_then(|rule| rule.max_output_bytes),
                scrub: rule.map_or_default(|rule| rule.env_scrub.clone()),
                env_allow: rule.map_or_default(|rule| rule.env_allow.clone()),
            };
            let grants = crate::sandbox::SandboxGrants::from_rule(rule);
            let sandbox = crate::sandbox::Sandbox::resolve(&ctx.workspace_root, grants);
            let bounded =
                crate::exec::run_bounded(&input.program, &input.args, ctx, &limits, &sandbox);
            let outcome = match rule.and_then(|rule| rule.timeout_secs) {
                None => bounded.await?,
                Some(secs) => {
                    match tokio::time::timeout(Duration::from_secs(secs), bounded).await {
                        Ok(result) => result?,
                        // The elapsed future was dropped, so the drop guard
                        // killed the whole process group; the model sees the
                        // cap and can retry with a smaller ask.
                        Err(_elapsed) => {
                            return Err(ToolError::Execution(format!(
                                "command \"{}\" timed out after {secs}s (process group killed)",
                                input.program
                            )));
                        }
                    }
                }
            };
            let recent_changes = ctx.recent_changes();
            // Same trace-side facts the text cannot carry: an
            // unconfined run (policy opt-out or platform without a
            // wrapper) and a stream abandoned to an escaped holder.
            let mut rendered = render(
                &input.program,
                &input.args,
                &outcome.output,
                &recent_changes,
                ctx,
                &self.budgets,
            );
            if let Some(original) = normalized {
                rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                    "program.normalized",
                    format!(
                        "\"{original}\" normalized to the bare name \"{}\"",
                        input.program
                    ),
                ));
            }
            if !sandbox.confined() {
                rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                    "sandbox.passthrough",
                    format!(
                        "\"{}\" ran unconfined (sandbox disabled by policy or platform)",
                        input.program
                    ),
                ));
            }
            if outcome.pipe_hold_truncated {
                rendered = rendered.with_notice(loom_kernel::tool::ToolNotice::new(
                    "stream.truncated",
                    format!(
                        "\"{}\" output truncated: a descendant held the pipes after exit",
                        input.program
                    ),
                ));
            }
            Ok(rendered)
        })
    }
}
