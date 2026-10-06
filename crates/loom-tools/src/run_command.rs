//! The `run_command` tool: bounded, allow-listed process execution.
//!
//! Mutating (architecture §10). There is **no shell**: the program and
//! its arguments are passed to [`tokio::process::Command`] directly, so
//! argument content can never be re-interpreted as shell syntax. The
//! program itself must be in an explicit allow-list — Iteration 1 allows
//! only the Rust build driver. The child runs with the workspace root as
//! its working directory and is killed when the call future is dropped
//! (timeout or cancellation), so a runaway command cannot outlive the
//! turn.
//!
//! A timeout or cancellation kills the direct child (`kill_on_drop`) but not
//! processes it spawned: tearing down the whole process group needs an
//! `unsafe` `libc::killpg`, and this crate is `unsafe_code = "forbid"`. A
//! timeout during `cargo build` can therefore leave `rustc` children running
//! until they finish. Process-group teardown belongs with the sandbox/policy
//! iteration (roadmap §5.3).
//!
//! Execution policy proper (resource limits, broader allow-lists,
//! sandboxing) is a later iteration's seam; this is the minimal safe
//! shape that lets Loom verify its own edits today.

use std::sync::OnceLock;

use futures::future::BoxFuture;
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::budget;
use crate::evidence::{Evidence, cargo_diagnostics, libtest_failures, nextest_junit};

/// Programs Iteration 1 will execute. Anything else is rejected as
/// invalid input before a process is created.
const DEFAULT_ALLOWED: &[&str] = &["cargo"];

/// Runs an allow-listed program in the workspace root and returns its
/// combined output and exit status.
#[derive(Debug)]
pub struct RunCommandTool {
    allowed: Vec<String>,
}

impl Default for RunCommandTool {
    fn default() -> Self {
        Self {
            allowed: DEFAULT_ALLOWED
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

impl RunCommandTool {
    /// The shipped tool: `cargo` only.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool allowing a different program set (tests, future
    /// allow-list policy).
    pub fn with_allowed<I, S>(allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct RunInput {
    /// Executable name, no path; allowed: cargo.
    program: String,
    /// Arguments, one array entry each.
    #[serde(default)]
    args: Vec<String>,
}

impl Tool for RunCommandTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "run_command".to_owned(),
            description: "Runs an allow-listed program in the workspace root and returns \
                          its combined stdout/stderr and exit status. No shell: pass the \
                          program and each argument separately. Allowed: `cargo`."
                .to_owned(),
            parameters: crate::schema::parameters::<RunInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: RunInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            if input.program.contains('/') || input.program.contains('\\') {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{}\" must be a bare executable name, not a path",
                    input.program
                )));
            }
            if !self.allowed.iter().any(|name| name == &input.program) {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{}\" is not allowed; allowed: {}",
                    input.program,
                    self.allowed.join(", ")
                )));
            }
            let output = crate::exec::run_bounded(&input.program, &input.args, ctx).await?;
            let recent_changes = ctx.recent_changes();
            Ok(render(
                &input.program,
                &input.args,
                &output,
                &recent_changes,
            ))
        })
    }
}

/// Renders one completed process as model-visible text, recording the
/// command as an effect (ADR-012). A non-zero exit is a *successful*
/// tool call: the command ran and the model must see the failure, so it
/// is never mapped to [`ToolError::Execution`].
///
/// A **failed** run whose output parses into structured evidence —
/// compiler diagnostics, libtest failures, nextest JUnit, the same
/// parsers `verify` uses — is shaped into that compact block instead of
/// the raw log (ADR-014, ADR-017 decision 3): the failing test, its
/// assertion, its location, and the recent change it links to. A
/// successful run keeps its bounded streams: `run_command` is
/// general-purpose, so its output may itself be the answer the model
/// asked for. A failed run with nothing parseable falls back to the
/// bounded raw output, so a failure is never hidden, only capped.
fn render(
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
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
    if !output.status.success() && !evidence.is_empty() {
        text.push('\n');
        text.push_str(&evidence.render());
    } else {
        if !raw.is_empty() {
            text.push_str("\n--- stdout ---\n");
            text.push_str(&raw);
        }
        if !stderr.is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(&stderr);
        }
    }
    // Errors live at the tail, so keep both ends (ADR-014). The status line and
    // early output survive at the head; the last lines of stderr survive at the
    // tail; the explicit marker names what was dropped.
    let (text, cut) = budget::head_tail(
        &text,
        budget::COMMAND_HEAD_BYTES,
        budget::COMMAND_TAIL_BYTES,
    );
    let rendered = if cut {
        ToolOutput::new(text).with_truncated()
    } else {
        ToolOutput::new(text)
    };
    rendered.with_effects(effects)
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
    use loom_kernel::tool::Tool;
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Output};
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
    async fn disallowed_program_is_invalid_input() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["echo"]);
        let err = run(&tool, &root, json!({"program": "rm", "args": ["-rf", "."]}))
            .await
            .expect_err("disallowed");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("not allowed")));
    }

    #[tokio::test]
    async fn path_qualified_program_is_invalid_input() {
        let (_guard, root) = temp_root();
        let tool = RunCommandTool::with_allowed(["/bin/echo"]);
        let err = run(&tool, &root, json!({"program": "/bin/echo"}))
            .await
            .expect_err("path");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("bare executable")));
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
        let rendered = render("echo", &[], &output, &[]);
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
        let rendered = render("cargo", &["test".to_owned()], &output, &[]);
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
        let rendered = render("cargo", &["test".to_owned()], &output, &[]);
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

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<RunInput>(json!({"program": "cargo"}));
    }
}
