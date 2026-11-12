//! Python ecosystem parsing: pytest human output and
//! `python -m compileall` diagnostics.

use super::{Diagnostic, Location, TestFailure};

/// Parses pytest human output into failing tests.
///
/// Recognizes the `____ <test name> ____` sections under `FAILURES`:
/// `E   <assertion detail>` lines supply the message (joined with
/// spaces), and the outermost column-0 traceback anchor
/// (`<file>.py:<line>: <whatever>`) supplies the location. Indented
/// source lines, blank lines, `>` markers, and repr lines are body;
/// only rule bars (`====`, `----`) end a section. Falls back to the
/// `FAILED <nodeid> - <message>` short summary lines when no section
/// parsed.
#[must_use]
pub fn pytest_failures(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let mut current: Option<TestFailure> = None;
    for line in output.lines() {
        if let Some(name) = pytest_section_header(line) {
            if let Some(failure) = current.take() {
                failures.push(failure);
            }
            current = Some(TestFailure {
                name,
                message: String::new(),
                location: None,
            });
            continue;
        }
        let Some(failure) = current.as_mut() else {
            continue;
        };
        if let Some(detail) = pytest_detail(line) {
            if failure.message.is_empty() {
                failure.message = detail.to_owned();
            } else {
                failure.message.push(' ');
                failure.message.push_str(detail);
            }
            continue;
        }
        // Section body: indented source lines, blank lines, the `>`
        // failing-line marker, column-0 repr lines (`self = <...>`), and
        // traceback frames are all body; a frame anchors the location
        // (the outermost frame wins) and is followed by the `E` details
        // in real output, so it must not end the section either. Only
        // rule bars (`====`, `----`) end a section.
        let trimmed = line.trim();
        if pytest_is_body(line, trimmed) {
            continue;
        }
        if let Some(location) = python_anchor(line) {
            if failure.location.is_none() {
                failure.location = Some(location);
            }
            continue;
        }
        if (trimmed.starts_with("==") || trimmed.starts_with("--"))
            && let Some(failure) = current.take()
        {
            failures.push(failure);
        }
        // Any other column-0 line is body (reprs, markers); keep scanning.
    }
    if let Some(failure) = current.take() {
        failures.push(failure);
    }
    if failures.is_empty() {
        return pytest_summary_failures(output);
    }
    failures
}

/// The `FAILED <nodeid> - <message>` short-summary fallback: when no
/// `FAILURES` section parsed (terse output, `-q`, plugins), pytest still
/// prints one summary line per failure at the end.
fn pytest_summary_failures(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("FAILED ") else {
            continue;
        };
        let Some((nodeid, message)) = rest.split_once(" - ") else {
            continue;
        };
        failures.push(TestFailure {
            name: nodeid.trim().to_owned(),
            message: message.trim().to_owned(),
            location: None,
        });
    }
    failures
}

/// Whether a line inside a `FAILURES` section is body (indented source,
/// blank, or the `>` failing-line marker) rather than a column-0
/// structural line.
fn pytest_is_body(line: &str, trimmed: &str) -> bool {
    line.starts_with([' ', '\t']) || trimmed.is_empty() || line.starts_with('>')
}

/// The test name in a pytest `____ <name> ____` section header.
fn pytest_section_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("___") || !trimmed.ends_with("___") {
        return None;
    }
    let name = trimmed.trim_matches('_').trim();
    (!name.is_empty() && !name.starts_with('=')).then(|| name.to_owned())
}

/// The assertion detail on a pytest `E   <detail>` line.
fn pytest_detail(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('E')?;
    let detail = rest.trim_start();
    (rest.starts_with(char::is_whitespace) && !detail.is_empty())
        .then_some(detail)
        .filter(|detail| !detail.starts_with("_"))
}

/// A pytest traceback anchor: `<file>.py:<line>: <whatever>`.
fn python_anchor(line: &str) -> Option<Location> {
    let at = line.find(".py:")?;
    let file = &line[..at + ".py".len()];
    let rest = &line[at + ".py:".len()..];
    let (number, _) = rest.split_once(':')?;
    let line_number = number.trim().parse::<u32>().ok()?;
    Some(Location::new(file, line_number, 1))
}

/// Parses `python -m compileall` error output into diagnostics.
///
/// The shape is a `*** Cannot compile <path>` headline, a `File "<path>",
/// line <n>` position, and a closing `<Error>: <message>` line; only the
/// closing line is guaranteed, so incomplete groups are dropped rather
/// than guessed at.
#[must_use]
pub fn python_diagnostics(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut pending: Option<(String, Option<u32>)> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("*** Cannot compile ") {
            // The headline ends with ` ***`; strip it so the path is clean.
            let path = rest.trim().trim_end_matches('*').trim();
            pending = Some((path.to_owned(), None));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("File \"") {
            if let Some((path, number)) = rest.split_once("\", line ") {
                let line_number = number.trim().parse::<u32>().ok();
                match pending.as_mut() {
                    Some(pending) => {
                        if pending.1.is_none() {
                            pending.1 = line_number;
                        }
                    }
                    None => pending = Some((path.to_owned(), line_number)),
                }
            }
            continue;
        }
        if let Some((kind, message)) = trimmed.split_once(": ")
            && kind.ends_with("Error")
            && let Some((file, line_number)) = pending.take()
        {
            diagnostics.push(Diagnostic {
                level: "error".to_owned(),
                message: message.to_owned(),
                location: line_number.map(|line| Location::new(file, line, 1)),
                code: None,
            });
        }
    }
    diagnostics
}
