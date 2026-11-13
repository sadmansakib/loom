use super::*;

fn span(file: &str, start: usize, end: usize, replacement: &str) -> FixSpan {
    FixSpan {
        file: file.to_owned(),
        byte_start: start,
        byte_end: end,
        replacement: replacement.to_owned(),
        code: Some("E0596".to_owned()),
        line_start: 1,
    }
}

#[test]
fn parse_reads_message_and_children_spans() {
    let record = serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "cannot borrow as mutable",
            "code": {"code": "E0596"},
            "spans": [],
            "children": [
                {
                    "message": "consider changing this to be mutable",
                    "spans": [{
                        "file_name": "src/lib.rs",
                        "byte_start": 8,
                        "byte_end": 12,
                        "line_start": 2,
                        "suggested_replacement": "mut v",
                        "suggestion_applicability": "MachineApplicable"
                    }]
                }
            ]
        }
    });
    let spans = machine_applicable(&record.to_string());
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].replacement, "mut v");
    assert_eq!(spans[0].code.as_deref(), Some("E0596"));
}

#[test]
fn parse_ignores_non_machine_applicable_and_noise() {
    let lines = format!(
        "{}\n{}\n{}",
        serde_json::json!({"reason": "compiler-artifact", "message": "noise"}),
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "warning",
                "message": "unused variable",
                "spans": [{
                    "file_name": "src/lib.rs",
                    "byte_start": 0, "byte_end": 1, "line_start": 1,
                    "suggested_replacement": "_x",
                    "suggestion_applicability": "MaybeIncorrect"
                }],
                "children": []
            }
        }),
        "not json at all"
    );
    assert_eq!(machine_applicable(&lines).len(), 0);
}

#[test]
fn plan_skips_overlapping_spans_in_one_file() {
    let spans = vec![
        span("src/lib.rs", 10, 20, "a"),
        span("src/lib.rs", 15, 25, "b"), // overlaps -> skipped
        span("src/lib.rs", 30, 35, "c"),
    ];
    let (plans, refusals) = plan(spans, Path::new("/ws"));
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].edits.len(), 2);
    assert_eq!(refusals.overlapping, 1);
}

#[test]
fn plan_refuses_paths_outside_the_workspace() {
    let spans = vec![
        span("../registry/lib.rs", 0, 3, "x"),
        span("/etc/passwd", 0, 3, "y"),
        span("src/lib.rs", 0, 3, "z"),
    ];
    let (plans, refusals) = plan(spans, Path::new("/ws"));
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].rel, "src/lib.rs");
    assert_eq!(refusals.outside_workspace, 2);
}

#[test]
fn stage_and_apply_splices_edits_and_reverts_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, "let v = 1;\nlet w = 2;\n").expect("write");
    let plans = vec![FilePlan {
        abs: path.clone(),
        rel: "lib.rs".to_owned(),
        edits: vec![
            span("lib.rs", 4, 5, "mut v"),
            span("lib.rs", 15, 16, "mut w"),
        ],
    }];
    let staged = stage_and_apply(plans, None);
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].image, None); // no store attached
    let after = std::fs::read_to_string(&path).expect("read");
    assert_eq!(after, "let mut v = 1;\nlet mut w = 2;\n");
    restore_files(vec![(path.clone(), staged[0].before.clone())]);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "let v = 1;\nlet w = 2;\n"
    );
}

#[test]
fn stage_and_apply_drops_stale_and_boundary_breaking_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, "héllo\n").expect("write"); // é is two bytes
    let plans = vec![FilePlan {
        abs: path.clone(),
        rel: "lib.rs".to_owned(),
        edits: vec![
            span("lib.rs", 1, 2, "x"),    // splits é (bytes 1..3)
            span("lib.rs", 99, 120, "y"), // past end
            span("lib.rs", 0, 1, "H"),    // valid
        ],
    }];
    let staged = stage_and_apply(plans, None);
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].edits.len(), 1);
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "Héllo\n");
}

// ---- behavioral tests: attempt() with a scripted re-run command ----
//
// `attempt` takes the program it re-runs as a parameter, so these
// tests drive the real exec/sandbox/render path with `sh -c …`
// payloads instead of a toolchain: fully offline and deterministic.

/// A diagnostic record `cargo_diagnostics` can count (and render).
fn diag(level: &str, code: &str) -> serde_json::Value {
    serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": level,
            "message": "fixture diagnostic",
            "code": {"code": code},
            "spans": [{
                "is_primary": true,
                "file_name": "src/lib.rs",
                "line_start": 1,
                "column_start": 5
            }],
            "children": []
        }
    })
}

/// A failing run whose E0384 carries a machine-applicable child span
/// replacing byte 4..5 of `src/lib.rs` ("x" → "mut x"), plus one
/// plain warning: two diagnostics, one fix.
fn failing_run_with_mut_fix() -> std::process::Output {
    let error = serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "cannot assign twice to immutable variable",
            "code": {"code": "E0384"},
            "spans": [{
                "is_primary": true,
                "file_name": "src/lib.rs",
                "line_start": 1,
                "column_start": 5
            }],
            "children": [{
                "message": "make this binding mutable",
                "spans": [{
                    "file_name": "src/lib.rs",
                    "byte_start": 4,
                    "byte_end": 5,
                    "line_start": 1,
                    "suggested_replacement": "mut x",
                    "suggestion_applicability": "MachineApplicable"
                }]
            }]
        }
    });
    let stdout = format!("{}\n{}\n", error, diag("warning", "unused_variables"));
    failing_output(&stdout)
}

fn failing_output(stdout: &str) -> std::process::Output {
    std::process::Output {
        status: exit_code(101),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

#[cfg(unix)]
fn exit_code(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

fn attempt_ctx(root: &Path) -> ToolContext {
    ToolContext::new(
        root.to_path_buf(),
        tokio_util::sync::CancellationToken::new(),
        std::time::Duration::from_secs(120),
    )
}

fn write_workspace(dir: &Path, lib: &str) {
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"probe\"\n").expect("toml");
    std::fs::write(dir.join("src/lib.rs"), lib).expect("lib");
}

/// `sh -c` command that prints `payload` then exits 101, so the
/// re-run is a *failing* cargo run for the gate to measure.
fn failing_rerun(payload: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!("printf '%s' '{payload}'; exit 101"),
    ]
}

#[tokio::test]
async fn attempt_keeps_fixes_when_rerun_is_not_worse() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_workspace(dir.path(), "let x = 1;\n");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    let first = failing_run_with_mut_fix();

    // Re-run still fails, but with one diagnostic instead of two.
    let args = failing_rerun(&diag("warning", "unused_variables").to_string());
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs")
    .expect("an attempt was made");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib"),
        "let mut x = 1;\n"
    );
    let record = outcome.effects.autofix.as_ref().expect("record");
    assert!(record.kept);
    assert_eq!(record.edits, 1);
    assert_eq!(record.files, 1);
    assert_eq!(record.diagnostics_before, 2);
    assert_eq!(record.diagnostics_after, 1);
    assert_eq!(outcome.notice_category, "autofix.applied");
    assert!(outcome.summary.contains("diagnostics: 2 -> 1 (kept)"));
    // The section the model sees after the original verify output.
    let merged = outcome.merge(ToolOutput::new("$ cargo check\nexit status: 101"));
    assert!(merged.content.contains("--- autofix ---"));
    assert!(merged.content.contains("--- re-verify ---"));
    assert!(
        merged
            .notices
            .iter()
            .any(|notice| notice.category == "autofix.applied")
    );
    assert!(
        merged
            .effects
            .files_written
            .iter()
            .any(|path| path == "src/lib.rs")
    );
}

#[tokio::test]
async fn attempt_reverts_when_rerun_is_worse() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_workspace(dir.path(), "let x = 1;\n");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    let first = failing_run_with_mut_fix();

    // The "fix" makes things worse: three diagnostics after.
    let payload = format!(
        "{}\n{}\n{}",
        diag("error", "E0384"),
        diag("error", "E0308"),
        diag("warning", "unused_variables")
    );
    let args = failing_rerun(&payload);
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs")
    .expect("an attempt was made");

    // Never-worse gate: byte-identical revert, no recorded writes.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib"),
        "let x = 1;\n"
    );
    let record = outcome.effects.autofix.as_ref().expect("record");
    assert!(!record.kept);
    assert_eq!(record.diagnostics_before, 2);
    assert_eq!(record.diagnostics_after, 3);
    assert_eq!(outcome.notice_category, "autofix.reverted");
    assert!(outcome.summary.contains("reverted"));
    assert_eq!(outcome.effects.files_written.len(), 0);
    assert_eq!(outcome.effects.before_images.len(), 0);
    assert!(
        outcome
            .effects
            .repairs
            .iter()
            .any(|repair| repair.contains("reverted"))
    );
}

#[tokio::test]
async fn attempt_reverts_when_rerun_is_unmeasurable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_workspace(dir.path(), "let x = 1;\n");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    let first = failing_run_with_mut_fix();

    // A re-run that fails with no parseable diagnostics at all cannot
    // show "not worse" — the gate must revert rather than guess.
    let args = vec!["-c".to_owned(), "exit 101".to_owned()];
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs")
    .expect("an attempt was made");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib"),
        "let x = 1;\n"
    );
    let record = outcome.effects.autofix.as_ref().expect("record");
    assert!(!record.kept);
    assert_eq!(record.diagnostics_after, 0);
}

#[tokio::test]
async fn attempt_does_nothing_without_machine_applicable_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_workspace(dir.path(), "let x = 1;\n");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );

    // Failing run, but only MaybeIncorrect suggestions.
    let maybe = serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "mismatched types",
            "code": {"code": "E0308"},
            "spans": [{
                "is_primary": true,
                "file_name": "src/lib.rs",
                "line_start": 1,
                "column_start": 5
            }],
            "children": [{
                "message": "consider borrowing",
                "spans": [{
                    "file_name": "src/lib.rs",
                    "byte_start": 4,
                    "byte_end": 5,
                    "line_start": 1,
                    "suggested_replacement": "&x",
                    "suggestion_applicability": "MaybeIncorrect"
                }]
            }]
        }
    });
    let first = failing_output(&maybe.to_string());
    let args = failing_rerun(&diag("error", "E0308").to_string());
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs");
    assert!(outcome.is_none(), "no machine-applicable spans, no attempt");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("lib"),
        "let x = 1;\n"
    );
}

#[tokio::test]
async fn attempt_refuses_spans_outside_the_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_workspace(dir.path(), "let x = 1;\n");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );

    // Machine-applicable, but aimed at the registry cache and /etc:
    // confinement refuses both, nothing applies, nothing reports.
    let outside = serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "cannot find crate",
            "code": {"code": "E0432"},
            "spans": [],
            "children": [{
                "message": "import here",
                "spans": [{
                    "file_name": "../registry/src/lib.rs",
                    "byte_start": 0,
                    "byte_end": 3,
                    "line_start": 1,
                    "suggested_replacement": "use x;",
                    "suggestion_applicability": "MachineApplicable"
                }]
            }]
        }
    });
    let first = failing_output(&outside.to_string());
    let args = failing_rerun(&diag("error", "E0432").to_string());
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs");
    assert!(outcome.is_none(), "every span refused, no attempt");
}

#[tokio::test]
async fn attempt_skips_a_passing_run_entirely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = attempt_ctx(dir.path());
    let sandbox = crate::sandbox::Sandbox::resolve(
        &ctx.workspace_root,
        crate::sandbox::SandboxGrants::from_rule(None),
    );
    // A green run never autofixes, spans or no spans.
    let mut first = failing_run_with_mut_fix();
    first.status = exit_code(0);
    let args = failing_rerun(&diag("warning", "unused_variables").to_string());
    let outcome = attempt(
        &ctx,
        "sh",
        &args,
        &first,
        &ExecLimits::default(),
        &sandbox,
        &[],
        &ToolBudgets::default(),
    )
    .await
    .expect("attempt runs");
    assert!(outcome.is_none());
}
