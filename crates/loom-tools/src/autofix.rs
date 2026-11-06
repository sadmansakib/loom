//! Machine-applicable compiler autofix after a failing verify
//! (roadmap item 33, docs/autofix-design.md).
//!
//! rustc ships exact fixes for a large class of mechanical errors —
//! `suggested_replacement` spans flagged `MachineApplicable` — that
//! `cargo_diagnostics` otherwise renders down to file:line text. This
//! module applies those spans after a **failing** Cargo verify, re-runs
//! the same fixed-table command, and keeps the edits only under a
//! never-worse gate: the diagnostic count must not increase. Anything
//! else — overlap, out-of-workspace spans, stale byte ranges, a re-run
//! that regresses — is skipped or reverted. Never a guess.
//!
//! Opt-in only (`EngineConfig::autofix`): the frozen eval protocol and
//! the bare-loop arm never set the flag, so nothing changes for them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::{
    AutofixEffect, BeforeImage, CommandEffect, ToolContext, ToolEffects, ToolError, ToolNotice,
    ToolOutput,
};

use crate::budget::ToolBudgets;
use crate::confinement::{resolve_within, workspace_relative};
use crate::evidence::cargo_diagnostics;
use crate::exec::{ExecLimits, run_bounded};
use crate::pre_image::capture_before_image;
use crate::sandbox::Sandbox;
use crate::verify::render;

/// One machine-applicable span: replace `[byte_start, byte_end)` in
/// `file` with `replacement`. Offsets are into the file as the compiler
/// just read it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FixSpan {
    file: String,
    byte_start: usize,
    byte_end: usize,
    replacement: String,
    /// The diagnostic's lint/error code, for the bounded report.
    code: Option<String>,
    /// 1-based line of the span start, for the bounded report.
    line_start: u32,
}

/// Parses `cargo --message-format=json` output into machine-applicable
/// spans. Both the message's own spans and its `children[]` spans count
/// (children carry most help suggestions); applicability values other
/// than `MachineApplicable` are deliberately ignored — those are guesses.
fn machine_applicable(output: &str) -> Vec<FixSpan> {
    let mut spans = Vec::new();
    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|reason| reason.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let code = message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(|code| code.as_str())
            .map(str::to_owned);
        let sources = std::iter::once(message).chain(
            message
                .get("children")
                .and_then(|children| children.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        for source in sources {
            let Some(span_list) = source.get("spans").and_then(|spans| spans.as_array()) else {
                continue;
            };
            for span in span_list {
                let replacement = span
                    .get("suggested_replacement")
                    .and_then(|replacement| replacement.as_str());
                let applicable = span
                    .get("suggestion_applicability")
                    .and_then(|applicability| applicability.as_str())
                    == Some("MachineApplicable");
                let (Some(replacement), true) = (replacement, applicable) else {
                    continue;
                };
                let (Some(file), Some(start), Some(end), Some(line)) = (
                    span.get("file_name").and_then(|file| file.as_str()),
                    span.get("byte_start").and_then(|byte| byte.as_u64()),
                    span.get("byte_end").and_then(|byte| byte.as_u64()),
                    span.get("line_start").and_then(|line| line.as_u64()),
                ) else {
                    continue;
                };
                spans.push(FixSpan {
                    file: file.to_owned(),
                    byte_start: start as usize,
                    byte_end: end as usize,
                    replacement: replacement.to_owned(),
                    code: code.clone(),
                    line_start: line as u32,
                });
            }
        }
    }
    spans
}

/// The plan for one file: accepted, non-overlapping edits in byte order.
struct FilePlan {
    /// Absolute path, already confinement-checked.
    abs: PathBuf,
    /// Workspace-relative display path (effect records).
    rel: String,
    edits: Vec<FixSpan>,
}

/// Why the planning pass refused spans — surfaced in the bounded report
/// so a refused fix is visible, not silent.
#[derive(Debug, Default)]
struct Refusals {
    outside_workspace: usize,
    overlapping: usize,
}

/// Resolves spans against the workspace, drops overlaps, and groups the
/// survivors per file. Registry dependencies and any other file outside
/// the root are refused (`resolve_within`); within a file, a span
/// overlapping an already-accepted one skips — conservative, per the
/// design's "never guess" contract.
fn plan(spans: Vec<FixSpan>, root: &Path) -> (Vec<FilePlan>, Refusals) {
    let mut per_file: BTreeMap<String, (PathBuf, Vec<FixSpan>)> = BTreeMap::new();
    let mut refusals = Refusals::default();
    for span in spans {
        match resolve_within(root, &span.file) {
            Ok(abs) => {
                let rel = workspace_relative(root, &abs);
                per_file
                    .entry(rel)
                    .or_insert_with(|| (abs, Vec::new()))
                    .1
                    .push(span);
            }
            Err(_) => refusals.outside_workspace += 1,
        }
    }
    let mut plans = Vec::new();
    for (rel, (abs, mut edits)) in per_file {
        edits.sort_by_key(|span| (span.byte_start, span.byte_end));
        let mut accepted = Vec::new();
        let mut last_end = 0;
        for edit in edits {
            if edit.byte_start < last_end {
                refusals.overlapping += 1;
                continue;
            }
            last_end = edit.byte_end;
            accepted.push(edit);
        }
        if !accepted.is_empty() {
            plans.push(FilePlan {
                abs,
                rel,
                edits: accepted,
            });
        }
    }
    (plans, refusals)
}

/// A staged file: edits applied, pre-edit bytes and pre-image retained
/// for the revert path.
struct StagedFile {
    abs: PathBuf,
    rel: String,
    edits: Vec<FixSpan>,
    before: Vec<u8>,
    image: Option<BeforeImage>,
}

/// What one autofix attempt did, for the caller to merge into its
/// original `ToolOutput`.
pub(crate) struct Outcome {
    /// Bounded lines for the model, after `--- autofix ---`.
    summary: String,
    /// Additive effects: re-run command, writes (when kept), pre-images,
    /// repair note, and the `AutofixEffect` record.
    effects: ToolEffects,
    /// The re-run rendered exactly like the original verify, so the model
    /// sees the remaining (or resolved) evidence.
    rerun: ToolOutput,
    /// Trace-side observation category (`autofix.applied`/`autofix.reverted`).
    notice_category: &'static str,
}

impl Outcome {
    /// Appends the autofix section, re-verify body, effects, and notice
    /// onto the original rendered output.
    pub(crate) fn merge(self, mut base: ToolOutput) -> ToolOutput {
        base.content
            .push_str("\n--- autofix ---\n--- machine-applicable fixes ---\n");
        base.content.push_str(&self.summary);
        base.content.push_str("\n--- re-verify ---\n");
        base.content.push_str(&self.rerun.content);
        base.effects = base.effects.merge(self.effects);
        base.notices.push(ToolNotice::new(
            self.notice_category,
            "machine-applicable fixes attempted after failing verify",
        ));
        if self.rerun.truncated {
            base = base.with_truncated();
        }
        base
    }
}

/// The never-worse gate input: the run's error+warning diagnostic count,
/// parsed the same way `parse_evidence` parses Cargo output (stdout,
/// stderr fallback).
fn diagnostic_count(output: &std::process::Output) -> usize {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = cargo_diagnostics(&stdout);
    if parsed.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cargo_diagnostics(&stderr).len()
    } else {
        parsed.len()
    }
}

/// Cap on per-fix lines in the bounded report; beyond that the count
/// alone is reported.
const MAX_REPORTED_FIXES: usize = 8;

/// Attempts one autofix round after a failing Cargo verify: plan,
/// snapshot, apply, re-run, gate. Returns `None` when there is nothing
/// to do (the run passed, no machine-applicable spans, or everything was
/// refused) — nothing happened, so nothing is reported.
///
/// The caller supplies the same execution pieces the original run used,
/// so the re-run observes identical limits and sandbox.
// The parameters mirror the verify call site exactly (program, args,
// first output, limits, sandbox, budgets); bundling them into a struct
// would obscure the one-to-one correspondence with `exec::run_bounded`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attempt(
    ctx: &ToolContext,
    program: &str,
    args: &[String],
    first: &std::process::Output,
    limits: &ExecLimits,
    sandbox: &Sandbox,
    recent_changes: &[String],
    budgets: &ToolBudgets,
) -> Result<Option<Outcome>, ToolError> {
    if first.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&first.stdout);
    let mut spans = machine_applicable(&stdout);
    if spans.is_empty() {
        let stderr = String::from_utf8_lossy(&first.stderr);
        spans = machine_applicable(&stderr);
    }
    if spans.is_empty() {
        return Ok(None);
    }
    let (plans, refusals) = plan(spans, &ctx.workspace_root);
    if plans.is_empty() {
        return Ok(None);
    }

    // Snapshot + apply in one blocking pass: pre-images are captured
    // from the exact bytes about to be edited, and edits are spliced
    // right-to-left so earlier offsets stay valid.
    let artifacts = ctx.artifacts.clone();
    let staged = tokio::task::spawn_blocking(move || stage_and_apply(plans, artifacts.as_deref()))
        .await
        .map_err(|err| ToolError::Execution(format!("autofix task failed: {err}")))?;
    let edits_total: usize = staged.iter().map(|file| file.edits.len()).sum();
    if edits_total == 0 {
        // Every span was stale (range/UTF-8 checks) — nothing applied.
        return Ok(None);
    }

    let before_count = diagnostic_count(first);
    // The re-run is the same fixed-table command through the same exec
    // path. If it fails to execute at all, the gate cannot say "not
    // worse", so the edits revert.
    let rerun_result = run_bounded(program, args, ctx, limits, sandbox).await;
    let (kept, rerun) = match &rerun_result {
        Ok(outcome) => {
            // Never-worse gate. Exit 0 is a strict win. A failing re-run
            // must still be measurable (diagnostics present, not more)
            // — a failure with zero parseable diagnostics cannot show
            // "not worse", so it reverts rather than guessing.
            let after = diagnostic_count(&outcome.output);
            let kept = outcome.output.status.success() || (after > 0 && after <= before_count);
            let rendered = render(
                crate::verify::Ecosystem::Cargo,
                program,
                args,
                &outcome.output,
                recent_changes,
                budgets,
            );
            (kept, rendered)
        }
        Err(err) => (
            false,
            ToolOutput::new(format!("re-verify failed to execute: {err}")),
        ),
    };

    if !kept {
        let restore = staged
            .iter()
            .map(|file| (file.abs.clone(), file.before.clone()))
            .collect();
        tokio::task::spawn_blocking(move || restore_files(restore))
            .await
            .map_err(|err| ToolError::Execution(format!("autofix restore failed: {err}")))?;
    }

    let after_count = match &rerun_result {
        Ok(outcome) => diagnostic_count(&outcome.output),
        Err(_) => before_count, // unknown, and reverted anyway
    };

    // The bounded report: what was applied, what was refused, the gate's
    // verdict. One line per fix up to the cap, then the counts.
    let mut summary = String::new();
    let mut reported = 0;
    for file in &staged {
        for edit in &file.edits {
            if reported >= MAX_REPORTED_FIXES {
                break;
            }
            let code = edit.code.as_deref().unwrap_or("diagnostic");
            summary.push_str(&format!(
                "fixed {code} at {}:{}\n",
                file.rel, edit.line_start
            ));
            reported += 1;
        }
    }
    if edits_total > MAX_REPORTED_FIXES {
        summary.push_str(&format!("…and {} more\n", edits_total - MAX_REPORTED_FIXES));
    }
    if refusals.outside_workspace > 0 || refusals.overlapping > 0 {
        summary.push_str(&format!(
            "skipped: {} outside workspace, {} overlapping\n",
            refusals.outside_workspace, refusals.overlapping
        ));
    }
    summary.push_str(&format!(
        "diagnostics: {before_count} -> {after_count} ({})",
        if kept {
            "kept"
        } else {
            "reverted: never-worse gate"
        }
    ));

    let exit_code = rerun_result
        .as_ref()
        .ok()
        .and_then(|outcome| outcome.output.status.code());
    let mut effects =
        ToolEffects::none().with_command(CommandEffect::new(program, args.to_vec(), exit_code));
    let record = AutofixEffect::new(
        edits_total as u32,
        staged.len() as u32,
        before_count as u32,
        after_count as u32,
        kept,
    );
    if kept {
        for file in &staged {
            effects = effects.with_file_written(file.rel.clone());
            if let Some(image) = &file.image {
                effects = effects.with_before_image(image.clone());
            }
        }
        effects = effects.with_repair(format!(
            "autofix: applied {edits_total} machine-applicable fix(es) in {} file(s), diagnostics {before_count} -> {after_count}",
            staged.len()
        ));
    } else {
        effects = effects.with_repair(format!(
            "autofix: attempted {edits_total} fix(es), reverted (diagnostics {before_count} -> {after_count})"
        ));
    }
    effects = effects.with_autofix(record);

    Ok(Some(Outcome {
        summary,
        effects,
        rerun,
        notice_category: if kept {
            "autofix.applied"
        } else {
            "autofix.reverted"
        },
    }))
}

/// Captures pre-images and splices the planned edits into each file.
/// Stale spans (past end-of-file, or splitting a UTF-8 sequence) are
/// dropped at this stage — the compiler read these bytes moments ago,
/// so a bad offset means the file changed underneath and the fix does
/// not apply.
fn stage_and_apply(plans: Vec<FilePlan>, store: Option<&dyn ArtifactStore>) -> Vec<StagedFile> {
    let mut staged = Vec::new();
    for plan in plans {
        let Ok(before) = std::fs::read(&plan.abs) else {
            continue;
        };
        let mut accepted = Vec::new();
        for edit in plan.edits {
            let slice = before.get(edit.byte_start..edit.byte_end);
            let valid = slice.is_some_and(|slice| std::str::from_utf8(slice).is_ok());
            if valid {
                accepted.push(edit);
            }
        }
        if accepted.is_empty() {
            continue;
        }
        let image = capture_before_image(store, &plan.rel, Some(&before));
        let mut content = before.clone();
        for edit in accepted.iter().rev() {
            content.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
        }
        if std::fs::write(&plan.abs, &content).is_ok() {
            staged.push(StagedFile {
                abs: plan.abs,
                rel: plan.rel,
                edits: accepted,
                before,
                image,
            });
        }
    }
    staged
}

/// Writes each staged file's pre-image bytes back (the revert path).
fn restore_files(files: Vec<(PathBuf, Vec<u8>)>) {
    for (path, before) in files {
        let _ = std::fs::write(path, before);
    }
}

#[cfg(test)]
mod tests {
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
        assert!(machine_applicable(&lines).is_empty());
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
        assert!(outcome.effects.files_written.is_empty());
        assert!(outcome.effects.before_images.is_empty());
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
}
