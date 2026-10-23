//! LSP integration, slice E (docs/lsp-design.md).
//!
//! Opt-in via `--lsp`: attaches language servers from the built-in
//! KNOWN_SERVERS table plus `.loom/lsp.toml`, spawns them under the
//! same OS sandbox as every loom command (network off unless granted,
//! no environment inheritance), and exposes a fixed set of read-only
//! semantic tools. Without the flag nothing here runs and the default
//! tool surface is byte-identical.

mod apply;
mod client;
mod manifest;
mod position;
mod tools;

use std::path::Path;
use std::sync::Arc;

pub use client::LspServer;
pub use manifest::{
    KNOWN_SERVERS, KnownServer, LspManifest, LspServerDecl, MANIFEST_RELATIVE_PATH,
    binary_available, candidates_for, manifest_for,
};

use crate::budget::ToolBudgets;
use tools::LspInner;

/// What attaching LSP servers yields for the engine.
#[derive(Default)]
pub struct LspAttachment {
    /// The six read tools (empty when no server attached).
    pub tools: Vec<Box<dyn loom_kernel::tool::Tool>>,
    /// Facts-block menu text (None when no server attached).
    pub menu: Option<String>,
}

/// Renders the compact facts menu (~100 tokens): servers plus the tool
/// surface and the position convention.
fn render_menu(servers: &[Arc<LspServer>]) -> String {
    let listing = servers
        .iter()
        .map(|s| match s.known {
            Some(known) => format!("{} ({})", s.name, known.language),
            None => s.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "LSP servers attached: {listing}. Semantic tools: lsp_definition, \
         lsp_references, lsp_symbols, lsp_workspace_symbols, lsp_hover, \
         lsp_diagnostics, lsp_rename, lsp_format. Pass a file path plus a \
         symbol name (substring) or 1-based line:column; results are \
         path:line:col."
    )
}

/// Attaches every eligible LSP server for the workspace.
///
/// Fail-closed on an invalid manifest (no servers, recorded error);
/// individual spawn/handshake failures warn and skip — LSP is an
/// accelerator, never a hard dependency.
pub async fn attach_lsp(workspace: &Path, budgets: ToolBudgets) -> LspAttachment {
    let manifest = match manifest_for(workspace) {
        Ok(Some(m)) => m,
        Ok(None) => LspManifest::default(),
        Err(e) => {
            tracing::warn!("{e}; attaching no LSP servers");
            return LspAttachment::default();
        }
    };
    let candidates = candidates_for(workspace, &manifest);
    if candidates.is_empty() {
        return LspAttachment::default();
    }

    let mut servers: Vec<Arc<LspServer>> = Vec::new();
    for candidate in &candidates {
        if !manifest::binary_available(&candidate.command)
            && !candidate.command.first().is_some_and(|c| c.contains('/'))
        {
            tracing::warn!(
                server = %candidate.name,
                "declared LSP server binary not found; skipping"
            );
            continue;
        }
        let settings = manifest
            .servers
            .get(&candidate.name)
            .and_then(|decl| decl.settings.clone())
            .unwrap_or(serde_json::Value::Null);
        match client::spawn_server(candidate, workspace, settings).await {
            Ok(server) => servers.push(server),
            Err(e) => tracing::warn!(server = %candidate.name, "{e}; skipping"),
        }
    }
    if servers.is_empty() {
        return LspAttachment::default();
    }
    let menu = render_menu(&servers);
    let inner = Arc::new(LspInner { servers, budgets });
    LspAttachment {
        tools: tools::lsp_tools(inner),
        menu: Some(menu),
    }
}
