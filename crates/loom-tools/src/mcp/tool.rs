//! The tool adapter: wraps one cataloged MCP tool call behind the
//! registry's [`Tool`] interface, with head-only result caps.

use std::sync::Arc;

use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde_json::Value;

use crate::budget::head_only;

use super::RESULT_HEAD_BYTES;
use super::client::McpServerProcess;

// ---------------------------------------------------------------------------
// Tool adapter
// ---------------------------------------------------------------------------

/// Joins a `tools/call` content array into display text. Text items are
/// kept; binary/resource items become explicit placeholders.
fn render_content(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    item.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
                Some("resource") => {
                    let text = item.pointer("/resource/text").and_then(Value::as_str);
                    match text {
                        Some(text) => parts.push(text.to_owned()),
                        None => parts.push("[resource content omitted]".to_owned()),
                    }
                }
                _ => parts.push("[non-text content omitted]".to_owned()),
            }
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
        && !structured.is_null()
        && let Ok(text) = serde_json::to_string_pretty(structured)
    {
        // 2025-06-18 structured results: when the content array carried
        // nothing renderable, fall back to the structured payload as
        // JSON text so the tool result is never silently empty.
        parts.push(text);
    }
    parts.join("\n")
}

/// Builds the model-facing tool name: `mcp_<server>_<tool>`, restricted
/// to the charset registry consumers expect. Public for tests and D2's
/// facts-block index.
#[must_use]
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    fn sanitize(part: &str) -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
    format!("mcp_{}_{}", sanitize(server), sanitize(tool))
}

/// A registered MCP tool: proxies `Tool::call` to a server process.
pub struct McpTool {
    pub(super) process: Arc<McpServerProcess>,
    pub(super) server: String,
    pub(super) remote_name: String,
    pub(super) spec: ToolSpec,
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// Conservatively false: a server tool may have any side effect.
    fn is_read_only(&self) -> bool {
        false
    }

    fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> futures::future::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        let process = Arc::clone(&self.process);
        let remote_name = self.remote_name.clone();
        let server = self.server.clone();
        let timeout = ctx.timeout;
        Box::pin(async move {
            // The engine's authoritative timeout runs outside this tool;
            // `timeout` here bounds the request at the same wall clock.
            let result = process
                .call(&remote_name, &input, timeout)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let text = format!(
                "[mcp:{server}]\n{}",
                crate::hygiene::strip_invisible(&render_content(&result))
            );
            let (head, _cut) = head_only(&text, RESULT_HEAD_BYTES);
            if result.get("isError").and_then(Value::as_bool) == Some(true) {
                return Err(ToolError::Execution(head));
            }
            Ok(ToolOutput::new(head))
        })
    }
}
