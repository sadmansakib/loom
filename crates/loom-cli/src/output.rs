//! Rendering of task events and the per-task efficiency summary.
//!
//! stdout carries the **result** (assistant text, or NDJSON events); stderr
//! carries diagnostics (reasoning, tool/retry notices, the summary). Logs go
//! to stderr too, so a pipeline can redirect stdout to a file and still see
//! progress.
//!
//! Iteration 1 records API usage only; it does **not** download prices or
//! compute cost (architecture §14 defers that to the benchmark harness), so
//! the summary reports tokens and wall time, not dollars.

use std::io::{IsTerminal, Write};

use loom_core::kernel::agent::{TaskError, TaskSummary};
use loom_core::kernel::event::{EventEnvelope, EventKind};
use loom_core::kernel::id::SessionId;

/// Renders the event stream to a terminal or to NDJSON.
pub struct Renderer {
    json: bool,
    color: bool,
    /// A text delta was written to stdout with no trailing newline yet.
    text_open: bool,
    /// A reasoning delta was written to stderr with no trailing newline yet.
    reasoning_open: bool,
}

impl Renderer {
    /// Creates a renderer; `json` selects NDJSON on stdout.
    pub fn new(json: bool) -> Self {
        Self {
            json,
            color: !json && std::io::stderr().is_terminal(),
            text_open: false,
            reasoning_open: false,
        }
    }

    /// Renders one event.
    pub fn render(&mut self, envelope: &EventEnvelope) {
        if self.json {
            if let Ok(line) = serde_json::to_string(envelope) {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{line}");
            }
            return;
        }

        match &envelope.kind {
            EventKind::TextDelta { text } => {
                self.close_reasoning();
                let mut out = std::io::stdout().lock();
                let _ = write!(out, "{text}");
                let _ = out.flush();
                self.text_open = true;
            }
            EventKind::ReasoningDelta { text } => {
                self.close_text();
                let mut err = std::io::stderr().lock();
                if !self.reasoning_open {
                    if self.color {
                        let _ = write!(err, "\x1b[2m");
                    }
                    self.reasoning_open = true;
                }
                let _ = write!(err, "{text}");
                let _ = err.flush();
            }
            EventKind::ModelRetry {
                attempt,
                kind,
                delay_ms,
                ..
            } => {
                self.close_all();
                self.notice(&format!(
                    "retrying: attempt {attempt} failed with {kind:?}; waiting {delay_ms} ms"
                ));
            }
            EventKind::ToolStarted { name, .. } => {
                self.close_all();
                self.notice(&format!("tool: {name}"));
            }
            EventKind::ToolCompleted { name, .. } => {
                self.close_all();
                self.notice(&format!("tool: {name} done"));
            }
            EventKind::ModelCompleted { .. } => {
                self.close_all();
            }
            EventKind::TaskStarted
            | EventKind::TurnStarted { .. }
            | EventKind::ModelRequestStarted { .. }
            | EventKind::TaskCompleted
            | EventKind::TaskFailed { .. }
            | EventKind::TaskCancelled => {}
            // `EventKind` is `#[non_exhaustive]`: a newer core may add
            // variants this iteration does not know how to render.
            _ => {}
        }
    }

    /// Flushes any dangling partial line.
    pub fn finish(&mut self) {
        self.close_all();
    }

    fn notice(&self, message: &str) {
        let mut err = std::io::stderr().lock();
        if self.color {
            let _ = writeln!(err, "\x1b[2m{message}\x1b[0m");
        } else {
            let _ = writeln!(err, "{message}");
        }
    }

    fn close_text(&mut self) {
        if self.text_open {
            let _ = writeln!(std::io::stdout().lock());
            self.text_open = false;
        }
    }

    fn close_reasoning(&mut self) {
        if self.reasoning_open {
            let mut err = std::io::stderr().lock();
            if self.color {
                let _ = writeln!(err, "\x1b[0m");
            } else {
                let _ = writeln!(err);
            }
            self.reasoning_open = false;
        }
    }

    fn close_all(&mut self) {
        self.close_text();
        self.close_reasoning();
    }
}

/// Prints the efficiency summary for a completed task.
pub fn print_summary(summary: &TaskSummary, end_reason: &str, session_id: &SessionId, json: bool) {
    if json {
        let line = serde_json::json!({
            "type": "summary",
            "task_id": summary.task_id.to_string(),
            "session_id": session_id.to_string(),
            "end_reason": end_reason,
            "turns": summary.turns,
            "model_requests": summary.model_requests,
            "input_tokens": summary.input_tokens,
            "output_tokens": summary.output_tokens,
            "cached_input_tokens": summary.cached_input_tokens,
            "successful_tool_calls": summary.successful_tool_calls,
            "failed_tool_calls": summary.failed_tool_calls,
            "wall_time_ms": summary.wall_time.as_millis() as u64,
            "events_dropped": summary.events_dropped,
        });
        if let Ok(text) = serde_json::to_string(&line) {
            let _ = writeln!(std::io::stdout().lock(), "{text}");
        }
        return;
    }

    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "task {} {end_reason}", summary.task_id);
    let _ = writeln!(
        err,
        "  turns {}  requests {}  tools {} ok / {} failed",
        summary.turns,
        summary.model_requests,
        summary.successful_tool_calls,
        summary.failed_tool_calls
    );
    let cached = summary
        .cached_input_tokens
        .map(|tokens| format!(" / {} cached", thousands(tokens)))
        .unwrap_or_default();
    let _ = writeln!(
        err,
        "  tokens {} in{} / {} out",
        thousands(summary.input_tokens),
        cached,
        thousands(summary.output_tokens)
    );
    let _ = writeln!(
        err,
        "  wall time {:.2}s  events dropped {}",
        summary.wall_time.as_secs_f64(),
        summary.events_dropped
    );
    let _ = writeln!(err, "  session {}", session_id);
}

/// Reports a terminal task error to stderr. A no-op in NDJSON mode, where the
/// `TaskFailed`/`TaskCancelled` event already carries the outcome on stdout.
pub fn print_failure(error: &TaskError, json: bool) {
    if json {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "task {}: {}", error.end_reason(), error);
}

/// Inserts thousands separators into a decimal count.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(byte as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(123), "123");
        assert_eq!(thousands(1_234), "1,234");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
