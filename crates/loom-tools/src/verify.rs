//! The `verify` tool (2F, ADR-017 decision 4).
//!
//! Exposes the verification runtime to the model. It runs a deterministic
//! `cargo` command, then compresses the result through
//! [`evidence`](crate::evidence) so the model receives the failing test, its
//! assertion, the source location, and the recent change it belongs to —
//! rather than an unbounded process log. The full output still overflows to an
//! artifact when oversized (ADR-012/014), like any tool result.
//!
//! Level selection here is deliberately coarse and deterministic: `check`
//! compiles without running tests, `test` compiles and runs them. Both use
//! `--message-format=json`, which yields stable compiler diagnostics while
//! libtest's human lines remain parseable; an escalation is a second call, not
//! a speculative broader run (ADR-017 decision 2).

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

/// Programs this tool will execute. Anything else is rejected before a process
/// is created.
const ALLOWED: &[&str] = &["cargo"];

/// The `verify` tool.
#[derive(Debug, Default)]
pub struct VerifyTool;

/// How much verification to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Scope {
    /// Compile only; no tests.
    Check,
    /// Compile and run tests.
    #[default]
    Test,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct VerifyInput {
    /// `check` compiles only; `test` compiles and runs tests.
    scope: Scope,
}

impl Scope {
    /// The `cargo` arguments this scope runs.
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Check => &["check", "--message-format=json"],
            Self::Test => &["test", "--message-format=json"],
        }
    }
}

impl VerifyTool {
    /// The shipped tool.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Tool for VerifyTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "verify".to_owned(),
            description: "Runs a bounded cargo verification and returns structured \
                          failure evidence: compiler diagnostics (`check`) or diagnostics \
                          plus failing tests (`test`), linked to files this task changed. \
                          Prefer the smallest scope that can prove the change; output is \
                          never a full log."
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
            let args: Vec<String> = input
                .scope
                .args()
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect();
            let program = ALLOWED[0];
            let output = crate::exec::run_bounded(program, &args, ctx).await?;
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                program,
                args.clone(),
                output.status.code(),
            ));
            let recent_changes = ctx.recent_changes();
            Ok(render(program, &args, &output, &recent_changes).with_effects(effects))
        })
    }
}

/// Shapes one verification run into a bounded, structured [`ToolOutput`].
fn render(
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
) -> ToolOutput {
    let raw = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut diagnostics = cargo_diagnostics(&raw);
    // Diagnostics can also be emitted on stderr by some toolchains; parse it
    // only if stdout yielded none, so the two never duplicate.
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

    let mut text = format!("$ {program} {}", args.join(" "));
    text.push('\n');
    text.push_str(&status_line(output.status));
    text.push('\n');

    if evidence.is_empty() {
        if output.status.success() {
            // A clean run is a pass, not a log: report the harness summary
            // line (or a fixed statement) and omit the raw output, so the
            // success path honours "Output is never a full log".
            let summary = pass_summary(&raw)
                .unwrap_or_else(|| String::from("no diagnostics, no failing tests"));
            text.push_str(&summary);
            text.push('\n');
        } else {
            // Nothing parseable but the command failed: fall back to the
            // bounded raw output so the failure is never hidden, just capped.
            if !raw.trim().is_empty() {
                text.push_str("--- output ---\n");
                text.push_str(&raw);
            }
            if !stderr.trim().is_empty() {
                text.push_str("--- stderr ---\n");
                text.push_str(&stderr);
            }
        }
    } else {
        text.push_str(&evidence.render());
    }

    let (text, cut) = budget::head_tail(
        &text,
        budget::COMMAND_HEAD_BYTES,
        budget::COMMAND_TAIL_BYTES,
    );
    let output = ToolOutput::new(text);
    if cut { output.with_truncated() } else { output }
}

/// The libtest summary line (`test result: ok. …`), if present, so a passing
/// run reports counts rather than the raw log.
fn pass_summary(raw: &str) -> Option<String> {
    raw.lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("test result:"))
        .map(str::to_owned)
}

/// `exit status: N`, or the signal form. A non-zero exit is a successful tool
/// call: the model must see the failure.
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
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx(root: PathBuf) -> ToolContext {
        ToolContext::new(root, CancellationToken::new(), Duration::from_secs(30))
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn render_compresses_diagnostics_and_links_changes() {
        let stdout = "\
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{\"code\":\"E0308\"},\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":3,\"column_start\":9,\"is_primary\":true}]}}
";
        let rendered = render(
            "cargo",
            &["check".to_owned(), "--message-format=json".to_owned()],
            &output(101, stdout, ""),
            &["src/lib.rs".to_owned()],
        );
        assert!(rendered.content.contains("exit status: 101"));
        assert!(
            rendered
                .content
                .contains("[error] mismatched types (E0308) at src/lib.rs:3:9")
        );
        assert!(rendered.content.contains("relevant change: src/lib.rs"));
    }

    #[test]
    fn render_reports_a_compact_pass_on_success() {
        let rendered = render(
            "cargo",
            &["test".to_owned()],
            &output(
                0,
                "running 2 tests\ntest a ... ok\ntest b ... ok\ntest result: ok. 2 passed; 0 failed\n",
                "",
            ),
            &["src/lib.rs".to_owned()],
        );
        assert!(
            rendered.content.contains("exit status: 0"),
            "{}",
            rendered.content
        );
        assert!(
            rendered
                .content
                .contains("test result: ok. 2 passed; 0 failed"),
            "{}",
            rendered.content
        );
        assert!(
            !rendered.content.contains("--- output ---"),
            "{}",
            rendered.content
        );
    }

    #[test]
    fn render_falls_back_to_raw_when_nothing_parses() {
        let rendered = render(
            "cargo",
            &["check".to_owned()],
            &output(1, "Compiling\nsome non-json noise\n", ""),
            &[],
        );
        assert!(rendered.content.contains("--- output ---"));
        assert!(rendered.content.contains("some non-json noise"));
    }

    #[test]
    fn spec_exposes_scope_and_has_no_unknown_fields() {
        let tool = VerifyTool::new();
        let spec = tool.spec();
        assert_eq!(spec.name, "verify");
        crate::schema::agreement::check::<VerifyInput>(json!({"scope": "check"}));
    }

    #[test]
    fn path_qualified_and_unknown_programs_are_rejected() {
        // The tool has no program input; the guard is that `verify` only ever
        // runs `cargo`. This test pins the constant.
        assert_eq!(ALLOWED, &["cargo"]);
    }

    #[test]
    fn scope_maps_to_expected_cargo_arguments() {
        assert_eq!(Scope::Check.args(), &["check", "--message-format=json"]);
        assert_eq!(Scope::Test.args(), &["test", "--message-format=json"]);
        assert_eq!(Scope::default(), Scope::Test);
    }

    #[tokio::test]
    async fn missing_program_surfaces_as_execution_error() {
        // A workspace root that does not exist makes `cargo` fail to spawn.
        let root = PathBuf::from("/nonexistent-loom-verify-root");
        let tool = VerifyTool::new();
        let error = tool
            .call(json!({"scope": "check"}), &ctx(root))
            .await
            .expect_err("spawn must fail");
        assert!(matches!(error, ToolError::Execution(_)));
    }
}
