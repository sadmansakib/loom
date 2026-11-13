//! Model-visible rendering of one completed process: the evidence /
//! reduced / raw body shapes, failure shaping, and artifact tee.

use std::fmt::Write as _;

use loom_kernel::artifact::{ArtifactRef, store_artifact};
use loom_kernel::tool::{CommandEffect, ToolContext, ToolEffects, ToolOutput};

use crate::budget::{self, ToolBudgets};
use crate::evidence::{Evidence, cargo_diagnostics, libtest_failures, nextest_junit};
use crate::reduce;

use super::process::status_line;

/// Renders one completed process as model-visible text, recording the
/// command as an effect (ADR-012). A non-zero exit is a *successful*
/// tool call: the command ran and the model must see the failure, so it
/// is never mapped to [`ToolError::Execution`].
///
/// Three body shapes, best-fit first (the output filters, roadmap
/// item 3):
/// - a **failed** run whose output parses into structured evidence —
///   compiler diagnostics, libtest failures, nextest `JUnit`, the same
///   parsers `verify` uses — is shaped into that compact block
///   (ADR-014, ADR-017 decision 3), and the raw log it replaced is
///   teed to `ctx`'s artifact store for refetch, so nothing is lost;
/// - a **successful** well-shaped command (see [`crate::reduce`]) with
///   a store attached — the reducer's compact stdout, raw stderr, and
///   the unreduced stdout teed;
/// - anything else — the bounded raw streams. `run_command` is
///   general-purpose, so its output may itself be the answer the
///   model asked for, and a failure is never hidden, only capped.
///
/// Tees are best-effort: without an artifact store, or if the store
/// refuses the write, the result is the bounded raw output it would
/// have been before filters existed.
pub(super) fn render(
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
    ctx: &ToolContext,
    budgets: &ToolBudgets,
) -> ToolOutput {
    let effects = ToolEffects::none().with_command(CommandEffect::new(
        program,
        args.to_vec(),
        output.status.code(),
    ));
    let raw = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut diagnostics = cargo_diagnostics(&raw);
    // Diagnostics can also be emitted on stderr by some toolchains; parse
    // it only if stdout yielded none, so the two never duplicate.
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

    let mut text = format!("$ {program}");
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text.push('\n');
    text.push_str(&status_line(output.status));

    let artifact = if !output.status.success() {
        failure_body(&mut text, ctx, &evidence, &raw, &stderr)
    } else if ctx.artifacts.is_some()
        && let Some(reduction) = reduce::reduce(program, args, &raw)
    {
        reduced_body(&mut text, ctx, &raw, &stderr, &reduction.text)
    } else {
        push_streams(&mut text, &raw, &stderr);
        None
    };
    // A `bwrap:` line on a failed run means the wrapper died *before*
    // the command executed — host policy, not the agent's command — so
    // the output gets actionable remediation instead of reading as a
    // defect in what was asked for. Ordinary failures match nothing
    // and stay unannotated (docs/linux-sandbox.md).
    if !output.status.success()
        && let Some(remediation) = crate::sandbox::bwrap_remediation(&stderr)
    {
        text.push_str("\n--- sandbox ---\n");
        text.push_str(&remediation);
    }
    // Errors live at the tail, so keep both ends (ADR-014). The status line and
    // early output survive at the head; the last lines of stderr survive at the
    // tail; the explicit marker names what was dropped.
    let (text, cut) = budget::head_tail(
        &text,
        budgets.command_head_bytes,
        budgets.command_tail_bytes,
    );
    let mut rendered = if cut {
        ToolOutput::new(text).with_truncated()
    } else {
        ToolOutput::new(text)
    };
    if let Some(reference) = artifact {
        rendered = rendered.with_artifact(reference);
    }
    rendered.with_effects(effects)
}

/// The failure path: when the output parses into structured evidence
/// (2F, ADR-017) the compact block replaces the raw log — and the
/// replaced log is teed to the artifact store so the elided detail stays
/// refetchable (roadmap item 3's "nothing is lost"). Nothing parseable
/// keeps the bounded raw streams, so a failure is never hidden.
fn failure_body(
    text: &mut String,
    ctx: &ToolContext,
    evidence: &Evidence,
    raw: &str,
    stderr: &str,
) -> Option<ArtifactRef> {
    if evidence.is_empty() {
        push_streams(text, raw, stderr);
        return None;
    }
    text.push('\n');
    text.push_str(&evidence.render());
    let mut body = String::from("--- stdout ---\n");
    body.push_str(raw);
    body.push_str("\n--- stderr ---\n");
    body.push_str(stderr);
    tee(text, ctx, &body)
}

/// The reduced-success path: the reducer's compact stdout inline, the
/// raw stderr verbatim beside it, and the unreduced stdout teed to the
/// artifact store for refetch.
fn reduced_body(
    text: &mut String,
    ctx: &ToolContext,
    raw: &str,
    stderr: &str,
    reduced: &str,
) -> Option<ArtifactRef> {
    push_streams(text, reduced, stderr);
    tee(text, ctx, raw)
}

/// The raw path: both bounded streams inline, nothing teed.
fn push_streams(text: &mut String, raw: &str, stderr: &str) {
    if !raw.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(raw);
    }
    if !stderr.is_empty() {
        text.push_str("\n--- stderr ---\n");
        text.push_str(stderr);
    }
}

/// The `kind` recorded on teed command output — the same kind the
/// engine's oversized-output spill uses, so `artifact_read` serves both.
const ARTIFACT_KIND: &str = "tool_output";

/// Stores `body` as an artifact and appends the refetch note, mirroring
/// the engine's spill note (ADR-012 decision 3, ADR-014). Best-effort:
/// without a store, or when the store refuses the write, the caller's
/// inline text is all there is.
fn tee(text: &mut String, ctx: &ToolContext, body: &str) -> Option<ArtifactRef> {
    let store = ctx.artifacts.as_ref()?;
    let reference = store_artifact(store.as_ref(), ARTIFACT_KIND, body.as_bytes()).ok()?;
    let _ = writeln!(
        text,
        "\n... [full output stored as artifact {} ({} bytes); read it with artifact_read]",
        reference.id, reference.bytes
    );
    Some(reference)
}
