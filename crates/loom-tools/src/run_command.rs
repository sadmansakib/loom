//! The `run_command` tool: bounded process execution inside an OS
//! sandbox, with the workspace's
//! [`CommandPolicy`](crate::policy::CommandPolicy) as the escalation
//! layer.
//!
//! Mutating (architecture §10). There is **no shell**: the program and
//! its arguments are passed to the spawned process directly, so
//! argument content can never be re-interpreted as shell syntax (a
//! model may still run `bash` itself — the sandbox is inherited by
//! everything it spawns). Any bare executable name is accepted and
//! runs inside the OS sandbox: writes confined to the workspace (plus
//! temp and cargo-home caches), network off. A `.loom/commands.toml`
//! rule escalates a program — network access, extra writable
//! directories, or unsandboxed execution — and carries the always-on
//! limits: a wall-time cap (the whole process group is killed on
//! expiry), a per-stream output window, and extra environment
//! scrubbing. The credential floor is always applied, sandbox or not,
//! so a child can never read Loom's provider keys. The child runs with
//! the workspace root as its working directory and is killed when the
//! call future is dropped (timeout or cancellation), so a runaway
//! command cannot outlive the turn.
//!
//! Audit: every executed command is recorded as a structured
//! [`CommandEffect`](loom_kernel::tool::CommandEffect) in the session
//! transcript; escalation grants and rule timeouts land as the call's
//! error status, so the session is the audit log.
//!
//! Output filters (roadmap item 3): successful output from a few
//! well-shaped commands (`git status`/`diff`/`log`, `ls`, `tree`,
//! `grep`, `cargo test`) is reduced at the command's own unit
//! boundaries — a counted marker replaces whole commits, entries,
//! matches, or passing-test lines instead of a byte cut — and the
//! unreduced stream is teed to the artifact store so nothing is lost.
//! The failure path tees the raw log its evidence block replaced, for
//! the same reason. Both tees are best-effort: without an artifact
//! store the output is the bounded raw form it always was.

use std::fmt::Write as _;
use std::time::Duration;

use futures::future::BoxFuture;
use loom_kernel::artifact::{ArtifactRef, store_artifact};
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::budget::{self, ToolBudgets};
use crate::evidence::{Evidence, cargo_diagnostics, libtest_failures, nextest_junit};
use crate::policy::{AskResolver, CommandPolicy, PolicyVerdict};
use crate::reduce;

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
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets explicit output budgets (item 20.2) on this instance.
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// A tool under an explicit policy (the engine's entry point after
    /// loading a workspace manifest).
    pub fn with_policy(policy: CommandPolicy) -> Self {
        Self::under(policy)
    }

    /// Attaches the operator-approval hook for `ask` verdicts
    /// (interactive frontends; without one, `ask` fails closed).
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
        sentence.push_str(&format!(" Never run: {names}."));
    }
    sentence
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct RunInput {
    /// Executable name, no path; runs inside the sandbox unless the
    /// policy escalates it.
    program: String,
    /// Arguments, one array entry each.
    #[serde(default, deserialize_with = "args_from_value")]
    args: Vec<String>,
}

/// Accepts the documented array form and, leniently, the two shapes a
/// model actually emits: the array's own JSON text (`"[--version]"`)
/// and a bare command line (`--version --quiet`). The coercion is
/// deliberate — every accepted shape has one unambiguous reading, and
/// a rejected shape costs the model a whole turn to retry.
fn args_from_value<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Ok(text),
                Value::Number(number) => Ok(number.to_string()),
                Value::Bool(flag) => Ok(flag.to_string()),
                other => Err(D::Error::custom(format!(
                    "args entries must be strings, got {}",
                    json_kind(&other)
                ))),
            })
            .collect(),
        Value::String(text) => Ok(split_args_lenient(&text)),
        other => Err(D::Error::custom(format!(
            "args must be an array of strings, got {}",
            json_kind(&other)
        ))),
    }
}

/// One-word name for a JSON value's kind, for coercion errors.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Splits `args` that arrived as one string, tolerating the two near-array
/// shapes the model actually emits: a leading `=` before the array
/// (`"=[\"ls\", \"-l\"]"`) and a bracketed list whose inner tokens are not
/// quoted (`"[test, r2574, --quiet]"`). Each has exactly one reading, and
/// rejecting it costs a turn to retry.
fn split_args_lenient(text: &str) -> Vec<String> {
    let trimmed = text.trim().trim_start_matches('=').trim();
    if let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        // Valid JSON array text never reaches here — `args_from_value`
        // parses it first — so the separators are commas that the model
        // left unquoted; `split_args` still honours any quotes present.
        return split_args(&inner.replace(',', " "));
    }
    split_args(trimmed)
}

/// Splits a command line the way a shell would tokenize it — whitespace
/// separates, quotes group, a backslash escapes the next character —
/// with no expansion, substitution, or globbing. Used only when `args`
/// arrives as one string that is not JSON array text.
fn split_args(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match quote {
            Some(mark) if character == mark => quote = None,
            Some(_) => {
                current.push(character);
                started = true;
            }
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    started = true;
                }
                '\\' => {
                    if let Some(escaped) = characters.next() {
                        current.push(escaped);
                        started = true;
                    }
                }
                character if character.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                character => {
                    current.push(character);
                    started = true;
                }
            },
        }
    }
    if started {
        words.push(current);
    }
    words
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
            // A path-qualified program is normally the same program the
            // model means, so keep the bare name when it resolves on
            // PATH (`/usr/bin/cargo` behaves like `cargo`). Only a
            // basename that is not on PATH stays an error.
            let mut normalized = None;
            if input.program.contains('/') || input.program.contains('\\') {
                let original = input.program.clone();
                match std::path::Path::new(&original)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                {
                    Some(base) if resolve_in_path(base).is_some() => {
                        input.program = base.to_owned();
                        normalized = Some(original);
                    }
                    _ => {
                        return Err(ToolError::InvalidInput(format!(
                            "program \"{}\" must be a bare executable name, not a path",
                            input.program
                        )));
                    }
                }
            }
            // The verdict layer gates execution before anything runs:
            // denied programs never execute, and `ask` verdicts need
            // operator approval (failing closed without a frontend).
            match self.policy.verdict(&input.program) {
                PolicyVerdict::Allow => {}
                PolicyVerdict::Deny => {
                    return Err(ToolError::InvalidInput(format!(
                        "program \"{}\" is denied by workspace policy (.loom/commands.toml); \
                         pick another approach",
                        input.program
                    )));
                }
                PolicyVerdict::Ask => {
                    let approved = match &self.ask {
                        Some(resolver) => resolver(&input.program).await,
                        None => false,
                    };
                    if !approved {
                        return Err(ToolError::InvalidInput(format!(
                            "program \"{}\" needs operator approval (policy unknown = \"ask\") \
                             and was not approved{}; pick another approach",
                            input.program,
                            if self.ask.is_none() {
                                " — no interactive frontend is attached"
                            } else {
                                ""
                            }
                        )));
                    }
                }
            }
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
                scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
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

/// Renders one completed process as model-visible text, recording the
/// command as an effect (ADR-012). A non-zero exit is a *successful*
/// tool call: the command ran and the model must see the failure, so it
/// is never mapped to [`ToolError::Execution`].
///
/// Three body shapes, best-fit first (the output filters, roadmap
/// item 3):
/// - a **failed** run whose output parses into structured evidence —
///   compiler diagnostics, libtest failures, nextest JUnit, the same
///   parsers `verify` uses — is shaped into that compact block
///   (ADR-014, ADR-017 decision 3), and the raw log it replaced is
///   teed to `ctx`'s artifact store for refetch, so nothing is lost;
/// - a **successful** well-shaped command (see [`crate::reduce`]) with
///   a store attached — the reducer's compact stdout, raw stderr, and
///   the unreduced stdout teed;
/// - anything else — the bounded raw streams. `run_command` is
///   general-purpose, so its output may itself be the answer the
///   model asked for, and a failure is never hidden, only capped.
///
/// Tees are best-effort: without an artifact store, or if the store
/// refuses the write, the result is the bounded raw output it would
/// have been before filters existed.
fn render(
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
    ctx: &ToolContext,
    budgets: &ToolBudgets,
) -> ToolOutput {
    let effects = ToolEffects::none().with_command(CommandEffect::new(
        program,
        args.to_vec(),
        output.status.code(),
    ));
    let raw = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut diagnostics = cargo_diagnostics(&raw);
    // Diagnostics can also be emitted on stderr by some toolchains; parse
    // it only if stdout yielded none, so the two never duplicate.
    if diagnostics.is_empty() {
        diagnostics = cargo_diagnostics(&stderr);
    }
    let mut failures = libtest_failures(&raw);
    if failures.is_empty() {
        failures = nextest_junit(&raw);
    }
    let evidence = Evidence {
        diagnostics,
        failures,
        changed_paths: recent_changes.to_vec(),
    };

    let mut text = format!("$ {program}");
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text.push('\n');
    text.push_str(&status_line(output.status));

    let artifact = if !output.status.success() {
        failure_body(&mut text, ctx, &evidence, &raw, &stderr)
    } else if ctx.artifacts.is_some()
        && let Some(reduction) = reduce::reduce(program, args, &raw)
    {
        reduced_body(&mut text, ctx, &raw, &stderr, &reduction.text)
    } else {
        push_streams(&mut text, &raw, &stderr);
        None
    };
    // A `bwrap:` line on a failed run means the wrapper died *before*
    // the command executed — host policy, not the agent's command — so
    // the output gets actionable remediation instead of reading as a
    // defect in what was asked for. Ordinary failures match nothing
    // and stay unannotated (docs/linux-sandbox.md).
    if !output.status.success()
        && let Some(remediation) = crate::sandbox::bwrap_remediation(&stderr)
    {
        text.push_str("\n--- sandbox ---\n");
        text.push_str(&remediation);
    }
    // Errors live at the tail, so keep both ends (ADR-014). The status line and
    // early output survive at the head; the last lines of stderr survive at the
    // tail; the explicit marker names what was dropped.
    let (text, cut) = budget::head_tail(
        &text,
        budgets.command_head_bytes,
        budgets.command_tail_bytes,
    );
    let mut rendered = if cut {
        ToolOutput::new(text).with_truncated()
    } else {
        ToolOutput::new(text)
    };
    if let Some(reference) = artifact {
        rendered = rendered.with_artifact(reference);
    }
    rendered.with_effects(effects)
}

/// The failure path: when the output parses into structured evidence
/// (2F, ADR-017) the compact block replaces the raw log — and the
/// replaced log is teed to the artifact store so the elided detail stays
/// refetchable (roadmap item 3's "nothing is lost"). Nothing parseable
/// keeps the bounded raw streams, so a failure is never hidden.
fn failure_body(
    text: &mut String,
    ctx: &ToolContext,
    evidence: &Evidence,
    raw: &str,
    stderr: &str,
) -> Option<ArtifactRef> {
    if evidence.is_empty() {
        push_streams(text, raw, stderr);
        return None;
    }
    text.push('\n');
    text.push_str(&evidence.render());
    let mut body = String::from("--- stdout ---\n");
    body.push_str(raw);
    body.push_str("\n--- stderr ---\n");
    body.push_str(stderr);
    tee(text, ctx, &body)
}

/// The reduced-success path: the reducer's compact stdout inline, the
/// raw stderr verbatim beside it, and the unreduced stdout teed to the
/// artifact store for refetch.
fn reduced_body(
    text: &mut String,
    ctx: &ToolContext,
    raw: &str,
    stderr: &str,
    reduced: &str,
) -> Option<ArtifactRef> {
    push_streams(text, reduced, stderr);
    tee(text, ctx, raw)
}

/// The raw path: both bounded streams inline, nothing teed.
fn push_streams(text: &mut String, raw: &str, stderr: &str) {
    if !raw.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(raw);
    }
    if !stderr.is_empty() {
        text.push_str("\n--- stderr ---\n");
        text.push_str(stderr);
    }
}

/// The `kind` recorded on teed command output — the same kind the
/// engine's oversized-output spill uses, so `artifact_read` serves both.
const ARTIFACT_KIND: &str = "tool_output";

/// Stores `body` as an artifact and appends the refetch note, mirroring
/// the engine's spill note (ADR-012 decision 3, ADR-014). Best-effort:
/// without a store, or when the store refuses the write, the caller's
/// inline text is all there is.
fn tee(text: &mut String, ctx: &ToolContext, body: &str) -> Option<ArtifactRef> {
    let store = ctx.artifacts.as_ref()?;
    let reference = store_artifact(store.as_ref(), ARTIFACT_KIND, body.as_bytes()).ok()?;
    let _ = writeln!(
        text,
        "\n... [full output stored as artifact {} ({} bytes); read it with artifact_read]",
        reference.id, reference.bytes
    );
    Some(reference)
}

/// Resolves a bare program name against `PATH`, mirroring the search the
/// OS would perform — used to fail fast (and legibly) before the program
/// enters the sandbox wrapper, which cannot report a missing binary.
///
/// A candidate must be a regular file the user may execute, which is what
/// the kernel itself requires. Without the check a non-executable file
/// sitting early on `PATH` passes here, shadows a later entry that would
/// have worked, and the failure then surfaces from the spawn as a
/// permissions error rather than the legible "not found on PATH".
fn resolve_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `candidate` is a regular file with an execute bit, the way
/// the kernel decides when exec'ing a `PATH` entry.
#[cfg(unix)]
fn is_executable_file(candidate: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Windows has no execute bit; a regular file is the best available
/// approximation.
#[cfg(not(unix))]
fn is_executable_file(candidate: &std::path::Path) -> bool {
    candidate.is_file()
}

/// `exit status: N`, or a signal note when the platform reports none.
fn status_line(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status: {code}"),
        None => String::from("exit status: terminated by signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
    use loom_kernel::store::{ArtifactStore, MemoryArtifactStore};
    use loom_kernel::tool::Tool;
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Output};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn ctx(root: &Path) -> ToolContext {
        ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(30),
        )
    }

    /// A context with no artifact store: the render paths keep the raw
    /// behavior they had before output filters existed.
    fn plain_ctx() -> ToolContext {
        ToolContext::new(
            PathBuf::from("/tmp"),
            CancellationToken::new(),
            Duration::from_secs(30),
        )
    }

    /// A context with an in-memory artifact store attached, plus the store
    /// itself so a test can assert on the teed body.
    fn stored_ctx() -> (ToolContext, Arc<MemoryArtifactStore>) {
        let store = Arc::new(MemoryArtifactStore::new());
        let ctx = plain_ctx().with_artifacts(Some(store.clone()));
        (ctx, store)
    }

    async fn run(tool: &RunCommandTool, root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        tool.call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn allowed_program_runs_and_captures_stdout() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "echo", "args": ["hi", "there"]}),
        )
        .await
        .expect("ok");
        assert!(
            output.content.contains("exit status: 0"),
            "{}",
            output.content
        );
        assert!(output.content.contains("hi there"), "{}", output.content);
        assert_eq!(
            output.effects.commands_run,
            vec![CommandEffect::new(
                "echo",
                vec!["hi".to_string(), "there".to_string()],
                Some(0)
            )]
        );
        assert!(output.effects.files_written.is_empty());
    }

    #[tokio::test]
    async fn writes_outside_the_workspace_are_denied() {
        let (_guard, root) = temp_root();
        // The sandbox's writable set is workspace + temp + cargo home; the
        // real HOME sits outside all three, so a write there must fail.
        let target = std::env::var("HOME")
            .map(|home| std::path::PathBuf::from(home).join("loom-sandbox-escape-probe"))
            .expect("HOME is set");
        let _ = std::fs::remove_file(&target);
        let tool = RunCommandTool::new();
        let output = run(
            &tool,
            &root,
            json!({"program": "touch", "args": [target.to_str().expect("utf8")]}),
        )
        .await
        .expect("the call itself succeeds");
        assert!(
            !target.exists(),
            "the sandbox must deny writes outside the workspace"
        );
        assert!(
            !output.content.contains("exit status: 0"),
            "{}",
            output.content
        );
        let _ = std::fs::remove_file(&target);
    }

    #[tokio::test]
    async fn path_qualified_program_is_normalized_to_its_basename() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(&tool, &root, json!({"program": "/bin/echo"}))
            .await
            .expect("path with a resolvable basename runs");
        assert!(
            output.content.contains("exit status: 0"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn path_qualified_program_without_a_path_entry_is_invalid_input() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["loom-no-such-program"]);
        let err = run(
            &tool,
            &root,
            json!({"program": "/opt/loom-no-such-program"}),
        )
        .await
        .expect_err("path");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("bare executable")));
    }

    #[tokio::test]
    async fn args_as_json_array_text_is_coerced() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "echo", "args": "[\"hello\", \"world\"]"}),
        )
        .await
        .expect("coerced");
        assert!(output.content.contains("hello world"), "{}", output.content);
    }

    #[tokio::test]
    async fn args_as_command_line_text_is_coerced() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "echo", "args": "hello \"wide world\""}),
        )
        .await
        .expect("coerced");
        assert!(
            output.content.contains("hello wide world"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn args_entries_are_coerced_to_strings() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(&tool, &root, json!({"program": "echo", "args": [1, true]}))
            .await
            .expect("coerced");
        assert!(output.content.contains("1 true"), "{}", output.content);
    }

    #[tokio::test]
    async fn args_of_a_nested_shape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let err = run(&tool, &root, json!({"program": "echo", "args": {"a": 1}}))
            .await
            .expect_err("object");
        assert!(
            matches!(err, ToolError::InvalidInput(ref msg) if msg.contains("args must be an array")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn args_with_a_leading_equals_before_an_array_are_coerced() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "echo", "args": "=[\"hello\", \"world\"]"}),
        )
        .await
        .expect("coerced");
        assert!(output.content.contains("hello world"), "{}", output.content);
    }

    #[tokio::test]
    async fn args_as_a_bracketed_list_without_quotes_are_coerced() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "echo", "args": "[hello, wide world]"}),
        )
        .await
        .expect("coerced");
        assert!(
            output.content.contains("hello wide world"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_successful_result() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["false"]);
        let output = run(&tool, &root, json!({"program": "false"}))
            .await
            .expect("ok");
        assert!(
            output.content.contains("exit status: 1"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn missing_program_is_execution_error() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["definitely-not-a-real-program-loom"]);
        let err = run(
            &tool,
            &root,
            json!({"program": "definitely-not-a-real-program-loom"}),
        )
        .await
        .expect_err("missing program");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("cannot run")));
    }

    #[tokio::test]
    async fn large_output_is_bounded_not_buffered_whole() {
        let (_guard, root) = temp_root();
        // ~1.2 MB of stdout: far past the cap, and it must not be held whole.
        let tool = RunCommandTool::with_allowed(["seq"]);
        let output = run(
            &tool,
            &root,
            json!({"program": "seq", "args": ["1", "200000"]}),
        )
        .await
        .expect("ok");
        assert!(output.truncated, "large output should be flagged truncated");
        assert!(
            output.content.contains("bytes omitted"),
            "the cut must be explicit: {}",
            &output.content[..output.content.len().min(200)]
        );
        assert!(
            output.content.len() <= budget::COMMAND_HEAD_BYTES + budget::COMMAND_TAIL_BYTES + 64,
            "content length {} exceeded the bound",
            output.content.len()
        );
        assert!(
            output.content.contains("exit status: 0"),
            "status must survive truncation: {}",
            output.content
        );
        assert!(
            output.content.contains("200000"),
            "the stream tail must survive the per-stream cap: {}",
            &output.content[output.content.len().saturating_sub(200)..]
        );
    }

    #[test]
    fn render_truncates_large_output_keeping_both_ends() {
        let mut stdout = vec![b'x'; MAX_TOOL_OUTPUT_BYTES + 32];
        stdout.extend_from_slice(b"\nLAST LINE: failure at the tail");
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout,
            stderr: Vec::new(),
        };
        let rendered = render(
            "echo",
            &[],
            &output,
            &[],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert!(rendered.truncated);
        assert!(
            rendered.content.contains("bytes omitted"),
            "the cut must be explicit"
        );
        assert!(
            rendered.content.ends_with("LAST LINE: failure at the tail"),
            "the tail must survive: {}",
            &rendered.content[rendered.content.len().saturating_sub(120)..]
        );
        assert!(
            rendered.content.len() <= budget::COMMAND_HEAD_BYTES + budget::COMMAND_TAIL_BYTES + 64
        );
    }

    #[test]
    fn render_includes_both_streams_and_status() {
        let output = Output {
            status: ExitStatus::from_raw(2 << 8),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output,
            &[],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert_eq!(
            rendered.content,
            "$ cargo test\nexit status: 2\n--- stdout ---\nout\n--- stderr ---\nerr"
        );
        assert!(!rendered.truncated);
        assert_eq!(
            rendered.effects.commands_run,
            vec![CommandEffect::new(
                "cargo",
                vec!["test".to_string()],
                Some(2)
            )]
        );
    }

    fn libtest_failure_stdout() -> Vec<u8> {
        let text = "\
running 2 tests
test tests::a ... ok
test tests::b ... FAILED

failures:

---- tests::b stdout ----
thread 'tests::b' panicked at src/lib.rs:12:5:
assertion `left == right` failed

failures:
    tests::b

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured
";
        text.as_bytes().to_vec()
    }

    #[test]
    fn failed_test_run_is_shaped_into_evidence() {
        let output = Output {
            status: ExitStatus::from_raw(1),
            stdout: libtest_failure_stdout(),
            stderr: Vec::new(),
        };
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output,
            &["src/lib.rs".to_owned()],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("failure evidence:"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("failing tests (1):"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("test: tests::b"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("at src/lib.rs:12:5"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("relevant change: src/lib.rs"),
            "change→failure linkage must survive shaping: {}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("---- tests::b stdout ----"),
            "the raw log must not ride along: {}",
            rendered.content
        );
        assert!(!rendered.truncated);
    }

    #[test]
    fn successful_run_keeps_its_streams() {
        // A passing run's output may itself be the answer the model asked
        // for, so it is never replaced by an evidence block.
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: libtest_failure_stdout(),
            stderr: Vec::new(),
        };
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output,
            &[],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("--- stdout ---"),
            "{}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("failure evidence:"),
            "{}",
            rendered.content
        );
    }

    /// 30 default-format `git log` commit blocks: the reducer keeps the
    /// first 25, marks the rest, and the unreduced stdout is teed.
    #[test]
    fn successful_well_shaped_output_is_reduced_and_teed() {
        let mut stdout = String::new();
        for index in 0..30 {
            let _ = write!(
                stdout,
                "commit 00000000000000000000000000000000000000{index:02}\n\
                 Author: A <a@example.com>\n    subject {index}\n"
            );
        }
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.clone().into_bytes(),
            stderr: Vec::new(),
        };
        let (ctx, store) = stored_ctx();
        let rendered = render(
            "git",
            &["log".to_owned()],
            &output,
            &[],
            &ctx,
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("subject 24"),
            "{}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("subject 25"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("[... 5 more commits omitted]"),
            "{}",
            rendered.content
        );
        assert!(rendered.content.contains("read it with artifact_read"));
        let reference = rendered.artifact.expect("the raw must be teed");
        let stored = store.get(&reference.id).expect("get").expect("present");
        assert_eq!(
            stored.body,
            stdout.as_bytes(),
            "the teed body is the raw stream"
        );
        // The command effect and the exit status survive the reduction.
        assert_eq!(
            rendered.effects.commands_run,
            vec![CommandEffect::new("git", vec!["log".to_string()], Some(0))]
        );
        assert!(rendered.content.contains("exit status: 0"));
    }

    #[test]
    fn reduction_needs_a_store() {
        // Without an artifact store the raw stream stands: reducing would
        // drop bytes with no way to refetch them.
        let mut stdout = String::new();
        for index in 0..30 {
            let _ = write!(stdout, "commit {index:040x}\n    subject {index}\n");
        }
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        };
        let rendered = render(
            "git",
            &["log".to_owned()],
            &output,
            &[],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("subject 29"),
            "{}",
            rendered.content
        );
        assert!(rendered.artifact.is_none());
        assert!(!rendered.content.contains("artifact_read"));
    }

    #[test]
    fn evidence_failure_tees_the_replaced_log() {
        let output = Output {
            status: ExitStatus::from_raw(1),
            stdout: libtest_failure_stdout(),
            stderr: Vec::new(),
        };
        let (ctx, store) = stored_ctx();
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output,
            &[],
            &ctx,
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("failure evidence:"),
            "{}",
            rendered.content
        );
        assert!(rendered.content.contains("read it with artifact_read"));
        let reference = rendered.artifact.expect("the replaced log must be teed");
        let stored = store.get(&reference.id).expect("get").expect("present");
        let body = String::from_utf8(stored.body).expect("utf8 body");
        assert!(
            body.contains("---- tests::b stdout ----"),
            "the teed body carries the raw the evidence replaced: {body}"
        );
    }

    #[test]
    fn evidence_failure_without_a_store_is_unchanged() {
        // Pre-filter behavior: the evidence block alone, no refetch note.
        let output = Output {
            status: ExitStatus::from_raw(1),
            stdout: libtest_failure_stdout(),
            stderr: Vec::new(),
        };
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output,
            &[],
            &plain_ctx(),
            &ToolBudgets::default(),
        );
        assert!(
            rendered.content.contains("failure evidence:"),
            "{}",
            rendered.content
        );
        assert!(rendered.artifact.is_none());
        assert!(!rendered.content.contains("artifact_read"));
    }

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<RunInput>(json!({"program": "cargo"}));
    }

    #[tokio::test]
    async fn a_rule_timeout_kills_the_command() {
        let (_guard, root) = temp_root();
        // `sleep` admitted with a 1s wall cap; the group is killed at
        // the cap and the model sees an execution error naming it.
        let policy = CommandPolicy::parse("[[command]]\nprogram = \"sleep\"\ntimeout_secs = 1\n")
            .expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let started = std::time::Instant::now();
        let error = run(&tool, &root, json!({"program": "sleep", "args": ["30"]}))
            .await
            .expect_err("timed out");
        assert!(
            matches!(&error, ToolError::Execution(message) if message.contains("timed out after 1s")),
            "{error}"
        );
        assert!(
            started.elapsed().as_secs() < 10,
            "must not wait for the child"
        );
    }

    #[tokio::test]
    async fn an_unsandboxed_rule_surfaces_a_passthrough_notice() {
        let (_guard, root) = temp_root();
        // The loudest escalation a manifest can grant: run outside the
        // OS sandbox entirely. The deviation rides the output as a
        // `sandbox.passthrough` notice — a fact for the trace, never
        // text the model reads.
        let policy = CommandPolicy::parse("[[command]]\nprogram = \"echo\"\nsandbox = false\n")
            .expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let output = run(&tool, &root, json!({"program": "echo", "args": ["hi"]}))
            .await
            .expect("echo ran");
        assert!(
            output
                .notices
                .iter()
                .any(|notice| notice.category == "sandbox.passthrough"
                    && notice.detail.contains("\"echo\"")),
            "expected a passthrough notice, got {:?}",
            output.notices
        );
    }

    #[tokio::test]
    async fn a_sandboxed_command_attaches_no_notices() {
        let (_guard, root) = temp_root();
        // No rule: the boxed default. On macOS/Linux the command runs
        // OS-confined and the output carries no notices — silence is
        // the signal that the defaults held.
        let tool = RunCommandTool::with_allowed(["echo"]);
        let output = run(&tool, &root, json!({"program": "echo", "args": ["hi"]}))
            .await
            .expect("echo ran");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            output.notices.is_empty(),
            "confined run must stay silent: {:?}",
            output.notices
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(
            output
                .notices
                .iter()
                .any(|n| n.category == "sandbox.passthrough"),
            "no wrapper on this platform: passthrough must be surfaced"
        );
    }

    /// Re-exec harness for the scrub tests: Rust 2024 makes environment
    /// mutation `unsafe`, and this workspace forbids `unsafe`, so the
    /// probe variable is injected by re-spawning the test binary with a
    /// controlled environment (`LOOM_SCRUB_MANIFEST`/`LOOM_SCRUB_SCRIPT`
    /// carry the scenario). The re-entered probe runs the real tool and
    /// reports `SCRUB_RESULT=ok|leak` on stdout; the parent asserts on
    /// that line.
    async fn scrub_probe(manifest: &str, env_pairs: &[(&str, &str)], child_script: &str) {
        if let (Ok(manifest), Ok(script)) = (
            std::env::var("LOOM_SCRUB_MANIFEST"),
            std::env::var("LOOM_SCRUB_SCRIPT"),
        ) {
            let (_guard, root) = temp_root();
            let policy = CommandPolicy::parse(&manifest).expect("policy");
            let tool = RunCommandTool::with_policy(policy);
            let output = run(
                &tool,
                &root,
                json!({"program": "sh", "args": ["-c", script]}),
            )
            .await
            .expect("child run");
            let ok =
                output.content.contains("scrubbed") && !output.content.contains("loom-test-secret");
            println!("SCRUB_RESULT={}", if ok { "ok" } else { "leak" });
            return;
        }
        let mut command = std::process::Command::new(std::env::current_exe().expect("exe"));
        command
            .args([
                "--exact",
                "run_command::tests::scrub_reexec_driver",
                "--nocapture",
            ])
            .env("LOOM_SCRUB_MANIFEST", manifest)
            .env("LOOM_SCRUB_SCRIPT", child_script);
        for (name, value) in env_pairs {
            command.env(name, value);
        }
        let out = command.output().expect("re-exec");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "re-exec'd test failed");
        assert!(
            stdout.contains("SCRUB_RESULT=ok"),
            "the secret leaked into the child: {stdout}"
        );
    }

    #[tokio::test]
    async fn the_credential_floor_hides_provider_keys() {
        // No manifest rule scrubs this name: only the non-negotiable
        // floor can remove it from the child.
        scrub_probe(
            "[[command]]\nprogram = \"sh\"\n",
            &[("OPENROUTER_API_KEY", "loom-test-secret")],
            "if [ -n \"$OPENROUTER_API_KEY\" ]; then echo leaked; else echo scrubbed; fi",
        )
        .await;
    }

    #[tokio::test]
    async fn a_rule_scrubs_extra_environment_names() {
        scrub_probe(
            "[[command]]\nprogram = \"sh\"\nenv_scrub = [\"LOOM_TEST_SECRET\"]\n",
            &[("LOOM_TEST_SECRET", "loom-test-secret")],
            "if [ -n \"$LOOM_TEST_SECRET\" ]; then echo leaked; else echo scrubbed; fi",
        )
        .await;
    }

    // The --exact entry point the scrub tests re-exec into; the harness
    // above does the asserting, this only re-enters the child branch.
    #[tokio::test]
    async fn scrub_reexec_driver() {
        scrub_probe(
            "[[command]]\nprogram = \"sh\"\n",
            &[],
            "if [ -n \"$LOOM_SCRUB_MANIFEST\" ]; then echo leaked; else echo scrubbed; fi",
        )
        .await;
    }

    #[tokio::test]
    async fn a_rule_output_window_caps_the_stream() {
        let (_guard, root) = temp_root();
        let policy =
            CommandPolicy::parse("[[command]]\nprogram = \"seq\"\nmax_output_bytes = 1024\n")
                .expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let output = run(
            &tool,
            &root,
            json!({"program": "seq", "args": ["1", "200000"]}),
        )
        .await
        .expect("ok");
        // The stream window's omission marker survives into the tool
        // output, and the whole result stays near the window size —
        // the 1.2 MB stream never reaches the model.
        assert!(
            output.content.contains("bytes omitted"),
            "the cut must be explicit: {}",
            &output.content[..output.content.len().min(200)]
        );
        assert!(
            output.content.len() < 8 * 1024,
            "content length {} exceeded the window: {}",
            output.content.len(),
            &output.content[..output.content.len().min(300)]
        );
        assert!(
            output.content.contains("200000"),
            "the tail must survive the window: {}",
            &output.content[output.content.len().saturating_sub(200)..]
        );
    }

    #[test]
    fn the_spec_lists_the_policys_programs() {
        let policy = CommandPolicy::parse("[[command]]\nprogram = \"git\"\n").expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        assert!(
            tool.spec().description.contains("Escalated,")
                && tool.spec().description.contains("`git`"),
            "{}",
            tool.spec().description
        );
    }

    /// The execute bit is what the kernel requires of a `PATH` entry, so
    /// a data file shadowing a real program must not resolve.
    #[cfg(unix)]
    #[test]
    fn a_path_candidate_must_be_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let (_guard, root) = temp_root();
        let executable = root.join("exe");
        std::fs::write(&executable, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        assert!(is_executable_file(&executable));

        let data = root.join("data.txt");
        std::fs::write(&data, b"notes").expect("write");
        assert!(!is_executable_file(&data), "a data file is not a program");
        assert!(!is_executable_file(&root), "a directory is not a program");
        assert!(
            !is_executable_file(&root.join("absent")),
            "a missing candidate is not a program"
        );
    }

    #[tokio::test]
    async fn denied_program_is_refused_before_execution() {
        let (_guard, root) = temp_root();
        // `echo` denied while unknown stays allow: the refusal names
        // the policy, proving the verdict fired (not a PATH miss — the
        // verdict check runs first, even for programs that do exist).
        let policy = CommandPolicy::parse("[[deny]]\nprogram = \"echo\"\n").expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let error = run(&tool, &root, json!({"program": "echo", "args": ["hi"]}))
            .await
            .expect_err("denied");
        assert!(
            matches!(&error, ToolError::InvalidInput(m) if m.contains("denied by workspace policy")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn strict_unknown_denies_unruled_programs_but_rules_still_run() {
        let (_guard, root) = temp_root();
        let policy =
            CommandPolicy::parse("[policy]\nunknown = \"deny\"\n[[command]]\nprogram = \"echo\"\n")
                .expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let refused = run(&tool, &root, json!({"program": "sleep", "args": ["0"]}))
            .await
            .expect_err("unruled denied");
        assert!(
            matches!(&refused, ToolError::InvalidInput(m) if m.contains("denied by workspace policy")),
            "{refused}"
        );
        let admitted = run(&tool, &root, json!({"program": "echo", "args": ["ok"]}))
            .await
            .expect("ruled programs are unaffected");
        assert!(admitted.content.contains("ok"), "{}", admitted.content);
    }

    #[tokio::test]
    async fn ask_fails_closed_without_a_frontend() {
        let (_guard, root) = temp_root();
        let policy = CommandPolicy::parse("[policy]\nunknown = \"ask\"\n").expect("policy");
        let tool = RunCommandTool::with_policy(policy);
        let error = run(&tool, &root, json!({"program": "echo", "args": ["hi"]}))
            .await
            .expect_err("no resolver");
        assert!(
            matches!(&error, ToolError::InvalidInput(m)
                if m.contains("operator approval") && m.contains("no interactive frontend")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn ask_resolver_decides_execution() {
        use std::sync::Arc;
        let (_guard, root) = temp_root();
        let policy = CommandPolicy::parse("[policy]\nunknown = \"ask\"\n").expect("policy");
        // Approve only `echo`; everything else is declined by the
        // operator, which the model sees as a refusal.
        let tool = RunCommandTool::with_policy(policy).with_ask_resolver(Arc::new(|program| {
            let program = program.to_owned();
            Box::pin(async move { program == "echo" })
        }));
        let approved = run(&tool, &root, json!({"program": "echo", "args": ["yes"]}))
            .await
            .expect("approved runs");
        assert!(approved.content.contains("yes"), "{}", approved.content);
        let declined = run(&tool, &root, json!({"program": "sleep", "args": ["0"]}))
            .await
            .expect_err("declined");
        assert!(
            matches!(&declined, ToolError::InvalidInput(m) if m.contains("was not approved")),
            "{declined}"
        );
    }

    #[test]
    fn spec_mentions_admission_only_when_the_policy_narrows() {
        let shipped = RunCommandTool::new();
        assert!(
            !shipped.spec().description.contains("Unrecognized programs"),
            "default spec stays byte-identical"
        );
        let strict = RunCommandTool::with_policy(
            CommandPolicy::parse("[policy]\nunknown = \"deny\"\n[[deny]]\nprogram = \"curl\"\n")
                .expect("policy"),
        );
        assert!(
            strict
                .spec()
                .description
                .contains("Unrecognized programs are refused.")
        );
        assert!(strict.spec().description.contains("Never run: `curl`."));
    }
}
