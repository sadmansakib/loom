use super::render::status_from_content;
use super::tool::VerifyInput;
use super::*;
use crate::budget::ToolBudgets;
use crate::policy::CommandPolicy;
use loom_kernel::tool::{Tool, ToolContext, ToolError};
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
        &ToolBudgets::default(),
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
#[test]
fn status_from_content_reads_the_verdict() {
    assert_eq!(
        status_from_content("$ make test\nexit status: 0\nno diagnostics"),
        Some(true)
    );
    assert_eq!(
        status_from_content("$ make test\nexit status: 7\nfailed"),
        Some(false)
    );
    // A signal death is not a suite verdict.
    assert_eq!(
        status_from_content("$ make test\nexit status: terminated by signal\n"),
        None
    );
    assert_eq!(status_from_content("no status line at all"), None);
}

#[tokio::test]
async fn suite_verdict_runs_the_fixed_table_suite() {
    let tool = VerifyTool::new();
    let cancel = CancellationToken::new();
    // No ecosystem marker: no verdict, and nothing executes.
    let bare = tempfile::tempdir().expect("tempdir");
    assert_eq!(tool.suite_verdict(bare.path(), &cancel).await, None);
    // Make is the one table entry whose suite a test can shape.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Makefile"), "test:\n\t@exit 0\n").expect("makefile");
    assert_eq!(tool.suite_verdict(dir.path(), &cancel).await, Some(true));
    std::fs::write(dir.path().join("Makefile"), "test:\n\t@exit 7\n").expect("makefile");
    assert_eq!(tool.suite_verdict(dir.path(), &cancel).await, Some(false));
}

#[tokio::test]
async fn an_operator_deny_blocks_verify() {
    // Item 2.6: an explicit deny verdict on the fixed-table
    // program refuses the verification (no verdict renders);
    // without the deny the same fixture yields one.
    let cancel = CancellationToken::new();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Makefile"), "test:\n\t@exit 0\n").expect("makefile");
    assert_eq!(
        VerifyTool::new().suite_verdict(dir.path(), &cancel).await,
        Some(true)
    );
    let denied = VerifyTool::with_policy(
        CommandPolicy::parse("[[deny]]\nprogram = \"make\"\n").expect("policy parses"),
    );
    assert_eq!(
        denied.suite_verdict(dir.path(), &cancel).await,
        None,
        "a denied program must not run the suite"
    );
}

#[tokio::test]
async fn ask_verdicts_reach_the_operator_with_the_full_command_line() {
    // Review F-13: verify consults the same operator approval as
    // run_command, and the request carries the driver's argv plus
    // escalation state — not just the program name.
    let cancel = CancellationToken::new();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Makefile"), "test:\n\t@exit 0\n").expect("makefile");
    let policy = CommandPolicy::parse("[policy]\nunknown = \"ask\"\n").expect("policy parses");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured = std::sync::Arc::clone(&seen);
    let tool =
        VerifyTool::with_policy(policy).with_ask_resolver(std::sync::Arc::new(move |request| {
            *captured.lock().unwrap() = Some(request);
            Box::pin(async move { false }) // declined this time
        }));

    // The suite verdict fails closed on a declined approval.
    assert_eq!(tool.suite_verdict(dir.path(), &cancel).await, None);
    let request = seen.lock().unwrap().take().expect("resolver consulted");
    assert_eq!(request.program, "make");
    assert!(
        request.args.iter().any(|arg| arg == "test"),
        "full argv, got {:?}",
        request.args
    );
    assert!(request.sandboxed, "no rule escalates make");
    assert!(!request.network);

    // A direct verify surfaces the refusal the model sees, naming
    // the approval rather than a missing frontend.
    let error = tool
        .call(
            json!({"scope": "test", "ecosystem": "make"}),
            &ctx(dir.path().to_path_buf()),
        )
        .await
        .expect_err("declined");
    assert!(
        matches!(&error, ToolError::InvalidInput(m) if m.contains("was not approved")),
        "{error}"
    );
    assert!(
        !error.to_string().contains("no interactive frontend"),
        "a resolver was attached: {error}"
    );
}
