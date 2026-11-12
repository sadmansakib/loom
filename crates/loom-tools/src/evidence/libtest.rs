//! libtest human-output parsing: `---- <name> stdout ----` sections and
//! the `test <name> ... FAILED` summary fallback.

use super::{Location, TestFailure};

/// Parses libtest **human** failure output into failing tests.
///
/// Recognizes the `---- <name> stdout ----` sections and the `panicked at
/// <file>:<line>:<col>:` line inside each, followed by the panic message. Falls
/// back to the `test <name> ... FAILED` summary lines when no section body is
/// present (e.g. a filtered run).
#[must_use]
pub fn libtest_failures(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let mut current: Option<TestFailure> = None;
    // A panic message spans every line up to the blank line that ends the
    // section body. A diff-style `assert_eq!` failure prints its compared
    // values there, so stopping at the first line leaves the model with
    // nothing to act on but a blind re-edit; the whole message is kept and
    // `clip` collapses and bounds it at render time.
    let mut message_done = false;
    for line in output.lines() {
        if let Some(name) = section_name(line) {
            if let Some(failure) = current.take() {
                failures.push(failure);
            }
            current = Some(TestFailure {
                name,
                message: String::new(),
                location: None,
            });
            message_done = false;
            continue;
        }
        let Some(failure) = current.as_mut() else {
            continue;
        };
        if failure.location.is_none()
            && let Some(location) = parse_panic_location(line)
        {
            failure.location = Some(location);
            continue;
        }
        if message_done {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("note:") {
            message_done = true;
            continue;
        }
        if trimmed.is_empty() {
            // A blank line ends the section body; the libtest trailer that
            // follows must not be swallowed into the message.
            message_done = !failure.message.is_empty();
            continue;
        }
        if trimmed.starts_with("----") {
            continue;
        }
        if !failure.message.is_empty() {
            failure.message.push('\n');
        }
        failure.message.push_str(trimmed);
    }
    if let Some(failure) = current.take() {
        failures.push(failure);
    }
    if failures.is_empty() {
        for line in output.lines() {
            if let Some(name) = failed_summary_name(line) {
                failures.push(TestFailure {
                    name,
                    message: String::new(),
                    location: None,
                });
            }
        }
    }
    failures
}

/// The test name in a `---- <name> stdout ----` header line.
fn section_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("---- ")?;
    let name = rest.strip_suffix(" stdout ----")?;
    Some(name.trim().to_owned())
}

/// The test name in a `test <name> ... FAILED` summary line.
fn failed_summary_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("test ")?;
    let name = rest.strip_suffix(" ... FAILED")?;
    Some(name.trim().to_owned())
}

/// Parses `panicked at <file>:<line>:<col>:` (file may contain no colon).
fn parse_panic_location(line: &str) -> Option<Location> {
    let trimmed = line.trim();
    // libtest prints `thread '<name>' panicked at <file>:<line>:<col>:`; some
    // harnesses omit the `thread '…' ` prefix. Accept either.
    let (_, rest) = trimmed.split_once("panicked at ")?;
    let rest = rest.trim_end_matches(':');
    let mut parts = rest.rsplitn(3, ':');
    let column = parts.next()?.trim().parse().ok()?;
    let line_number = parts.next()?.trim().parse().ok()?;
    let file = parts.next()?.trim();
    if file.is_empty() {
        return None;
    }
    Some(Location::new(file, line_number, column))
}
