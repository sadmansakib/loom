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
    assert_eq!(output.effects.files_written.len(), 0);
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
async fn args_as_json_array_string_with_trailing_pipeline_runs() {
    // xarray-4687 storm shape: the model serializes args as a JSON
    // array STRING with a shell pipeline appended outside the
    // brackets. Salvage must run the intended pipeline, not tokenize
    // the brackets into argv (which cost 100 turns before this fix).
    let (_guard, root) = temp_root();
    let tool = RunCommandTool::with_allowed(["sh"]);
    let output = run(
        &tool,
        &root,
        json!({"program": "sh", "args": "[\"-c\", \"printf 'first\\nsecond\\n'\"] | tail -1"}),
    )
    .await
    .expect("salvaged");
    assert!(
        output.content.contains("second"),
        "pipeline must run as the model intended: {}",
        output.content
    );
    assert!(
        !output.content.contains("cannot open"),
        "brackets must not leak into argv: {}",
        output.content
    );
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
    assert!(rendered.content.len() <= budget::COMMAND_HEAD_BYTES + budget::COMMAND_TAIL_BYTES + 64);
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
    let policy =
        CommandPolicy::parse("[[command]]\nprogram = \"echo\"\nsandbox = false\n").expect("policy");
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

#[tokio::test]
async fn the_suffix_patterns_scrub_dynamic_provider_keys() {
    // `loom provider add <name>` mints <NAME>_API_KEY names the
    // fixed floor never knew; the suffix pattern must catch them.
    scrub_probe(
        "[[command]]\nprogram = \"sh\"\n",
        &[
            ("MY_GATEWAY_API_KEY", "loom-test-secret"),
            ("TEAM_SERVICE_TOKEN", "loom-test-secret"),
            ("BUILD_DB_PASSWORD", "loom-test-secret"),
        ],
        "for v in MY_GATEWAY_API_KEY TEAM_SERVICE_TOKEN BUILD_DB_PASSWORD; do \
             if printenv \"$v\" >/dev/null; then echo leaked:$v; fi; done; echo scrubbed",
    )
    .await;
}

#[tokio::test]
async fn an_env_allow_entry_exempts_one_credential() {
    // The explicit opt-in: `gh` may receive GH_TOKEN, but the
    // exemption is per-name — the unrelated pattern match stays
    // scrubbed.
    scrub_probe(
        "[[command]]\nprogram = \"sh\"\nenv_allow = [\"GH_TOKEN\"]\n",
        &[
            ("GH_TOKEN", "ok-to-see"),
            ("OTHER_TOKEN", "loom-test-secret"),
        ],
        "if ! printenv GH_TOKEN >/dev/null; then echo leaked:no-gh; fi; \
             if printenv OTHER_TOKEN >/dev/null; then echo leaked:other; fi; echo scrubbed",
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
    let policy = CommandPolicy::parse("[[command]]\nprogram = \"seq\"\nmax_output_bytes = 1024\n")
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
        tool.spec().description.contains("Escalated,") && tool.spec().description.contains("`git`"),
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
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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
    // operator, which the model sees as a refusal. The resolver
    // sees the full request (review F-13), so pin that too.
    let tool = RunCommandTool::with_policy(policy).with_ask_resolver(Arc::new(|request| {
        let approved = request.program == "echo";
        let seen_full_argv = request.args == ["yes"];
        Box::pin(async move { approved && seen_full_argv })
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
