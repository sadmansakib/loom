//! Integration tests for declared verification runners (item 23):
//! the standalone `verify` tool and the fused verify inside `edit_file`
//! both execute through a runner declared in `.loom/verify.toml` and
//! keep the same evidence contract.
//!
//! The runner is a mock: this test binary re-executed with
//! `--exact verify_mock_runner` (the pattern proven by the MCP suite).
//! Re-exec mode is gated on `LOOM_VERIFY_MOCK` so a normal suite run
//! of the dispatcher is a no-op; the declaring test sets the variable
//! before invoking the tool, and the spawned runner inherits it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use loom_kernel::tool::{Tool, ToolContext, ToolError};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use loom_tools::VerifyTool;

/// The re-exec dispatcher. Suite mode is a plain pass; mock mode plays
/// a `cargo check` that failed with one E0308 diagnostic, then exits
/// 101 like cargo would. Mode is the working directory: the runner
/// executes with cwd = the declaring workspace, which by construction
/// carries `.loom/verify.toml`, while a suite run's cwd never does
/// (edition 2024 forbids the env-var alternative — `set_var` is
/// `unsafe` and the workspace bans `unsafe`).
#[test]
fn verify_mock_runner() {
    let mock_mode = std::env::current_dir()
        .map(|dir| dir.join(".loom/verify.toml").is_file())
        .unwrap_or(false);
    if !mock_mode {
        return;
    }
    use std::io::Write as _;
    let mut out = std::io::stdout();
    // Libtest prints its progress line without a newline; give the
    // harness's line-oriented reader a clean boundary.
    writeln!(out).unwrap();
    writeln!(
        out,
        "{{\"reason\":\"compiler-message\",\"message\":{{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{{\"code\":\"E0308\"}},\"spans\":[{{\"file_name\":\"src/lib.rs\",\"line_start\":3,\"column_start\":9,\"is_primary\":true}}]}}}}"
    )
    .unwrap();
    out.flush().unwrap();
    std::process::exit(101);
}

/// A cargo workspace with a runner manifest pointing at the mock.
fn workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-verify-runner-test-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
    let exe = std::env::current_exe().unwrap().display().to_string();
    std::fs::write(
        dir.join(".loom/verify.toml"),
        format!(
            "[runner]\ncommand = '{exe} --exact verify_mock_runner --test-threads=1 --nocapture'\n"
        ),
    )
    .unwrap();
    dir
}

fn context(root: &std::path::Path) -> ToolContext {
    ToolContext::new(
        root.to_owned(),
        CancellationToken::new(),
        Duration::from_secs(60),
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The declared runner executes instead of local cargo, the evidence
/// contract is unchanged (structured E0308 diagnostic, linked to the
/// recent change), and the trace records both the unconfined-substrate
/// notice and the full expanded argv as the command effect.
#[test]
fn standalone_verify_runs_through_the_declared_runner() {
    let root = workspace();
    let tool = VerifyTool::new();
    let rt = runtime();
    let output = rt
        .block_on(tool.call(json!({"scope": "check"}), &context(&root)))
        .expect("runner-mediated verify returns");

    // Same evidence contract as local cargo: structured diagnostic,
    // not a raw log.
    assert!(output.content.contains("exit status: 101"));
    assert!(
        output
            .content
            .contains("[error] mismatched types (E0308) at src/lib.rs:3:9"),
        "{}",
        output.content
    );
    // The header shows what actually ran: the mock, not `cargo check`.
    assert!(
        output.content.contains("verify_mock_runner"),
        "{}",
        output.content
    );

    // The trace records the runner as the executed command.
    let commands = &output.effects.commands_run;
    assert_eq!(commands.len(), 1);
    assert!(
        commands[0].program.contains("verify_runner"),
        "unexpected program {:?}",
        commands[0].program
    );
    assert!(commands[0].args.contains(&"--exact".to_owned()));
    assert_eq!(commands[0].exit_code, Some(101));

    // And it says, in the trace, that confinement did not apply.
    assert!(
        output
            .notices
            .iter()
            .any(|notice| notice.category == "verify.runner"),
        "runner notice missing: {:?}",
        output.notices
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A broken manifest fails closed: `verify` refuses instead of
/// quietly running unisolated local commands.
#[test]
fn broken_manifest_fails_closed() {
    let root = workspace();
    std::fs::write(root.join(".loom/verify.toml"), "garbage\n").unwrap();
    let tool = VerifyTool::new();
    let rt = runtime();
    let error = rt
        .block_on(tool.call(json!({"scope": "check"}), &context(&root)))
        .expect_err("broken manifest must refuse");
    let message = match &error {
        ToolError::Execution(message) | ToolError::InvalidInput(message) => message.clone(),
        _ => String::from("unexpected variant"),
    };
    assert!(message.contains(".loom/verify.toml"), "{message}");

    std::fs::remove_dir_all(&root).ok();
}

/// The fused verify inside `edit_file` honors the same declaration:
/// the write applies, verification runs through the mock, and the
/// structured evidence rides the same tool output.
#[test]
fn fused_verify_runs_through_the_declared_runner() {
    let root = workspace();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let tool = loom_tools::EditFileTool::default();
    let rt = runtime();
    let output = rt
        .block_on(tool.call(
            json!({
                "path": "src/lib.rs",
                "old_string": "pub fn a() {}",
                "new_string": "pub fn b() {}",
                "verify": "check",
            }),
            &context(&root),
        ))
        .expect("fused edit with runner verify returns");

    assert!(
        output.content.contains("--- verify ---"),
        "{}",
        output.content
    );
    assert!(
        output
            .content
            .contains("[error] mismatched types (E0308) at src/lib.rs:3:9"),
        "{}",
        output.content
    );
    // Two effects: the file write and the runner command.
    assert!(!output.effects.files_written.is_empty());
    assert_eq!(output.effects.commands_run.len(), 1);
    assert!(
        output
            .notices
            .iter()
            .any(|notice| notice.category == "verify.runner"),
        "runner notice missing: {:?}",
        output.notices
    );

    std::fs::remove_dir_all(&root).ok();
}
