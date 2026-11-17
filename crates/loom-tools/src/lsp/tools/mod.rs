//! The LSP tools (docs/lsp-design.md §2/§3).
//!
//! The tool count is fixed regardless of how many servers attach; each
//! tool routes by file suffix to the owning server. Positions are
//! drift-tolerant: pass a `symbol` substring or a 1-based
//! `line`(+`column`). Output is 1-based `path:line:col` with
//! Serena-style shortening and a head cap; every result carries an
//! `[lsp:<server>]` provenance label.

mod read;
mod shared;
mod write;

use std::sync::Arc;

use loom_kernel::tool::Tool;

pub(crate) use shared::LspInner;
pub(crate) use write::edit_op_count;

use read::{
    LspCallHierarchyTool, LspDefinitionTool, LspDiagnosticsTool, LspHoverTool, LspReferencesTool,
    LspSymbolsTool, LspWorkspaceSymbolsTool,
};
use write::{LspFormatTool, LspRenameTool};

/// Builds all nine LSP tools (seven read + rename/format) over the
/// attached servers.
pub(crate) fn lsp_tools(inner: Arc<LspInner>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(LspDefinitionTool::new(inner.clone())),
        Box::new(LspReferencesTool::new(inner.clone())),
        Box::new(LspSymbolsTool::new(inner.clone())),
        Box::new(LspWorkspaceSymbolsTool::new(inner.clone())),
        Box::new(LspHoverTool::new(inner.clone())),
        Box::new(LspDiagnosticsTool::new(inner.clone())),
        Box::new(LspCallHierarchyTool::new(inner.clone())),
        Box::new(LspRenameTool::new(inner.clone())),
        Box::new(LspFormatTool::new(inner)),
    ]
}
