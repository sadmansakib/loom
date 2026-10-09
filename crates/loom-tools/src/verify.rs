//! The `verify` tool (2F, ADR-017 decision 4).
//!
//! Exposes the verification runtime to the model. It detects the
//! workspace's build system from root marker files — or takes an explicit
//! `ecosystem` argument — runs that ecosystem's deterministic check/test
//! command, then compresses the result through
//! [`evidence`](crate::evidence) so the model receives the failing test,
//! its assertion, the source location, and the recent change it belongs
//! to — rather than an unbounded process log. The full output still
//! overflows to an artifact when oversized (ADR-012/014), like any tool
//! result.
//!
//! Level selection here is deliberately coarse and deterministic: `check`
//! compiles (or statically verifies) without running tests, `test` runs
//! them. The command comes from a fixed per-ecosystem table — never from
//! model input — so no allow-list decision is needed; an escalation is a
//! second call, not a speculative broader run (ADR-017 decision 2).
//! Ecosystems without a build-agnostic `check` (npm) reject the scope
//! rather than guess a script.

use std::path::Path;
use std::sync::OnceLock;

use futures::future::BoxFuture;
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::budget;
use crate::evidence::{
    Evidence, cargo_diagnostics, go_diagnostics, go_failures, libtest_failures, nextest_junit,
    pytest_failures, python_diagnostics,
};
use crate::policy::CommandPolicy;

/// The build system a verification runs for. Variant meanings live in
/// the command table and the `ecosystem` field docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ecosystem {
    Cargo,
    Go,
    Python,
    Npm,
    Make,
}

/// Marker files that identify an ecosystem, in detection-priority order:
/// the first marker present wins, so a mixed workspace stays
/// deterministic. Mirrors the facts scanner's `BUILD_SYSTEMS` names.
const MARKERS: &[(&str, Ecosystem)] = &[
    ("Cargo.toml", Ecosystem::Cargo),
    ("package.json", Ecosystem::Npm),
    ("pyproject.toml", Ecosystem::Python),
    ("setup.py", Ecosystem::Python),
    ("go.mod", Ecosystem::Go),
    ("Makefile", Ecosystem::Make),
    ("makefile", Ecosystem::Make),
];

/// Detects the workspace's ecosystem from root marker files.
pub(crate) fn detect_ecosystem(root: &Path) -> Option<Ecosystem> {
    MARKERS
        .iter()
        .find(|(marker, _)| root.join(marker).is_file())
        .map(|(_, ecosystem)| *ecosystem)
}

impl Ecosystem {
    /// The program and arguments this scope runs. The program always
    /// comes from this table — never from model input.
    pub(crate) fn command(self, scope: Scope) -> Result<(&'static str, Vec<String>), ToolError> {
        fn owned(args: &[&str]) -> Vec<String> {
            args.iter().map(|arg| (*arg).to_owned()).collect()
        }
        match (self, scope) {
            (Self::Cargo, Scope::Check) => {
                Ok(("cargo", owned(&["check", "--message-format=json"])))
            }
            (Self::Cargo, Scope::Test) => Ok(("cargo", owned(&["test", "--message-format=json"]))),
            (Self::Go, Scope::Check) => Ok(("go", owned(&["vet", "./..."]))),
            (Self::Go, Scope::Test) => Ok(("go", owned(&["test", "./..."]))),
            (Self::Python, Scope::Check) => {
                Ok(("python3", owned(&["-m", "compileall", "-q", "."])))
            }
            (Self::Python, Scope::Test) => Ok(("python3", owned(&["-m", "pytest"]))),
            (Self::Npm, Scope::Test) => Ok(("npm", owned(&["test"]))),
            (Self::Make, scope) => Ok(("make", owned(&[scope.label()]))),
            (Self::Npm, Scope::Check) => Err(ToolError::InvalidInput(String::from(
                "npm has no build-agnostic check; use the test scope",
            ))),
        }
    }
}

/// The `verify` tool.
#[derive(Debug, Default)]
pub struct VerifyTool {
    /// Escalation rules for the build drivers (`cargo`, `go`, …) —
    /// drivers without a rule run inside the boxed default sandbox.
    policy: CommandPolicy,
}

/// How much verification to run.
///
/// Shared with the mutating tools' fused `verify` argument (action
/// fusion), so one enum owns the vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    /// Compile only.
    Check,
    /// Compile and run tests.
    #[default]
    Test,
}

impl Scope {
    /// The scope's wire name, used where an ecosystem takes it as its
    /// own argument (Make targets).
    fn label(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Test => "test",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct VerifyInput {
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
        }
    }

    /// A tool under an explicit policy (the engine's entry point after
    /// loading a workspace manifest), so build drivers escalate exactly
    /// like `run_command` programs.
    #[must_use]
    pub fn with_policy(policy: CommandPolicy) -> Self {
        Self { policy }
    }
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
            // Drivers escalate through the same policy rules as
            // `run_command`; a driver without a rule runs inside the
            // boxed default sandbox (network off).
            let rule = self.policy.rule(program);
            let limits = crate::exec::ExecLimits {
                stream_window: rule.and_then(|rule| rule.max_output_bytes),
                scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
            };
            let grants = crate::sandbox::SandboxGrants::from_rule(rule);
            let sandbox = crate::sandbox::Sandbox::resolve(&ctx.workspace_root, grants);
            let output = crate::exec::run_bounded(program, &args, ctx, &limits, &sandbox).await?;
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                program,
                args.clone(),
                output.status.code(),
            ));
            let recent_changes = ctx.recent_changes();
            Ok(render(ecosystem, program, &args, &output, &recent_changes).with_effects(effects))
        })
    }
}

/// Shapes one verification run into a bounded, structured [`ToolOutput`].
pub(crate) fn render(
    ecosystem: Ecosystem,
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
) -> ToolOutput {
    let raw = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let evidence = parse_evidence(ecosystem, &raw, &stderr, recent_changes);

    let mut text = format!("$ {program} {}", args.join(" "));
    text.push('\n');
    text.push_str(&status_line(output.status));
    text.push('\n');

    if evidence.is_empty() {
        if output.status.success() {
            // A clean run is a pass, not a log: report the harness summary
            // line (or a fixed statement) and omit the raw output, so the
            // success path honours "Output is never a full log".
            let summary = pass_summary(ecosystem, &raw)
                .unwrap_or_else(|| String::from("no diagnostics, no failing tests"));
            text.push_str(&summary);
            text.push('\n');
        } else {
            // Nothing parseable but the command failed: fall back to the
            // bounded raw output so the failure is never hidden, just capped.
            push_raw_fallback(&mut text, &raw, &stderr);
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

/// The first list if non-empty, otherwise the fallback's.
fn non_empty<T>(first: Vec<T>, fallback: impl FnOnce() -> Vec<T>) -> Vec<T> {
    if first.is_empty() { fallback() } else { first }
}

/// Parses one run's output streams into structured [`Evidence`].
///
/// Parsers are per ecosystem, each with a stdout-then-stderr fallback so
/// the two streams never duplicate one failure (Python inverts the
/// diagnostic order: compileall reports on stderr, pytest on stdout).
fn parse_evidence(
    ecosystem: Ecosystem,
    raw: &str,
    stderr: &str,
    recent_changes: &[String],
) -> Evidence {
    let mut evidence = Evidence {
        changed_paths: recent_changes.to_vec(),
        ..Evidence::default()
    };
    match ecosystem {
        Ecosystem::Cargo => {
            evidence.diagnostics = non_empty(cargo_diagnostics(raw), || cargo_diagnostics(stderr));
            evidence.failures = non_empty(libtest_failures(raw), || nextest_junit(raw));
        }
        Ecosystem::Go => {
            evidence.diagnostics = non_empty(go_diagnostics(raw), || go_diagnostics(stderr));
            evidence.failures = go_failures(raw);
        }
        Ecosystem::Python => {
            // compileall reports on stderr; pytest prints on stdout.
            evidence.diagnostics =
                non_empty(python_diagnostics(stderr), || python_diagnostics(raw));
            evidence.failures = non_empty(pytest_failures(raw), || pytest_failures(stderr));
        }
        // No stable human format for these runners: the bounded raw
        // fallback in `render` is the honest representation.
        Ecosystem::Npm | Ecosystem::Make => {}
    }
    evidence
}

/// Appends the capped raw streams, labeled, for output nothing parsed.
fn push_raw_fallback(text: &mut String, raw: &str, stderr: &str) {
    if !raw.trim().is_empty() {
        text.push_str("--- output ---\n");
        text.push_str(raw);
    }
    if !stderr.trim().is_empty() {
        text.push_str("--- stderr ---\n");
        text.push_str(stderr);
    }
}

/// The harness's own one-line summary of a passing run, if present, so a
/// pass reports counts rather than the raw log.
fn pass_summary(ecosystem: Ecosystem, raw: &str) -> Option<String> {
    let found = match ecosystem {
        // libtest: `test result: ok. 2 passed; 0 failed`
        Ecosystem::Cargo => raw
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| line.starts_with("test result:"))
            .map(str::to_owned),
        // pytest: the closing `==== 2 passed in 0.01s ====` rule line.
        Ecosystem::Python => raw
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| line.starts_with('=') && line.ends_with('=') && line.contains(" passed"))
            .map(|line| line.trim_matches('=').trim().to_owned()),
        // go test: the last per-package `ok  <pkg>  <dur>` line.
        Ecosystem::Go => raw
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| line.starts_with("ok "))
            .map(|line| line.replace('\t', " ")),
        Ecosystem::Npm | Ecosystem::Make => None,
    };
    found.filter(|summary| !summary.is_empty())
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
            Ecosystem::Cargo,
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
            Ecosystem::Cargo,
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
            Ecosystem::Cargo,
            "cargo",
            &["check".to_owned()],
            &output(1, "Compiling\nsome non-json noise\n", ""),
            &[],
        );
        assert!(rendered.content.contains("--- output ---"));
        assert!(rendered.content.contains("some non-json noise"));
    }

    #[test]
    fn render_parses_go_vet_and_go_test_failures() {
        let stdout = "\
--- FAIL: TestSum (0.00s)
    main_test.go:12: sum(2, 2) = 5, want 4
FAIL
FAIL\texample.com/m\t0.012s
";
        let stderr = "# example.com/m [example.com/m.test]\n./main.go:9:2: undefined: z\n";
        let rendered = render(
            Ecosystem::Go,
            "go",
            &["test".to_owned(), "./...".to_owned()],
            &output(1, stdout, stderr),
            &[],
        );
        assert!(
            rendered.content.contains("exit status: 1"),
            "{}",
            rendered.content
        );
        assert!(
            rendered
                .content
                .contains("- [error] undefined: z at ./main.go:9:2"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("- test: TestSum"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("message: sum(2, 2) = 5, want 4"),
            "{}",
            rendered.content
        );
    }

    #[test]
    fn render_reports_a_pytest_pass_summary() {
        let rendered = render(
            Ecosystem::Python,
            "python3",
            &["-m".to_owned(), "pytest".to_owned()],
            &output(
                0,
                "....\n================== 4 passed in 0.02s ==================\n",
                "",
            ),
            &[],
        );
        assert!(
            rendered.content.contains("4 passed in 0.02s"),
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
    fn render_parses_pytest_failures_from_sections() {
        let stdout = "\
============================= test session starts ==============================
FAILURES
_______________________________ TestSum.test_sum _______________________________

self = <tests.test_math.TestSum object at 0x1024>

    def test_sum():
>       assert sum(2, 2) == 5
E       assert 4 == 5

tests/test_math.py:8: AssertionError
=========================== short test summary info ============================
FAILED tests/test_math.py::TestSum::test_sum - assert 4 == 5
============================== 1 failed in 0.01s ===============================
";
        let rendered = render(
            Ecosystem::Python,
            "python3",
            &["-m".to_owned(), "pytest".to_owned()],
            &output(1, stdout, ""),
            &[],
        );
        assert!(
            rendered.content.contains("- test: TestSum.test_sum"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("assert 4 == 5"),
            "{}",
            rendered.content
        );
        assert!(
            rendered.content.contains("at tests/test_math.py:8:1"),
            "{}",
            rendered.content
        );
    }

    #[test]
    fn make_failures_fall_back_to_bounded_raw_output() {
        let rendered = render(
            Ecosystem::Make,
            "make",
            &["check".to_owned()],
            &output(2, "", "make: *** [check] Error 2\n"),
            &[],
        );
        assert!(
            rendered.content.contains("--- stderr ---"),
            "{}",
            rendered.content
        );
        assert!(rendered.content.contains("Error 2"), "{}", rendered.content);
    }

    #[test]
    fn spec_exposes_scope_and_has_no_unknown_fields() {
        let tool = VerifyTool::new();
        let spec = tool.spec();
        assert_eq!(spec.name, "verify");
        crate::schema::agreement::check::<VerifyInput>(json!({"scope": "check"}));
    }

    #[test]
    fn command_table_maps_every_ecosystem_and_scope() {
        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).to_owned()).collect()
        }
        assert_eq!(
            Ecosystem::Cargo
                .command(Scope::Check)
                .expect("cargo check plans"),
            ("cargo", args(&["check", "--message-format=json"]))
        );
        assert_eq!(
            Ecosystem::Cargo
                .command(Scope::Test)
                .expect("cargo test plans"),
            ("cargo", args(&["test", "--message-format=json"]))
        );
        assert_eq!(
            Ecosystem::Go.command(Scope::Check).expect("go vet plans"),
            ("go", args(&["vet", "./..."]))
        );
        assert_eq!(
            Ecosystem::Go.command(Scope::Test).expect("go test plans"),
            ("go", args(&["test", "./..."]))
        );
        assert_eq!(
            Ecosystem::Python
                .command(Scope::Check)
                .expect("compileall plans"),
            ("python3", args(&["-m", "compileall", "-q", "."]))
        );
        assert_eq!(
            Ecosystem::Python
                .command(Scope::Test)
                .expect("pytest plans"),
            ("python3", args(&["-m", "pytest"]))
        );
        assert_eq!(
            Ecosystem::Npm.command(Scope::Test).expect("npm test plans"),
            ("npm", args(&["test"]))
        );
        assert_eq!(
            Ecosystem::Make
                .command(Scope::Check)
                .expect("make check plans"),
            ("make", args(&["check"]))
        );
        assert_eq!(
            Ecosystem::Make
                .command(Scope::Test)
                .expect("make test plans"),
            ("make", args(&["test"]))
        );
        // npm has no build-agnostic check: the table refuses rather than
        // guessing a program.
        assert!(matches!(
            Ecosystem::Npm.command(Scope::Check),
            Err(ToolError::InvalidInput(_))
        ));
        assert_eq!(Scope::default(), Scope::Test);
    }

    #[test]
    fn detection_follows_marker_priority_and_requires_a_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("go.mod"), "module example.com/m\n").expect("write go.mod");
        std::fs::write(dir.path().join("package.json"), "{}\n").expect("write package.json");
        // package.json outranks go.mod in the marker table.
        assert_eq!(detect_ecosystem(dir.path()), Some(Ecosystem::Npm));

        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write Cargo.toml");
        // Cargo outranks everything.
        assert_eq!(detect_ecosystem(dir.path()), Some(Ecosystem::Cargo));

        let bare = tempfile::tempdir().expect("bare tempdir");
        assert_eq!(detect_ecosystem(bare.path()), None);
    }

    #[tokio::test]
    async fn missing_program_surfaces_as_execution_error() {
        // An explicit ecosystem skips detection (the root does not exist),
        // so the failure is the spawn itself, not planning.
        let root = PathBuf::from("/nonexistent-loom-verify-root");
        let tool = VerifyTool::new();
        let error = tool
            .call(json!({"scope": "check", "ecosystem": "cargo"}), &ctx(root))
            .await
            .expect_err("spawn must fail");
        assert!(matches!(error, ToolError::Execution(_)));
    }
}
