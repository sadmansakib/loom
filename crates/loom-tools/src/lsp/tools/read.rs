use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use async_lsp::lsp_types;
use async_lsp::lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
    DocumentDiagnosticRequest, DocumentSymbolRequest, GotoDefinition, HoverRequest, References,
    WorkspaceSymbolRequest,
};
use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::budget::head_only;
use crate::lsp::position::render_position;

use super::shared::{
    HEAD_BYTES, LspInner, PositionalInput, document_uri, finish, loc_line, lsp_read_tool,
    marked_string, recent_changes, spec_of,
};

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
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
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
                        let file = link.target_uri.to_file_path().map_or_else(
                            |()| link.target_uri.to_string(),
                            |p| p.display().to_string(),
                        );
                        let _ = writeln!(
                            body,
                            "definition: {file}:{}",
                            render_position(link.target_selection_range.start)
                        );
                    }
                }
                None => {}
            }
            Ok(finish(&server, body))
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
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
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
            Ok(finish(&server, body))
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

/// Renders nested document symbols as an indented outline shared by
/// the symbols tool call.
fn walk_symbols(symbols: &[lsp_types::DocumentSymbol], depth: usize, body: &mut String) {
    for s in symbols {
        body.push_str(&"  ".repeat(depth));
        let _ = writeln!(
            body,
            "{} [{:?}] {}",
            s.name,
            s.kind,
            render_position(s.range.start)
        );
        if let Some(children) = &s.children {
            walk_symbols(children, depth + 1, body);
        }
    }
}

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
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            let result = server
                .request::<DocumentSymbolRequest>(params)
                .await
                .map_err(ToolError::Execution)?;
            let mut body = String::new();
            match result {
                Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                    walk_symbols(&symbols, 0, &mut body);
                }
                Some(lsp_types::DocumentSymbolResponse::Flat(infos)) => {
                    for info in &infos {
                        body.push_str(&loc_line(&format!("{}:", info.name), &info.location));
                        body.push('\n');
                    }
                }
                None => {}
            }
            Ok(finish(&server, body))
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
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
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
                                    let _ = writeln!(body, "{}: {} (no range)", sym.name, wl.uri);
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
            let body = crate::hygiene::strip_invisible(&body);
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
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
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
            Ok(finish(&server, body))
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
            // True when the served diagnostics predate the document
            // version the server has been told about (push cache only).
            let possibly_stale = |ver: Option<i32>| {
                ver.is_some_and(|v| {
                    server
                        .open_version(&cache_path)
                        .is_some_and(|open| i64::from(open) > i64::from(v.max(0)))
                })
            };
            let (diagnostics, stale) = if pull_supported {
                let params = lsp_types::DocumentDiagnosticParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    identifier: None,
                    previous_result_id: None,
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
                };
                match server.request::<DocumentDiagnosticRequest>(params).await {
                    Ok(lsp_types::DocumentDiagnosticReportResult::Report(
                        lsp_types::DocumentDiagnosticReport::Full(full),
                    )) => (full.full_document_diagnostic_report.items, false),
                    Ok(_) => {
                        let (d, v) = server.cached_diagnostics_versioned(&cache_path);
                        let stale = possibly_stale(v);
                        (d, stale)
                    }
                    Err(e) => {
                        tracing::warn!(server = %server.name, "pull diagnostics failed: {e}");
                        let (d, v) = server.cached_diagnostics_versioned(&cache_path);
                        let stale = possibly_stale(v);
                        (d, stale)
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
                    let (cached, v) = server.cached_diagnostics_versioned(&cache_path);
                    let stale = possibly_stale(v);
                    if !cached.is_empty() || waited >= BOUND_MS {
                        break (cached, stale);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(STEP_MS)).await;
                    waited += STEP_MS;
                }
            };

            let mut body = String::new();
            for d in &diagnostics {
                let severity = d
                    .severity
                    .map_or_else(|| "info".to_string(), |s| format!("{s:?}").to_lowercase());
                let _ = writeln!(
                    body,
                    "{} {} {}",
                    severity,
                    render_position(d.range.start),
                    d.message.lines().next().unwrap_or("")
                );
            }
            if stale && !diagnostics.is_empty() {
                body.push_str("(note: these diagnostics were published before the latest text version; possibly stale)\n");
            }
            Ok(finish(&server, body))
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct CallHierarchyInput {
    /// File path of the symbol (workspace-relative or absolute).
    pub path: String,
    /// 1-based line number. Optional when `symbol` is given.
    pub line: Option<u32>,
    /// 1-based column in characters. Defaults to 1 when `line` is given.
    pub column: Option<u32>,
    /// Symbol substring; the tool locates its first occurrence instead
    /// of requiring exact line/column.
    pub symbol: Option<String>,
    /// "incoming" (who calls this symbol; the default) or "outgoing"
    /// (what this symbol calls).
    pub direction: Option<String>,
}

lsp_read_tool!(
    LspCallHierarchyTool,
    "lsp_call_hierarchy",
    "Show a symbol's callers (direction \"incoming\", the default) or callees (\"outgoing\"). Input: path plus `symbol` (substring) or 1-based line/column. Returns path:line:col entries with call-site counts.",
    CallHierarchyInput
);

/// Renders one call-hierarchy entry (`callee`/`caller`) with its
/// call-site count.
fn render_call(body: &mut String, label: &str, item: lsp_types::CallHierarchyItem, sites: usize) {
    body.push_str(&loc_line(
        &format!("{label} {}", item.name),
        &lsp_types::Location {
            uri: item.uri,
            range: item.selection_range,
        },
    ));
    let _ = writeln!(body, " ({sites} call site(s))");
}

impl Tool for LspCallHierarchyTool {
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
            let input: CallHierarchyInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let outgoing = match input.direction.as_deref() {
                None | Some("incoming") => false,
                Some("outgoing") => true,
                Some(other) => {
                    return Err(ToolError::InvalidInput(format!(
                        "direction must be \"incoming\" or \"outgoing\", got {other:?}"
                    )));
                }
            };
            let server = inner.pick_server(&input.path)?;
            if server.capabilities.call_hierarchy_provider.is_none() {
                return Err(ToolError::Execution(
                    "server does not support call hierarchy".to_string(),
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
                        symbol: input.symbol,
                    },
                )
                .await?;
            let params = lsp_types::CallHierarchyPrepareParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: document_uri(&server, &input.path)?,
                    },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };
            let items = server
                .request::<CallHierarchyPrepare>(params)
                .await
                .map_err(ToolError::Execution)?;
            let Some(item) = items.unwrap_or_default().into_iter().next() else {
                return Ok(finish(&server, String::new()));
            };
            // Header names the symbol via its own definition location.
            let mut body = loc_line(
                &format!(
                    "{} of {} at",
                    if outgoing { "callees" } else { "callers" },
                    item.name
                ),
                &lsp_types::Location {
                    uri: item.uri.clone(),
                    range: item.selection_range,
                },
            );
            body.push('\n');
            if outgoing {
                let calls = server
                    .request::<CallHierarchyOutgoingCalls>(
                        lsp_types::CallHierarchyOutgoingCallsParams {
                            item,
                            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                            partial_result_params: lsp_types::PartialResultParams::default(),
                        },
                    )
                    .await
                    .map_err(ToolError::Execution)?;
                for call in calls.unwrap_or_default() {
                    render_call(&mut body, "callee", call.to, call.from_ranges.len());
                }
            } else {
                let calls = server
                    .request::<CallHierarchyIncomingCalls>(
                        lsp_types::CallHierarchyIncomingCallsParams {
                            item,
                            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                            partial_result_params: lsp_types::PartialResultParams::default(),
                        },
                    )
                    .await
                    .map_err(ToolError::Execution)?;
                for call in calls.unwrap_or_default() {
                    render_call(&mut body, "caller", call.from, call.from_ranges.len());
                }
            }
            Ok(finish(&server, body))
        })
    }
}
