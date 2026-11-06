//! End-to-end autofix tests against a real `cargo` toolchain (roadmap
//! item 33, docs/autofix-design.md). The unit tests in
//! `src/autofix.rs` drive `attempt` with scripted re-run commands;
//! these prove the wiring — `EngineConfig::autofix` → tool flag →
//! failing verify → applied `MachineApplicable` span → re-run → gate —
//! with rustc's actual suggestions. Skipped when no toolchain exists.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use loom_kernel::tool::{Tool, ToolContext};
use loom_tools::{CommandPolicy, EditFileTool, VerifyTool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn cargo_present() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A minimal crate whose `f` fails `cargo check` with exactly one
/// machine-applicable error (E0384: assign to immutable).
fn crate_with_assign_immutable_error(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nedition = \"2021\"\n",
    )
    .expect("Cargo.toml");
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() { let x = 1; x = 2; }\n").expect("lib.rs");
}

fn ctx(dir: &Path) -> ToolContext {
    ToolContext::new(
        dir.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(600),
    )
}

#[tokio::test]
async fn verify_autofix_applies_a_real_rustc_mut_fix() {
    if !cargo_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    crate_with_assign_immutable_error(dir.path());
    let tool = VerifyTool::with_policy(CommandPolicy::cargo_only()).with_autofix(true);

    let output = tool
        .call(json!({"scope": "check"}), &ctx(dir.path()))
        .await
        .expect("verify runs");

    // The re-run rendered after the autofix section: the error is gone
    // (the unused-assignment warning may remain — kept ≠ clean).
    assert!(
        output.content.contains("--- autofix ---"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("diagnostics: 3 -> 2 (kept)"),
        "{}",
        output.content
    );
    let lib = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib");
    assert!(lib.contains("let mut x"), "{lib}");
    let record = output.effects.autofix.as_ref().expect("record");
    assert!(record.kept);
    assert_eq!(record.edits, 1);
    assert_eq!(record.files, 1);
    // The effect trail: the failing run plus the re-run.
    assert_eq!(output.effects.commands_run.len(), 2);
    assert!(
        output
            .effects
            .files_written
            .iter()
            .any(|path| path == "src/lib.rs")
    );
    assert!(
        output
            .notices
            .iter()
            .any(|notice| notice.category == "autofix.applied")
    );
}

#[tokio::test]
async fn verify_autofix_is_off_by_default() {
    if !cargo_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    crate_with_assign_immutable_error(dir.path());

    let output = VerifyTool::new()
        .call(json!({"scope": "check"}), &ctx(dir.path()))
        .await
        .expect("verify runs");

    assert!(
        !output.content.contains("--- autofix ---"),
        "{}",
        output.content
    );
    assert!(output.effects.autofix.is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib"),
        "pub fn f() { let x = 1; x = 2; }\n"
    );
}

#[tokio::test]
async fn edit_file_fused_verify_carries_the_autofix_round() {
    if !cargo_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nedition = \"2021\"\n",
    )
    .expect("Cargo.toml");
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn f() {}\n").expect("lib.rs");

    // The edit introduces the E0384; the fused verify fails; the
    // machine-applicable round fixes it within the same tool call.
    let tool = EditFileTool::default().with_autofix(true);
    let output = tool
        .call(
            json!({
                "path": "src/lib.rs",
                "old_string": "pub fn f() {}",
                "new_string": "pub fn f() { let x = 1; x = 2; }",
                "verify": "check"
            }),
            &ctx(dir.path()),
        )
        .await
        .expect("edit runs");

    assert!(
        output.content.contains("--- verify ---"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("--- autofix ---"),
        "{}",
        output.content
    );
    let lib = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib");
    assert!(lib.contains("let mut x"), "{lib}");
    assert!(output.effects.autofix.as_ref().expect("record").kept);
}
