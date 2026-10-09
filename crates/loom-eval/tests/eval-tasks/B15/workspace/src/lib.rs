//! Shapes the outcome of a command run for the model.
//!
//! ## Contract
//!
//! [`libtest_failures`] parses libtest's human output — what
//! `cargo test` prints — into structured failures:
//!
//! * a line `test <name> ... FAILED` marks `<name>` failed, in
//!   document order;
//! * a later line `thread '<name>' panicked at <location>:` attaches
//!   `<location>` (without the trailing colon) to the failure of the
//!   same name;
//! * the message is the next non-empty line after the panicked line,
//!   unless it starts with `note:` — then there is no message.
//!
//! [`shape`] renders the whole run:
//!
//! ```text
//! $ <program>
//! exit status: <code>
//! ```
//!
//! followed, when the run **failed and** the parse found failures, by
//! the evidence block:
//!
//! ```text
//! --- test failures ---
//! <name>: panicked at <location>
//! <message>
//! ```
//!
//! one pair of lines per failure (the message line is omitted when
//! empty), and, when `changed_paths` is non-empty, by:
//!
//! ```text
//! --- recent changes ---
//! - <path>
//! ```
//!
//! Otherwise — a successful run, or a failure with nothing parseable —
//! the raw streams are kept: `--- stdout ---` and `--- stderr ---`
//! sections in that order, each omitted when empty, each carrying its
//! stream with a trailing newline trimmed. A failure is never hidden,
//! only shaped.

/// One failed test from libtest's human output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestFailure {
    pub name: String,
    pub location: String,
    pub message: String,
}

/// Parses libtest human output into the failures it reports.
pub fn libtest_failures(_stdout: &str) -> Vec<TestFailure> {
    Vec::new()
}

/// Renders a command run: shaped evidence for a parseable failure,
/// raw streams otherwise.
pub fn shape(program: &str, exit_code: i32, stdout: &str, stderr: &str, changed_paths: &[&str]) -> String {
    let mut text = format!("$ {program}\nexit status: {exit_code}");
    let failures = libtest_failures(stdout);
    if exit_code != 0 && !failures.is_empty() {
        text.push_str("\n--- test failures ---");
        for failure in &failures {
            text.push_str(&format!("\n{}: panicked at {}", failure.name, failure.location));
            if !failure.message.is_empty() {
                text.push_str(&format!("\n{}", failure.message));
            }
        }
        if !changed_paths.is_empty() {
            text.push_str("\n--- recent changes ---");
            for path in changed_paths {
                text.push_str(&format!("\n- {path}"));
            }
        }
    } else {
        if !stdout.is_empty() {
            text.push_str("\n--- stdout ---\n");
            text.push_str(stdout.trim_end_matches('\n'));
        }
        if !stderr.is_empty() {
            text.push_str("\n--- stderr ---\n");
            text.push_str(stderr.trim_end_matches('\n'));
        }
    }
    text.push('\n');
    text
}
