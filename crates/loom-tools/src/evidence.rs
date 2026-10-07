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
//! - nextest JUnit XML, when present (stable).
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

use std::fmt::Write as _;

/// Most diagnostics rendered before an explicit omission marker.
const MAX_DIAGNOSTICS: usize = 20;
/// Most test failures rendered before an explicit omission marker.
const MAX_FAILURES: usize = 20;
/// Longest single message kept inline.
const MAX_MESSAGE_CHARS: usize = 240;

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
fn paths_match(changed: &str, reported: &str) -> bool {
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

/// Parses `cargo --message-format=json` output into diagnostics.
///
/// Non-JSON lines (cargo status lines) and non-`compiler-message` records are
/// ignored; only `error` and `warning` levels are kept, since those are what a
/// model acts on.
#[must_use]
pub fn cargo_diagnostics(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
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
        let level = message
            .get("level")
            .and_then(|level| level.as_str())
            .unwrap_or("error")
            .to_owned();
        if level != "error" && level != "warning" {
            continue;
        }
        let text = message
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_default()
            .to_owned();
        let code = message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(|code| code.as_str())
            .map(str::to_owned);
        let location = primary_span(message);
        diagnostics.push(Diagnostic {
            level,
            message: text,
            location,
            code,
        });
    }
    diagnostics
}

/// The primary span of a cargo JSON message, if any.
fn primary_span(message: &serde_json::Value) -> Option<Location> {
    let spans = message.get("spans")?.as_array()?;
    let span = spans
        .iter()
        .find(|span| span.get("is_primary").and_then(|flag| flag.as_bool()) == Some(true))
        .or_else(|| spans.first())?;
    let file = span.get("file_name").and_then(|file| file.as_str())?;
    let line = span.get("line_start").and_then(|line| line.as_u64())? as u32;
    let column = span
        .get("column_start")
        .and_then(|column| column.as_u64())
        .unwrap_or(1) as u32;
    Some(Location::new(file, line, column))
}

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
        if failure.message.is_empty() && !line.trim().is_empty() {
            let trimmed = line.trim();
            if !trimmed.starts_with("note:") && !trimmed.starts_with("----") {
                failure.message = trimmed.to_owned();
            }
        }
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

/// Parses nextest JUnit XML into failing tests (best effort).
///
/// Only `<testcase>` blocks containing a `<failure>` element are reported; the
/// `name` attribute and the `message` attribute (or body) supply the fields.
#[must_use]
pub fn nextest_junit(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let mut rest = output;
    while let Some(start) = rest.find("<testcase") {
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        // A self-closing `<testcase .../>` is a passing case: it carries no
        // body and therefore no `<failure>`.
        if after[..=gt].trim_end().ends_with("/>") {
            rest = &after[gt + 1..];
            continue;
        }
        let Some(end) = after.find("</testcase>") else {
            break;
        };
        let block = &after[..end];
        rest = &after[end + "</testcase>".len()..];
        if !block.contains("<failure") {
            continue;
        }
        let Some(name) = attribute(block, "name") else {
            continue;
        };
        let failure_tag = block.find("<failure").map(|index| &block[index..]);
        let message = failure_tag
            .and_then(|tag| attribute(tag, "message"))
            .or_else(|| failure_tag.and_then(element_body))
            .unwrap_or_default();
        failures.push(TestFailure {
            name,
            message,
            location: None,
        });
    }
    failures
}

/// Reads `attr="value"` from a tag string.
///
/// The attribute name must start at a boundary — the start of the tag or a
/// preceding whitespace, `<`, or `/` — so a request for `name="…"` cannot be
/// satisfied by `classname="…"` (or any other attribute sharing the suffix).
fn attribute(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let mut from = 0;
    while let Some(offset) = tag[from..].find(&needle) {
        let start = from + offset;
        let boundary = start == 0
            || matches!(
                tag.as_bytes()[start - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'<' | b'/'
            );
        if boundary {
            let rest = &tag[start + needle.len()..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_owned());
        }
        from = start + 1;
    }
    None
}

/// The text between the first `>` and the next `<` in a tag fragment.
fn element_body(tag: &str) -> Option<String> {
    let start = tag.find('>')? + 1;
    let rest = &tag[start..];
    let end = rest.find('<').unwrap_or(rest.len());
    let body = rest[..end].trim();
    (!body.is_empty()).then(|| body.to_owned())
}

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
        let is_status = trimmed == "FAIL"
            || trimmed == "PASS"
            || trimmed.starts_with("ok ")
            || trimmed.starts_with("FAIL");
        let Some(failure) = current.as_mut() else {
            continue;
        };
        if is_status {
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
        if line.starts_with([' ', '\t']) || trimmed.is_empty() || line.starts_with('>') {
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
    }
    failures
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_evidence_says_so() {
        let evidence = Evidence::default();
        assert!(evidence.is_empty());
        let rendered = evidence.render();
        assert!(rendered.contains("diagnostics: none"));
        assert!(rendered.contains("tests: none failing"));
    }

    #[test]
    fn cargo_json_diagnostics_are_extracted() {
        let output = "\
    Checking app v0.1.0
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{\"code\":\"E0308\"},\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":12,\"column_start\":5,\"is_primary\":true}]}}
{\"reason\":\"build-finished\",\"success\":false}
";
        let diagnostics = cargo_diagnostics(output);
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.level, "error");
        assert_eq!(diagnostic.message, "mismatched types");
        assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
        assert_eq!(
            diagnostic.location,
            Some(Location::new("src/lib.rs", 12, 5))
        );
    }

    #[test]
    fn non_error_levels_and_non_json_lines_are_ignored() {
        let output = "\
Compiling app
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"note\",\"message\":\"fyi\",\"spans\":[]}}
not json at all
";
        assert!(cargo_diagnostics(output).is_empty());
    }

    #[test]
    fn libtest_human_failures_are_parsed() {
        let output = "\
running 2 tests
test payment::rejects_negative ... FAILED

failures:

---- payment::rejects_negative stdout ----
thread 'payment::rejects_negative' panicked at src/payment/validator.rs:42:5:
assertion `left == right` failed
  left: 200
 right: 400
note: run with `RUST_BACKTRACE=1` for a backtrace

failures:
    payment::rejects_negative

test result: FAILED. 1 passed; 1 failed
";
        let failures = libtest_failures(output);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "payment::rejects_negative");
        assert_eq!(
            failures[0].location,
            Some(Location::new("src/payment/validator.rs", 42, 5))
        );
        assert!(
            failures[0]
                .message
                .contains("assertion `left == right` failed")
        );
    }

    #[test]
    fn libtest_falls_back_to_failed_summary_lines() {
        let output =
            "test a::b ... FAILED\ntest c::d ... ok\ntest result: FAILED. 1 passed; 1 failed\n";
        let failures = libtest_failures(output);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "a::b");
        assert!(failures[0].location.is_none());
    }

    #[test]
    fn nextest_junit_attribute_requires_a_boundary() {
        // `classname` precedes `name`; a substring match would return the
        // classname as the test name.
        let output = r#"<testsuites><testsuite>
    <testcase classname="app::payment" name="payment::rejects_negative" time="0.02">
      <failure message="assertion failed">left: 200, right: 400</failure>
    </testcase>
</testsuite></testsuites>"#;
        let failures = nextest_junit(output);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "payment::rejects_negative");
        assert_eq!(failures[0].message, "assertion failed");
    }

    #[test]
    fn nextest_junit_failures_are_parsed() {
        let output = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="app" tests="2">
    <testcase name="payment::ok" classname="app" time="0.01"/>
    <testcase name="payment::rejects_negative" classname="app" time="0.02">
      <failure message="assertion failed">left: 200, right: 400</failure>
    </testcase>
  </testsuite>
</testsuites>"#;
        let failures = nextest_junit(output);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "payment::rejects_negative");
        assert_eq!(failures[0].message, "assertion failed");
    }

    #[test]
    fn linkage_marks_recent_changes() {
        let evidence = Evidence {
            diagnostics: vec![Diagnostic {
                level: "error".to_owned(),
                message: "mismatched types".to_owned(),
                location: Some(Location::new("/abs/ws/src/lib.rs", 12, 5)),
                code: Some("E0308".to_owned()),
            }],
            failures: vec![],
            changed_paths: vec!["src/lib.rs".to_owned()],
        };
        let rendered = evidence.render();
        assert!(
            rendered.contains("relevant change: src/lib.rs"),
            "{rendered}"
        );
    }

    #[test]
    fn render_is_bounded_and_marks_omissions() {
        let evidence = Evidence {
            diagnostics: (0..25)
                .map(|index| Diagnostic {
                    level: "error".to_owned(),
                    message: format!("problem {index}"),
                    location: None,
                    code: None,
                })
                .collect(),
            failures: vec![],
            changed_paths: vec![],
        };
        let rendered = evidence.render();
        assert!(
            rendered.contains("... [5 more diagnostics omitted]"),
            "{rendered}"
        );
        assert!(rendered.contains("problem 19"));
        assert!(!rendered.contains("problem 20"));
    }

    #[test]
    fn long_messages_are_clipped() {
        let evidence = Evidence {
            diagnostics: vec![],
            failures: vec![TestFailure {
                name: "t".to_owned(),
                message: "x".repeat(500),
                location: None,
            }],
            changed_paths: vec![],
        };
        let rendered = evidence.render();
        assert!(rendered.contains('…'));
        assert!(rendered.len() < 1000);
    }

    #[test]
    fn paths_match_absolute_and_relative_forms() {
        assert!(paths_match("src/lib.rs", "/ws/src/lib.rs"));
        assert!(paths_match("/ws/src/lib.rs", "src/lib.rs"));
        assert!(paths_match("src/lib.rs", "src/lib.rs"));
        assert!(!paths_match("src/lib.rs", "src/other.rs"));
    }

    #[test]
    fn go_vet_diagnostics_are_parsed() {
        let output = "\
# example.com/m [example.com/m.test]
./main.go:9:2: undefined: z
./pkg/math.go:12: unreachable code
ok  \texample.com/m\t0.5s
some prose line without a location
";
        let diagnostics = go_diagnostics(output);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0],
            Diagnostic {
                level: "error".to_owned(),
                message: "undefined: z".to_owned(),
                location: Some(Location::new("./main.go", 9, 2)),
                code: None,
            }
        );
        // No column: defaults to 1, message may span colons.
        assert_eq!(
            diagnostics[1].location,
            Some(Location::new("./pkg/math.go", 12, 1))
        );
        assert_eq!(diagnostics[1].message, "unreachable code");
    }

    #[test]
    fn go_test_failures_are_parsed_with_locations() {
        let output = "\
--- FAIL: TestSum (0.00s)
    main_test.go:12: sum(2, 2) = 5, want 4
    main_test.go:13: later log line
--- PASS: TestOther (0.00s)
--- FAIL: TestDiv (0.01s)
    div_test.go:4: division by zero
FAIL
FAIL\texample.com/m\t0.012s
";
        let failures = go_failures(output);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert_eq!(failures[0].name, "TestSum");
        // The first log line supplies location and message; later ones are
        // extra detail, not duplication.
        assert_eq!(
            failures[0].location,
            Some(Location::new("main_test.go", 12, 1))
        );
        assert_eq!(failures[0].message, "sum(2, 2) = 5, want 4");
        assert_eq!(failures[1].name, "TestDiv");
        assert_eq!(failures[1].message, "division by zero");
    }

    #[test]
    fn pytest_sections_supply_message_and_location() {
        let output = "\
FAILURES
_______________________ TestMath.test_sum _______________________

    def test_sum():
>       assert sum(2, 2) == 5
E       assert 4 == 5
E       +  where 4 = sum(2, 2)

tests/test_math.py:8: AssertionError
=========================== short test summary info ============================
";
        let failures = pytest_failures(output);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].name, "TestMath.test_sum");
        assert_eq!(failures[0].message, "assert 4 == 5 +  where 4 = sum(2, 2)");
        assert_eq!(
            failures[0].location,
            Some(Location::new("tests/test_math.py", 8, 1))
        );
    }

    #[test]
    fn pytest_falls_back_to_short_summary_lines() {
        let output = "\
=========================== short test summary info ============================
FAILED tests/test_math.py::test_sum - assert 4 == 5
FAILED tests/test_math.py::test_div - ZeroDivisionError: division by zero
============================== 2 failed in 0.01s ===============================
";
        let failures = pytest_failures(output);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert_eq!(failures[0].name, "tests/test_math.py::test_sum");
        assert_eq!(failures[0].message, "assert 4 == 5");
        assert_eq!(failures[1].message, "ZeroDivisionError: division by zero");
        assert!(failures[0].location.is_none());
    }

    #[test]
    fn compileall_errors_are_parsed() {
        let output = "\
*** Cannot compile src/broken.py ***
  File \"src/broken.py\", line 3
    def f(:
          ^
SyntaxError: invalid syntax
";
        let diagnostics = python_diagnostics(output);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0],
            Diagnostic {
                level: "error".to_owned(),
                message: "invalid syntax".to_owned(),
                location: Some(Location::new("src/broken.py", 3, 1)),
                code: None,
            }
        );
    }

    #[test]
    fn compileall_groups_without_a_position_lose_the_location() {
        // A closing error line with no File anchor: kept messageless rather
        // than guessed at... but the closing line always follows an anchor
        // in real compileall output; here the anchor has a bad line number,
        // so the location is dropped and the message kept.
        let output = "\
*** Cannot compile src/x.py ***
  File \"src/x.py\", line ?
SyntaxError: invalid syntax
";
        let diagnostics = python_diagnostics(output);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].message, "invalid syntax");
        assert!(diagnostics[0].location.is_none());
    }
}
