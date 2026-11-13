//! Structured failure evidence (2F, ADR-017 decision 3).
//!
//! Raw compiler and test output is exactly the unbounded tool result 2D
//! bounded and the census flagged. Verification failures are therefore
//! **compressed before they enter the model context**: the failing test, its
//! assertion/error, the source location, and the recent change it belongs to —
//! never a full process log. The full log still follows the `ArtifactRef`
//! overflow path (ADR-012/014) in the tool that produced it.
//!
//! Three stable cargo sources are parsed:
//! - compiler diagnostics, from `cargo --message-format=json` (stable);
//! - test failures, from libtest **human** output (`panicked at file:line:col`,
//!   `---- <name> stdout ----` sections) — libtest JSON is nightly-only;
//! - nextest `JUnit` XML, when present (stable).
//!
//! Beyond cargo (roadmap "verification beyond Cargo"), the same
//! [`Diagnostic`]/[`TestFailure`] shapes are parsed from other ecosystems'
//! stable human formats: Go toolchain lines (`go_diagnostics`,
//! `go_failures`), pytest sections (`pytest_failures`), and
//! `python -m compileall` groups (`python_diagnostics`). Ecosystems whose
//! runners define no stable format (npm, Make) ride the bounded raw
//! fallback instead of a guessing parser.
//!
//! Change→failure linkage is by **recently changed path** (from recorded 2B
//! `ToolEffects`), not by symbol: symbol-level linkage waits for repository
//! intelligence stage 2 (ADR-015).

mod cargo;
mod go;
mod libtest;
mod nextest;
mod python;
mod render;

pub use cargo::cargo_diagnostics;
pub use go::{go_diagnostics, go_failures};
pub use libtest::libtest_failures;
pub use nextest::nextest_junit;
pub use python::{pytest_failures, python_diagnostics};

#[cfg(test)]
use render::paths_match;

/// A source position. `file` is as the producer reported it (usually
/// workspace-relative); `line`/`column` are 1-based.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// File path as reported.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

impl Location {
    /// Builds a location.
    #[must_use]
    pub fn new(file: impl Into<String>, line: u32, column: u32) -> Self {
        Self {
            file: file.into(),
            line,
            column,
        }
    }
}

/// One compiler diagnostic, reduced to its actionable fields.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `error`, `warning`, and so on.
    pub level: String,
    /// The diagnostic message (first paragraph).
    pub message: String,
    /// Primary source location, when the diagnostic has one.
    pub location: Option<Location>,
    /// The diagnostic code (e.g. `E0308`), when present.
    pub code: Option<String>,
}

/// One failing test.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailure {
    /// The test's fully-qualified name.
    pub name: String,
    /// The panic/assertion message.
    pub message: String,
    /// Where the failure was raised, when known.
    pub location: Option<Location>,
}

/// The compressed verification evidence for one run.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    /// Compiler diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Failing tests.
    pub failures: Vec<TestFailure>,
    /// Recently changed workspace-relative paths used for linkage, oldest
    /// first. Provided from recorded effects; empty for read-only runs.
    pub changed_paths: Vec<String>,
}

#[cfg(test)]
mod tests;
