//! LSP integration, slice E (docs/lsp-design.md).
//!
//! Opt-in via `--lsp`: attaches language servers from the built-in
//! `KNOWN_SERVERS` table plus `.loom/lsp.toml`, spawns them under the
//! same OS sandbox as every loom command (network off unless granted, a
//! minimal inherited environment), and exposes a fixed set of semantic
//! tools (reads plus rename/format). Without the flag nothing here runs
//! and the default tool surface is byte-identical.

mod apply;
mod client;
mod manifest;
mod position;
mod tools;

use std::path::Path;
use std::sync::Arc;

/// Crate-internal seams for the fast-path LSP fallback (item 34).
pub(crate) use apply::{apply_workspace_edit, confined_path};
pub use client::LspServer;
pub use manifest::{
    KNOWN_SERVERS, KnownServer, LspManifest, LspServerDecl, MANIFEST_RELATIVE_PATH,
    binary_available, candidates_for, manifest_for,
};
pub(crate) use tools::edit_op_count;

use tools::LspInner;

/// What attaching LSP servers yields for the engine.
#[derive(Default)]
pub struct LspAttachment {
    /// The attached LSP tools (empty when no server attached).
    pub tools: Vec<Box<dyn loom_kernel::tool::Tool>>,
    /// Facts-block menu text (a short note when no server attached).
    pub menu: Option<String>,
    /// Live server handles (empty when none attached). The fast-path
    /// LSP rename fallback (roadmap item 34) speaks to these directly;
    /// everything else goes through the tools.
    pub servers: Vec<Arc<LspServer>>,
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
         lsp_diagnostics, lsp_call_hierarchy, lsp_rename, lsp_format. Pass a file path plus a \
         symbol name (substring) or 1-based line:column; results are \
         path:line:col."
    )
}

/// One-line surface note when `--lsp` is on but nothing attached: a
/// silent no-op hides a broken install or manifest.
fn no_servers() -> LspAttachment {
    LspAttachment {
        menu: Some(
            "LSP enabled, but no servers attached (install a language server or check \
             .loom/lsp.toml)"
                .to_string(),
        ),
        ..LspAttachment::default()
    }
}

/// Attaches every eligible LSP server for the workspace.
///
/// Fail-closed on an invalid manifest (no servers, recorded error);
/// individual spawn/handshake failures warn and skip — LSP is an
/// accelerator, never a hard dependency.
pub async fn attach_lsp(workspace: &Path) -> LspAttachment {
    let manifest = match manifest_for(workspace) {
        Ok(Some(m)) => m,
        Ok(None) => LspManifest::default(),
        Err(e) => {
            tracing::warn!("{e}; attaching no LSP servers");
            return LspAttachment {
                menu: Some(format!(
                    "LSP disabled: invalid {} (see logs)",
                    manifest::MANIFEST_RELATIVE_PATH
                )),
                ..LspAttachment::default()
            };
        }
    };
    let candidates = candidates_for(workspace, &manifest);
    if candidates.is_empty() {
        return no_servers();
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
        return no_servers();
    }
    let menu = render_menu(&servers);
    let inner = Arc::new(LspInner {
        servers: servers.clone(),
    });
    LspAttachment {
        tools: tools::lsp_tools(inner),
        menu: Some(menu),
        servers,
    }
}
