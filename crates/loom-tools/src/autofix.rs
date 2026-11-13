//! Machine-applicable compiler autofix after a failing verify
//! (roadmap item 33, docs/autofix-design.md).
//!
//! rustc ships exact fixes for a large class of mechanical errors —
//! `suggested_replacement` spans flagged `MachineApplicable` — that
//! `cargo_diagnostics` otherwise renders down to `<file:line>` text. This
//! module applies those spans after a **failing** Cargo verify, re-runs
//! the same fixed-table command, and keeps the edits only under a
//! never-worse gate: the diagnostic count must not increase. Anything
//! else — overlap, out-of-workspace spans, stale byte ranges, a re-run
//! that regresses — is skipped or reverted. Never a guess.
//!
//! Opt-in only (`EngineConfig::autofix`): the frozen eval protocol and
//! the bare-loop arm never set the flag, so nothing changes for them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::{
    AutofixEffect, BeforeImage, CommandEffect, ToolContext, ToolEffects, ToolError, ToolNotice,
    ToolOutput,
};

use crate::budget::ToolBudgets;
use crate::confinement::{resolve_within, workspace_relative};
use crate::evidence::cargo_diagnostics;
use crate::exec::{ExecLimits, ExecOutcome, run_bounded};
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
                .map_or_default(Vec::as_slice),
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
                    span.get("byte_start").and_then(serde_json::Value::as_u64),
                    span.get("byte_end").and_then(serde_json::Value::as_u64),
                    span.get("line_start").and_then(serde_json::Value::as_u64),
                ) else {
                    continue;
                };
                // rustc JSON offsets and line numbers are file-relative
                // and fit usize/u32 on every supported target.
                #[allow(clippy::cast_possible_truncation)]
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

/// The never-worse gate over a re-run outcome: exit 0 is a strict win;
/// a failing re-run must still be measurable (diagnostics present, not
/// more) — a failure with zero parseable diagnostics cannot show "not
/// worse", so it reverts rather than guessing.
fn gate_verdict(
    output: &std::process::Output,
    program: &str,
    args: &[String],
    recent_changes: &[String],
    budgets: &ToolBudgets,
    before_count: usize,
) -> (bool, ToolOutput) {
    let after = diagnostic_count(output);
    let kept = output.status.success() || (after > 0 && after <= before_count);
    let rendered = render(
        crate::verify::Ecosystem::Cargo,
        program,
        args,
        output,
        recent_changes,
        budgets,
    );
    (kept, rendered)
}

/// Builds the bounded autofix report: one line per fix up to the cap,
/// then the skip counts and the gate's verdict.
fn build_summary(
    staged: &[StagedFile],
    edits_total: usize,
    refusals: &Refusals,
    before_count: usize,
    after_count: usize,
    kept: bool,
) -> String {
    let mut summary = String::new();
    let mut reported = 0;
    for file in staged {
        for edit in &file.edits {
            if reported >= MAX_REPORTED_FIXES {
                break;
            }
            let code = edit.code.as_deref().unwrap_or("diagnostic");
            let _ = writeln!(summary, "fixed {code} at {}:{}", file.rel, edit.line_start);
            reported += 1;
        }
    }
    if edits_total > MAX_REPORTED_FIXES {
        let _ = writeln!(summary, "…and {} more", edits_total - MAX_REPORTED_FIXES);
    }
    if refusals.outside_workspace > 0 || refusals.overlapping > 0 {
        let _ = writeln!(
            summary,
            "skipped: {} outside workspace, {} overlapping",
            refusals.outside_workspace, refusals.overlapping
        );
    }
    let _ = write!(
        summary,
        "diagnostics: {before_count} -> {after_count} ({})",
        if kept {
            "kept"
        } else {
            "reverted: never-worse gate"
        }
    );
    summary
}

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
        Ok(outcome) => gate_verdict(
            &outcome.output,
            program,
            args,
            recent_changes,
            budgets,
            before_count,
        ),
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
    let summary = build_summary(
        &staged,
        edits_total,
        &refusals,
        before_count,
        after_count,
        kept,
    );

    let effects = build_effects(
        program,
        args,
        &staged,
        edits_total,
        &GateOutcome {
            before: before_count,
            after: after_count,
            kept,
        },
        &rerun_result,
    );

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

/// The never-worse gate outcome for one autofix round.
struct GateOutcome {
    before: usize,
    after: usize,
    kept: bool,
}

/// Assembles the tool effects for one autofix round: the command
/// effect for the re-run, file writes and before-images when the gate
/// keeps the edits, and the bounded autofix record either way.
fn build_effects(
    program: &str,
    args: &[String],
    staged: &[StagedFile],
    edits_total: usize,
    gate: &GateOutcome,
    rerun_result: &Result<ExecOutcome, ToolError>,
) -> ToolEffects {
    let exit_code = rerun_result
        .as_ref()
        .ok()
        .and_then(|outcome| outcome.output.status.code());
    let mut effects =
        ToolEffects::none().with_command(CommandEffect::new(program, args.to_vec(), exit_code));
    // Edit/diagnostic counts are bounded by tool budgets, far below u32.
    #[allow(clippy::cast_possible_truncation)]
    let record = AutofixEffect::new(
        edits_total as u32,
        staged.len() as u32,
        gate.before as u32,
        gate.after as u32,
        gate.kept,
    );
    if gate.kept {
        for file in staged {
            effects = effects.with_file_written(file.rel.clone());
            if let Some(image) = &file.image {
                effects = effects.with_before_image(image.clone());
            }
        }
        effects = effects.with_repair(format!(
            "autofix: applied {edits_total} machine-applicable fix(es) in {} file(s), diagnostics {} -> {}",
            staged.len(),
            gate.before,
            gate.after
        ));
    } else {
        effects = effects.with_repair(format!(
            "autofix: attempted {edits_total} fix(es), reverted (diagnostics {} -> {})",
            gate.before, gate.after
        ));
    }
    effects.with_autofix(record)
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
#[path = "autofix/tests.rs"]
mod tests;
