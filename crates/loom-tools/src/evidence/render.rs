//! Bounded rendering of `Evidence`: capped lists, clipped messages, and
//! the change-to-failure linkage.

use std::fmt::Write as _;

use super::{Diagnostic, Evidence, Location, TestFailure};

/// Most diagnostics rendered before an explicit omission marker.
const MAX_DIAGNOSTICS: usize = 20;
/// Most test failures rendered before an explicit omission marker.
const MAX_FAILURES: usize = 20;
/// Longest single message kept inline.
const MAX_MESSAGE_CHARS: usize = 240;

impl Evidence {
    /// Whether there is nothing to report.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty() && self.failures.is_empty()
    }

    /// Renders the evidence as a compact, bounded block.
    ///
    /// Lists are capped; a `... [N more ... omitted]` marker is appended when
    /// anything is dropped, and each individual message is clipped. Every
    /// diagnostic and failure whose file is among [`Self::changed_paths`]
    /// carries a `relevant change:` line — the change→failure linkage.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("failure evidence:\n");
        if !self.changed_paths.is_empty() {
            let shown = self
                .changed_paths
                .iter()
                .rev()
                .take(MAX_FAILURES)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "recent changes: {shown}");
        }
        if self.diagnostics.is_empty() {
            out.push_str("diagnostics: none\n");
        } else {
            let _ = writeln!(out, "diagnostics ({}):", self.diagnostics.len());
            for diagnostic in self.diagnostics.iter().take(MAX_DIAGNOSTICS) {
                self.render_diagnostic(&mut out, diagnostic);
            }
            if self.diagnostics.len() > MAX_DIAGNOSTICS {
                let _ = writeln!(
                    out,
                    "... [{} more diagnostics omitted]",
                    self.diagnostics.len() - MAX_DIAGNOSTICS
                );
            }
        }
        if self.failures.is_empty() {
            out.push_str("tests: none failing\n");
        } else {
            let _ = writeln!(out, "failing tests ({}):", self.failures.len());
            for failure in self.failures.iter().take(MAX_FAILURES) {
                self.render_failure(&mut out, failure);
            }
            if self.failures.len() > MAX_FAILURES {
                let _ = writeln!(
                    out,
                    "... [{} more test failures omitted]",
                    self.failures.len() - MAX_FAILURES
                );
            }
        }
        out.pop();
        out
    }

    fn render_diagnostic(&self, out: &mut String, diagnostic: &Diagnostic) {
        let _ = write!(out, "- [{}] {}", diagnostic.level, diagnostic.message);
        if let Some(code) = &diagnostic.code {
            let _ = write!(out, " ({code})");
        }
        if let Some(location) = &diagnostic.location {
            let _ = write!(
                out,
                " at {}:{}:{}",
                location.file, location.line, location.column
            );
        }
        out.push('\n');
        self.link_changed(out, diagnostic.location.as_ref());
    }

    fn render_failure(&self, out: &mut String, failure: &TestFailure) {
        let _ = writeln!(out, "- test: {}", failure.name);
        let message = clip(&failure.message);
        if !message.is_empty() {
            let _ = writeln!(out, "  message: {message}");
        }
        if let Some(location) = &failure.location {
            let _ = writeln!(
                out,
                "  at {}:{}:{}",
                location.file, location.line, location.column
            );
        }
        self.link_changed(out, failure.location.as_ref());
    }

    /// Emits `relevant change: <path>` when the location's file is a recent
    /// change (change→failure linkage, ADR-017).
    fn link_changed(&self, out: &mut String, location: Option<&Location>) {
        let Some(location) = location else {
            return;
        };
        if let Some(path) = self
            .changed_paths
            .iter()
            .find(|changed| paths_match(changed, &location.file))
        {
            let _ = writeln!(out, "  relevant change: {path}");
        }
    }
}

/// Whether a changed path and a reported file refer to the same file, allowing
/// either to be a suffix of the other (absolute vs workspace-relative forms).
pub(super) fn paths_match(changed: &str, reported: &str) -> bool {
    let changed = changed.trim_end_matches('/');
    let reported = reported.trim_end_matches('/');
    changed == reported
        || changed.ends_with(&format!("/{reported}"))
        || reported.ends_with(&format!("/{changed}"))
}

/// Clips a message to [`MAX_MESSAGE_CHARS`] on a char boundary, collapsing
/// whitespace so a multi-line panic stays one line.
fn clip(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_MESSAGE_CHARS {
        return collapsed;
    }
    let mut clipped: String = collapsed.chars().take(MAX_MESSAGE_CHARS).collect();
    clipped.push('…');
    clipped
}
