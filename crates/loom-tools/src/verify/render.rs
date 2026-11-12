use crate::budget::{self, ToolBudgets};
use crate::evidence::{
    Evidence, cargo_diagnostics, go_diagnostics, go_failures, libtest_failures, nextest_junit,
    pytest_failures, python_diagnostics,
};
use loom_kernel::tool::ToolOutput;

use super::ecosystem::Ecosystem;

/// Shapes one verification run into a bounded, structured [`ToolOutput`].
pub(crate) fn render(
    ecosystem: Ecosystem,
    program: &str,
    args: &[String],
    output: &std::process::Output,
    recent_changes: &[String],
    budgets: &ToolBudgets,
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
        budgets.command_head_bytes,
        budgets.command_tail_bytes,
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

/// Reads the pass/fail verdict out of a rendered verify output: the
/// status line is always the second line (`$ cmd`, then `exit status:`).
/// A signal death or unparseable shape yields `None` — no verdict.
pub(super) fn status_from_content(content: &str) -> Option<bool> {
    match content.lines().nth(1)?.strip_prefix("exit status: ") {
        Some("0") => Some(true),
        // Only a numeric code is a verdict; signal deaths and anything
        // else unparseable are "no verdict".
        Some(code) if code.bytes().all(|b| b.is_ascii_digit()) && !code.is_empty() => Some(false),
        _ => None,
    }
}
