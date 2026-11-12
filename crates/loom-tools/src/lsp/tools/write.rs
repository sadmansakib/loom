use std::sync::{Arc, OnceLock};

use async_lsp::lsp_types;
use async_lsp::lsp_types::request::{Formatting, Rename};
use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;

use super::shared::{LspInner, PositionalInput, document_uri, recent_changes, spec_of};

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
pub(crate) fn edit_op_count(edit: &lsp_types::WorkspaceEdit) -> usize {
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
            let effects =
                crate::lsp::apply::apply_workspace_edit(&server, &edit, ctx.artifacts.as_ref())
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
            let effects =
                crate::lsp::apply::apply_workspace_edit(&server, &edit, ctx.artifacts.as_ref())
                    .await
                    .map_err(ToolError::Execution)?;
            Ok(
                ToolOutput::new(format!("[lsp:{}]\nformatted {}", server.name, input.path))
                    .with_effects(effects),
            )
        })
    }
}
