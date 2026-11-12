//! Go toolchain output parsing: `go vet`/`go build` diagnostics and
//! `go test` failures.

use super::{Diagnostic, Location, TestFailure};

/// Splits a Go-shaped diagnostic line (`<file>:<line>[:<col>]: <message>`)
/// into its location and message tail, when the file is a `.go` path.
///
/// Shared by `go vet`/`go build` diagnostics (column 0 lines) and `go
/// test` log lines (indented), so the caller chooses which lines to offer.
fn go_location(line: &str) -> Option<(Location, &str)> {
    let at = line.find(".go:")?;
    let file = &line[..at + ".go".len()];
    let rest = &line[at + ".go:".len()..];
    let (numbers, message) = rest.split_once(": ")?;
    let mut fields = numbers.split(':');
    let line_number = fields.next()?.trim().parse::<u32>().ok()?;
    let column = match fields.next() {
        Some(raw) if !raw.trim().is_empty() => raw.trim().parse::<u32>().ok()?,
        _ => 1,
    };
    if fields.next().is_some() {
        return None;
    }
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    Some((Location::new(file, line_number, column), message))
}

/// Parses Go toolchain diagnostics (`go vet`, `go build`, `go test`
/// compile failures) from human output.
///
/// The stable shape is one diagnostic per line — `./pkg/math.go:12:2:
/// unreachable code` — with `# <package>` section headers, blank lines,
/// and indented test-log lines ignored. Everything kept is a real
/// complaint; the toolchain does not label a level.
#[must_use]
pub fn go_diagnostics(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for line in output.lines() {
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((location, message)) = go_location(line) else {
            continue;
        };
        diagnostics.push(Diagnostic {
            level: "error".to_owned(),
            message: message.to_owned(),
            location: Some(location),
            code: None,
        });
    }
    diagnostics
}

/// Parses `go test` human output into failing tests.
///
/// Recognizes `--- FAIL: <name> (<dur>)` sections and the indented
/// `<file>_test.go:<line>: <message>` log lines inside them. A section
/// ends at the next `--- ` marker or a bare package status line
/// (`FAIL`, `PASS`, `ok <pkg>`, `FAIL <pkg>`).
#[must_use]
pub fn go_failures(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let mut current: Option<TestFailure> = None;
    for line in output.lines() {
        if let Some(name) = line.trim().strip_prefix("--- FAIL: ") {
            if let Some(failure) = current.take() {
                failures.push(failure);
            }
            let name = name.split_once(" (").map_or(name, |(name, _)| name);
            current = Some(TestFailure {
                name: name.trim().to_owned(),
                message: String::new(),
                location: None,
            });
            continue;
        }
        if line.starts_with("--- ") {
            // A sibling PASS/SKIP section ends the failure.
            if let Some(failure) = current.take() {
                failures.push(failure);
            }
            continue;
        }
        let trimmed = line.trim();
        let Some(failure) = current.as_mut() else {
            continue;
        };
        if go_is_status(trimmed) {
            if let Some(failure) = current.take() {
                failures.push(failure);
            }
            continue;
        }
        if line.starts_with([' ', '\t'])
            && let Some((location, message)) = go_location(trimmed)
        {
            if failure.location.is_none() {
                failure.location = Some(location);
            }
            if failure.message.is_empty() {
                failure.message = message.to_owned();
            }
        }
    }
    if let Some(failure) = current.take() {
        failures.push(failure);
    }
    failures
}

/// Whether `trimmed` is a bare `go test` package status line — the
/// section-independent trailers that end any open failure section.
fn go_is_status(trimmed: &str) -> bool {
    trimmed == "FAIL"
        || trimmed == "PASS"
        || trimmed.starts_with("ok ")
        || trimmed.starts_with("FAIL")
}
