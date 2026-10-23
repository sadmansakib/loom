//! The six read-only LSP tools (docs/lsp-design.md §2/§3).
//!
//! The tool count is fixed regardless of how many servers attach; each
//! tool routes by file suffix to the owning server. Positions are
//! drift-tolerant: pass a `symbol` substring or a 1-based
//! `line`(+`column`). Output is 1-based `path:line:col` with
//! Serena-style shortening and a head cap; every result carries an
//! `[lsp:<server>]` provenance label.

use std::sync::{Arc, OnceLock};

use async_lsp::lsp_types;
use async_lsp::lsp_types::request::{
    DocumentDiagnosticRequest, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest,
    References, Rename, WorkspaceSymbolRequest,
};
use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::budget::{ToolBudgets, head_only};
use crate::lsp::client::LspServer;
use crate::lsp::position::{locate_symbol, render_position};

/// Result head cap for LSP tool output (Serena-style shortening plus a
/// hard ceiling keeps reference/symbol lists token-cheap).
const HEAD_BYTES: usize = 16 * 1024;

/// Shared state behind every LSP tool.
pub(crate) struct LspInner {
    /// Attached servers, spawn order.
    pub servers: Vec<Arc<LspServer>>,
    #[allow(dead_code)]
    pub budgets: ToolBudgets,
}

impl LspInner {
    fn pick_server(&self, path: &str) -> Result<Arc<LspServer>, ToolError> {
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

    fn server_list(&self) -> String {
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
    async fn resolve_position(
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

fn recent_changes(ctx: &ToolContext) -> Vec<String> {
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

fn document_uri(server: &LspServer, path: &str) -> Result<lsp_types::Url, ToolError> {
    server.document_uri(path).map_err(ToolError::InvalidInput)
}

fn loc_line(label: &str, loc: &lsp_types::Location) -> String {
    let file = loc
        .uri
        .to_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| loc.uri.to_string());
    format!("{label} {file}:{}", render_position(loc.range.start))
}

/// Renders a [`lsp_types::MarkedString`] (string or fenced language
/// block) as plain text.
fn marked_string(ms: &lsp_types::MarkedString) -> String {
    match ms {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
    }
}

fn finish(server: &LspServer, mut body: String) -> Result<ToolOutput, ToolError> {
    if body.is_empty() {
        body = "(no results)".to_string();
    }
    let (shaped, cut) = head_only(&body, HEAD_BYTES);
    let mut out = ToolOutput::new(format!("[lsp:{}]\n{shaped}", server.name));
    if cut {
        out = out.with_truncated();
    }
    Ok(out)
}

fn spec_of(name: &'static str, description: &'static str, params: serde_json::Value) -> ToolSpec {
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

lsp_read_tool!(
    LspDefinitionTool,
    "lsp_definition",
    "Find where a symbol is defined. Input: path plus `symbol` (substring) or 1-based line/column. Returns path:line:col entries.",
    PositionalInput
);

impl Tool for LspDefinitionTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: PositionalInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            let position = inner.resolve_position(&ctx, &server, &input).await?;
            let params = lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let result = server
                .request::<GotoDefinition>(params)
                .await
                .map_err(ToolError::Execution)?;
            let mut body = String::new();
            match result {
                Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => {
                    body.push_str(&loc_line("definition:", &loc));
                }
                Some(lsp_types::GotoDefinitionResponse::Array(locs)) => {
                    for loc in &locs {
                        body.push_str(&loc_line("definition:", loc));
                        body.push('\n');
                    }
                }
                Some(lsp_types::GotoDefinitionResponse::Link(links)) => {
                    for link in &links {
                        let file = link
                            .target_uri
                            .to_file_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| link.target_uri.to_string());
                        body.push_str(&format!(
                            "definition: {file}:{}\n",
                            render_position(link.target_selection_range.start)
                        ));
                    }
                }
                None => {}
            }
            finish(&server, body)
        })
    }
}

lsp_read_tool!(
    LspReferencesTool,
    "lsp_references",
    "List references to a symbol across the workspace. Input: path plus `symbol` (substring) or 1-based line/column. Returns path:line:col entries.",
    PositionalInput
);

impl Tool for LspReferencesTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: PositionalInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            let position = inner.resolve_position(&ctx, &server, &input).await?;
            let params = lsp_types::ReferenceParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: lsp_types::ReferenceContext {
                    include_declaration: true,
                },
            };
            let result = server
                .request::<References>(params)
                .await
                .map_err(ToolError::Execution)?;
            let mut body = String::new();
            if let Some(locs) = result {
                for loc in &locs {
                    body.push_str(&loc_line("ref:", loc));
                    body.push('\n');
                }
            }
            finish(&server, body)
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct PathInput {
    /// File path whose symbols to list.
    pub path: String,
}

lsp_read_tool!(
    LspSymbolsTool,
    "lsp_symbols",
    "Outline the symbols (functions, types, fields) of one file. Returns an indented outline with 1-based positions.",
    PathInput
);

impl Tool for LspSymbolsTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: PathInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            server.sync(&recent_changes(&ctx)).await;
            server
                .ensure_open(&input.path)
                .await
                .map_err(ToolError::InvalidInput)?;
            let params = lsp_types::DocumentSymbolParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: document_uri(&server, &input.path)?,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let result = server
                .request::<DocumentSymbolRequest>(params)
                .await
                .map_err(ToolError::Execution)?;
            let mut body = String::new();
            fn walk(symbols: &[lsp_types::DocumentSymbol], depth: usize, body: &mut String) {
                for s in symbols {
                    body.push_str(&"  ".repeat(depth));
                    body.push_str(&format!(
                        "{} [{:?}] {}\n",
                        s.name,
                        s.kind,
                        render_position(s.range.start)
                    ));
                    if let Some(children) = &s.children {
                        walk(children, depth + 1, body);
                    }
                }
            }
            match result {
                Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                    walk(&symbols, 0, &mut body)
                }
                Some(lsp_types::DocumentSymbolResponse::Flat(infos)) => {
                    for info in &infos {
                        body.push_str(&loc_line(&format!("{}:", info.name), &info.location));
                        body.push('\n');
                    }
                }
                None => {}
            }
            finish(&server, body)
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct QueryInput {
    /// Query matched against symbol names across the workspace.
    pub query: String,
}

lsp_read_tool!(
    LspWorkspaceSymbolsTool,
    "lsp_workspace_symbols",
    "Search symbol names across the whole workspace (all attached LSP servers). Returns path:line:col entries.",
    QueryInput
);

impl Tool for LspWorkspaceSymbolsTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let input: QueryInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let mut body = String::new();
            for server in &inner.servers {
                let params = lsp_types::WorkspaceSymbolParams {
                    query: input.query.clone(),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                };
                let Ok(result) = server.request::<WorkspaceSymbolRequest>(params).await else {
                    continue;
                };
                match result {
                    Some(lsp_types::WorkspaceSymbolResponse::Flat(infos)) => {
                        for info in &infos {
                            body.push_str(&loc_line(&format!("{}:", info.name), &info.location));
                            body.push('\n');
                        }
                    }
                    Some(lsp_types::WorkspaceSymbolResponse::Nested(symbols)) => {
                        for sym in &symbols {
                            match &sym.location {
                                lsp_types::OneOf::Left(loc) => {
                                    body.push_str(&loc_line(&format!("{}:", sym.name), loc));
                                    body.push('\n');
                                }
                                lsp_types::OneOf::Right(wl) => {
                                    body.push_str(&format!(
                                        "{}: {} (no range)\n",
                                        sym.name, wl.uri
                                    ));
                                }
                            }
                        }
                    }
                    None => {}
                }
            }
            if body.is_empty() {
                body = format!("(no results for {:?})", input.query);
            }
            let (shaped, cut) = head_only(&body, HEAD_BYTES);
            let mut out = ToolOutput::new(format!("[lsp:{}]\n{shaped}", inner.server_list()));
            if cut {
                out = out.with_truncated();
            }
            Ok(out)
        })
    }
}

lsp_read_tool!(
    LspHoverTool,
    "lsp_hover",
    "Show type/signature information for a symbol. Input: path plus `symbol` (substring) or 1-based line/column.",
    PositionalInput
);

impl Tool for LspHoverTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: PositionalInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            let position = inner.resolve_position(&ctx, &server, &input).await?;
            let params = lsp_types::HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
            };
            let result = server
                .request::<HoverRequest>(params)
                .await
                .map_err(ToolError::Execution)?;
            let mut body = String::new();
            if let Some(hover) = result {
                match hover.contents {
                    lsp_types::HoverContents::Scalar(ms) => body.push_str(&marked_string(&ms)),
                    lsp_types::HoverContents::Markup(markup) => body.push_str(&markup.value),
                    lsp_types::HoverContents::Array(items) => {
                        for item in &items {
                            body.push_str(&marked_string(item));
                            body.push('\n');
                        }
                    }
                }
            }
            finish(&server, body)
        })
    }
}

lsp_read_tool!(
    LspDiagnosticsTool,
    "lsp_diagnostics",
    "Compiler/analysis diagnostics for one file (pull when the server supports it, else the pushed cache). Returns severity, position, and message.",
    PathInput
);

impl Tool for LspDiagnosticsTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: PathInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            server.sync(&recent_changes(&ctx)).await;
            server
                .ensure_open(&input.path)
                .await
                .map_err(ToolError::InvalidInput)?;

            // The push cache is keyed by absolute path (the
            // PublishDiagnostics handler converts URIs to file paths),
            // so lookups must resolve the tool path the same way.
            let cache_path = server
                .resolve_path(&input.path)
                .to_string_lossy()
                .to_string();
            let pull_supported = server.capabilities.diagnostic_provider.is_some();
            let diagnostics = if pull_supported {
                let params = lsp_types::DocumentDiagnosticParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    identifier: None,
                    previous_result_id: None,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                };
                match server.request::<DocumentDiagnosticRequest>(params).await {
                    Ok(lsp_types::DocumentDiagnosticReportResult::Report(
                        lsp_types::DocumentDiagnosticReport::Full(full),
                    )) => full.full_document_diagnostic_report.items,
                    Ok(_) => server.cached_diagnostics(&cache_path),
                    Err(e) => {
                        tracing::warn!(server = %server.name, "pull diagnostics failed: {e}");
                        server.cached_diagnostics(&cache_path)
                    }
                }
            } else {
                // Push-only servers race the tool call against the
                // publishDiagnostics notification still in flight
                // (design §5: bounded wait, never stale-as-fresh). Poll
                // the push cache briefly before reporting empty.
                const BOUND_MS: u64 = 500;
                const STEP_MS: u64 = 50;
                let mut waited = 0;
                loop {
                    let cached = server.cached_diagnostics(&cache_path);
                    if !cached.is_empty() || waited >= BOUND_MS {
                        break cached;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(STEP_MS)).await;
                    waited += STEP_MS;
                }
            };

            let mut body = String::new();
            for d in &diagnostics {
                let severity = d
                    .severity
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "info".to_string());
                body.push_str(&format!(
                    "{} {} {}\n",
                    severity,
                    render_position(d.range.start),
                    d.message.lines().next().unwrap_or("")
                ));
            }
            finish(&server, body)
        })
    }
}

// ------------------------------------------------------------ E2: write

#[derive(Deserialize, JsonSchema)]
pub(crate) struct RenameInput {
    /// File path of the symbol (workspace-relative or absolute).
    pub path: String,
    /// 1-based line number. Optional when `symbol` is given.
    pub line: Option<u32>,
    /// 1-based column in characters. Defaults to 1 when `line` is given.
    pub column: Option<u32>,
    /// Symbol substring; the tool locates its first occurrence instead
    /// of requiring exact line/column.
    pub symbol: Option<String>,
    /// The new name for the symbol.
    pub new_name: String,
}

/// Counts the document operations in a WorkspaceEdit for output text.
fn edit_op_count(edit: &lsp_types::WorkspaceEdit) -> usize {
    match &edit.document_changes {
        Some(lsp_types::DocumentChanges::Operations(ops)) => ops.len(),
        Some(lsp_types::DocumentChanges::Edits(edits)) => edits.len(),
        None => edit.changes.as_ref().map_or(0, |m| m.len()),
    }
}

pub struct LspRenameTool {
    pub(crate) inner: Arc<LspInner>,
}

impl LspRenameTool {
    #[allow(clippy::new_without_default)]
    pub fn new(inner: Arc<LspInner>) -> Self {
        Self { inner }
    }

    fn spec_ref() -> &'static ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| {
            spec_of(
                "lsp_rename",
                "Rename a symbol across the workspace. Input: path plus `symbol` (substring) or 1-based line/column, and `new_name`. Edits land through loom's write path.",
                crate::schema::parameters::<RenameInput>(),
            )
        })
    }
}

impl Tool for LspRenameTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: RenameInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            let supported = matches!(
                &server.capabilities.rename_provider,
                Some(lsp_types::OneOf::Left(true)) | Some(lsp_types::OneOf::Right(_))
            );
            if !supported {
                return Err(ToolError::Execution(
                    "server does not support rename".to_string(),
                ));
            }
            let position = inner
                .resolve_position(
                    &ctx,
                    &server,
                    &PositionalInput {
                        path: input.path.clone(),
                        line: input.line,
                        column: input.column,
                        symbol: input.symbol.clone(),
                    },
                )
                .await?;
            let params = lsp_types::RenameParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    position,
                },
                new_name: input.new_name.clone(),
                work_done_progress_params: Default::default(),
            };
            let edit = server
                .request::<Rename>(params)
                .await
                .map_err(ToolError::Execution)?;
            let Some(edit) = edit else {
                return Ok(ToolOutput::new(format!(
                    "[lsp:{}]\n(no changes)",
                    server.name
                )));
            };
            let ops = edit_op_count(&edit);
            let effects = crate::lsp::apply::apply_workspace_edit(&server, &edit)
                .await
                .map_err(ToolError::Execution)?;
            Ok(ToolOutput::new(format!(
                "[lsp:{}]\nrenamed symbol to {} across {ops} document change(s)",
                server.name, input.new_name
            ))
            .with_effects(effects))
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct FormatInput {
    /// File path to format (workspace-relative or absolute).
    pub path: String,
    /// Tab size in spaces for the formatter (default 4).
    pub tab_size: Option<u32>,
    /// Prefer spaces over tabs (default true).
    pub insert_spaces: Option<bool>,
}

pub struct LspFormatTool {
    pub(crate) inner: Arc<LspInner>,
}

impl LspFormatTool {
    #[allow(clippy::new_without_default)]
    pub fn new(inner: Arc<LspInner>) -> Self {
        Self { inner }
    }

    fn spec_ref() -> &'static ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| {
            spec_of(
                "lsp_format",
                "Format one file with the attached language server. Optional `tab_size` (default 4) and `insert_spaces` (default true).",
                crate::schema::parameters::<FormatInput>(),
            )
        })
    }
}

impl Tool for LspFormatTool {
    fn spec(&self) -> &ToolSpec {
        Self::spec_ref()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            let input: FormatInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let server = inner.pick_server(&input.path)?;
            let supported = matches!(
                &server.capabilities.document_formatting_provider,
                Some(lsp_types::OneOf::Left(true)) | Some(lsp_types::OneOf::Right(_))
            );
            if !supported {
                return Err(ToolError::Execution(
                    "server does not support document formatting".to_string(),
                ));
            }
            server.sync(&recent_changes(&ctx)).await;
            server
                .ensure_open(&input.path)
                .await
                .map_err(ToolError::InvalidInput)?;
            let params = lsp_types::DocumentFormattingParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: document_uri(&server, &input.path)?,
                },
                options: lsp_types::FormattingOptions {
                    tab_size: input.tab_size.unwrap_or(4),
                    insert_spaces: input.insert_spaces.unwrap_or(true),
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };
            let edits = server
                .request::<Formatting>(params)
                .await
                .map_err(ToolError::Execution)?;
            let Some(edits) = edits.filter(|e| !e.is_empty()) else {
                return Ok(ToolOutput::new(format!(
                    "[lsp:{}]\n(no changes; already formatted)",
                    server.name
                )));
            };
            // Formatting is whole-document: one TextDocumentEdit with a
            // null version (no stale-text risk — we synced first).
            let edit = lsp_types::WorkspaceEdit {
                document_changes: Some(lsp_types::DocumentChanges::Edits(vec![
                    lsp_types::TextDocumentEdit {
                        text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                            uri: document_uri(&server, &input.path)?,
                            version: None,
                        },
                        edits: edits.into_iter().map(lsp_types::OneOf::Left).collect(),
                    },
                ])),
                ..Default::default()
            };
            let effects = crate::lsp::apply::apply_workspace_edit(&server, &edit)
                .await
                .map_err(ToolError::Execution)?;
            Ok(
                ToolOutput::new(format!("[lsp:{}]\nformatted {}", server.name, input.path))
                    .with_effects(effects),
            )
        })
    }
}

/// Builds all eight LSP tools (six read + rename/format) over the
/// attached servers.
pub(crate) fn lsp_tools(inner: Arc<LspInner>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(LspDefinitionTool::new(inner.clone())),
        Box::new(LspReferencesTool::new(inner.clone())),
        Box::new(LspSymbolsTool::new(inner.clone())),
        Box::new(LspWorkspaceSymbolsTool::new(inner.clone())),
        Box::new(LspHoverTool::new(inner.clone())),
        Box::new(LspDiagnosticsTool::new(inner.clone())),
        Box::new(LspRenameTool::new(inner.clone())),
        Box::new(LspFormatTool::new(inner)),
    ]
}
