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

use std::process::Stdio;
use std::sync::OnceLock;

use futures::future::BoxFuture;
use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::budget;

/// Programs Iteration 1 will execute. Anything else is rejected as
/// invalid input before a process is created.
const DEFAULT_ALLOWED: &[&str] = &["cargo"];

/// Per-stream byte budget buffered before the remainder is discarded, so a
/// chatty program cannot exhaust memory. `render` still enforces the combined
/// head+tail budget (ADR-014) on the model-visible text.
const STREAM_BUFFER_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

impl Tool for RunCommandTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "run_command".to_owned(),
            description: "Runs an allow-listed program in the workspace root and returns \
                          its combined stdout/stderr and exit status. No shell is involved; \
                          pass the program and each argument separately. Iteration 1 allows \
                          `cargo` (e.g. build, test, fmt, clippy)."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "program": {
                        "type": "string",
                        "description": "Executable name (no path). Allowed: cargo."
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Arguments passed to the program, one array entry each."
                    }
                },
                "required": ["program"],
                "additionalProperties": false
            }),
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
            let mut child = tokio::process::Command::new(&input.program)
                .args(&input.args)
                .current_dir(&ctx.workspace_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|err| {
                    ToolError::Execution(format!("cannot run \"{}\": {err}", input.program))
                })?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::Execution(String::from("cannot capture stdout pipe")))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::Execution(String::from("cannot capture stderr pipe")))?;
            // Read both streams concurrently to EOF. Each buffer is capped and
            // the remainder is discarded, so the child never blocks on a full
            // pipe and memory stays bounded regardless of output size.
            let (stdout, stderr) = futures::join!(read_capped(stdout), read_capped(stderr));
            let stdout =
                stdout.map_err(|err| ToolError::Execution(format!("cannot read output: {err}")))?;
            let stderr =
                stderr.map_err(|err| ToolError::Execution(format!("cannot read output: {err}")))?;
            let status = child.wait().await.map_err(|err| {
                ToolError::Execution(format!("cannot run \"{}\": {err}", input.program))
            })?;
            let output = std::process::Output {
                status,
                stdout,
                stderr,
            };
            Ok(render(&input.program, &input.args, &output))
        })
    }
}

/// Reads a child stream to EOF, keeping at most [`STREAM_BUFFER_BYTES`] and
/// discarding the rest so the child cannot block writing to a full pipe.
async fn read_capped<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if kept.len() < STREAM_BUFFER_BYTES {
            let room = STREAM_BUFFER_BYTES - kept.len();
            kept.extend_from_slice(&chunk[..room.min(read)]);
        }
    }
    Ok(kept)
}

/// Renders one completed process as model-visible text, recording the
/// command as an effect (ADR-012). A non-zero exit is a *successful*
/// tool call: the command ran and the model must see the failure, so it
/// is never mapped to [`ToolError::Execution`].
fn render(program: &str, args: &[String], output: &std::process::Output) -> ToolOutput {
    let effects = ToolEffects::none().with_command(CommandEffect::new(
        program,
        args.to_vec(),
        output.status.code(),
    ));
    let mut text = format!("$ {program}");
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text.push('\n');
    text.push_str(&status_line(output.status));
    if !output.stdout.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        text.push_str("\n--- stderr ---\n");
        text.push_str(&String::from_utf8_lossy(&output.stderr));
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
    use loom_kernel::tool::Tool;
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
        let rendered = render("echo", &[], &output);
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
        let rendered = render("cargo", &["test".to_owned()], &output);
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
}
