use std::sync::Arc;

use async_lsp::lsp_types;
use loom_kernel::tool::{ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::budget::{ToolBudgets, head_only};
use crate::lsp::client::LspServer;
use crate::lsp::position::{locate_symbol, render_position};

/// Result head cap for LSP tool output (Serena-style shortening plus a
/// hard ceiling keeps reference/symbol lists token-cheap).
pub(super) const HEAD_BYTES: usize = 16 * 1024;

/// Shared state behind every LSP tool.
pub(crate) struct LspInner {
    /// Attached servers, spawn order.
    pub servers: Vec<Arc<LspServer>>,
    #[allow(dead_code)]
    pub budgets: ToolBudgets,
}

impl LspInner {
    pub(super) fn pick_server(&self, path: &str) -> Result<Arc<LspServer>, ToolError> {
        self.servers
            .iter()
            .find(|s| s.owns_path(path))
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidInput(format!(
                    "no attached LSP server owns {path}; attached: {}",
                    self.server_list()
                ))
            })
    }

    pub(super) fn server_list(&self) -> String {
        if self.servers.is_empty() {
            "none".to_string()
        } else {
            self.servers
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    /// Syncs recent changes, opens the document, and resolves
    /// symbol-or-line input to an LSP position (docs/lsp-design.md §5).
    pub(super) async fn resolve_position(
        &self,
        ctx: &ToolContext,
        server: &LspServer,
        input: &PositionalInput,
    ) -> Result<lsp_types::Position, ToolError> {
        server.sync(&recent_changes(ctx)).await;
        server
            .ensure_open(&input.path)
            .await
            .map_err(ToolError::InvalidInput)?;
        let (line, column) = match (&input.symbol, input.line) {
            (Some(symbol), _) => {
                let full = server.resolve_path(&input.path);
                let text = tokio::fs::read_to_string(&full).await.map_err(|e| {
                    ToolError::InvalidInput(format!("cannot read {}: {e}", input.path))
                })?;
                let lines: Vec<&str> = text.lines().collect();
                let hit = locate_symbol(&lines, symbol).ok_or_else(|| {
                    ToolError::InvalidInput(format!(
                        "symbol {symbol:?} not found in {}",
                        input.path
                    ))
                })?;
                (hit.line, hit.column)
            }
            (None, Some(line)) => (line, input.column),
            (None, None) => {
                return Err(ToolError::InvalidInput(
                    "provide either `symbol` or `line` (1-based)".to_string(),
                ));
            }
        };
        server
            .surface_position(&input.path, line, column)
            .await
            .map_err(ToolError::InvalidInput)
    }
}

pub(super) fn recent_changes(ctx: &ToolContext) -> Vec<String> {
    ctx.recent_changes
        .lock()
        .map(|c| c.clone())
        .unwrap_or_default()
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PositionalInput {
    /// File path to query (workspace-relative or absolute).
    pub path: String,
    /// 1-based line number. Optional when `symbol` is given.
    pub line: Option<u32>,
    /// 1-based column in characters. Defaults to 1 when `line` is given.
    pub column: Option<u32>,
    /// Symbol substring; the tool locates its first occurrence instead
    /// of requiring exact line/column.
    pub symbol: Option<String>,
}

pub(super) fn document_uri(server: &LspServer, path: &str) -> Result<lsp_types::Url, ToolError> {
    server.document_uri(path).map_err(ToolError::InvalidInput)
}

pub(super) fn loc_line(label: &str, loc: &lsp_types::Location) -> String {
    let file = loc
        .uri
        .to_file_path()
        .map_or_else(|()| loc.uri.to_string(), |p| p.display().to_string());
    format!("{label} {file}:{}", render_position(loc.range.start))
}

/// Renders a [`lsp_types::MarkedString`] (string or fenced language
/// block) as plain text.
pub(super) fn marked_string(ms: &lsp_types::MarkedString) -> String {
    match ms {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
    }
}

pub(super) fn finish(server: &LspServer, mut body: String) -> ToolOutput {
    if body.is_empty() {
        body = "(no results)".to_string();
    }
    // Server text is untrusted: strip the invisible-character class
    // (zero-width, bidi, controls) before it enters the transcript.
    // Layout survives — results are often code.
    let body = crate::hygiene::strip_invisible(&body);
    let (shaped, cut) = head_only(&body, HEAD_BYTES);
    let mut out = ToolOutput::new(format!("[lsp:{}]\n{shaped}", server.name));
    if cut {
        out = out.with_truncated();
    }
    out
}

pub(super) fn spec_of(
    name: &'static str,
    description: &'static str,
    params: serde_json::Value,
) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters: params,
    }
}

/// Declares one read tool: struct + constructor + cached spec. The
/// `Tool` impl stays per-tool (single, non-conflicting) because each
/// `call` body is unique.
macro_rules! lsp_read_tool {
    ($ty:ident, $name:literal, $desc:literal, $input:ty) => {
        pub struct $ty {
            pub(crate) inner: Arc<LspInner>,
        }

        impl $ty {
            #[allow(clippy::new_without_default)]
            pub fn new(inner: Arc<LspInner>) -> Self {
                Self { inner }
            }

            fn spec_ref() -> &'static ToolSpec {
                static SPEC: OnceLock<ToolSpec> = OnceLock::new();
                SPEC.get_or_init(|| spec_of($name, $desc, crate::schema::parameters::<$input>()))
            }
        }
    };
}

pub(crate) use lsp_read_tool;
