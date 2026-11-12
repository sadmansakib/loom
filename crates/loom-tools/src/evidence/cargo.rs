//! Cargo diagnostics: parsing `cargo --message-format=json` output.

use super::{Diagnostic, Location};

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
