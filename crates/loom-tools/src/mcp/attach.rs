//! The attach pipeline: spawn the manifest's (or the editor-forwarded)
//! servers, fetch their tool lists, sanitize and pin them, and build the
//! registry tools plus the deferred catalog.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use loom_kernel::agent::{SurfacePolicy, ToolSurface};
use loom_kernel::tool::{Tool, ToolSpec};

use crate::policy::PolicyVerdict;

use super::McpError;
use super::catalog::{
    McpCatalog, McpCatalogEntry, McpLoadTools, McpSearchTools, one_liner, render_menu,
};
use super::client::{McpServerProcess, RawTool};
use super::hygiene::{
    contains_cross_server_reference, contains_imperative, glob_to_regex, pin_hash, sanitize_text,
    truncate_description, warn_if_unpinned_runner,
};
use super::lockfile::{McpLock, load_lock, write_lock};
use super::manifest::{McpManifest, McpServerDecl};
use super::tool::{McpTool, mcp_tool_name};
use super::{IMPERATIVE_WARNING, MANIFEST_RELATIVE_PATH, SCHEMA_MAX_BYTES, TOOLS_MAX_PER_SERVER};

// ---------------------------------------------------------------------------
// Attach
// ---------------------------------------------------------------------------

/// The model-facing name for one admitted tool, held as `Arc` so the same
/// tool can live in the catalog (deferred), the session surface (loaded),
/// and the registry (eager `always_load` pin) without duplication.
struct Surviving {
    tool: Arc<McpTool>,
    model_name: String,
}

/// What attaching every manifest server produced (slice D2).
#[derive(Default)]
pub struct McpAttachment {
    /// Tools to register eagerly on the base registry: the two
    /// meta-tools (when anything is deferred) plus `always_load` pins.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Facts-block menu naming the deferred catalog, when non-empty.
    pub menu: Option<String>,
    /// Session tool surface that promotes loaded tools between turns
    /// and evicts them after idle, when non-empty.
    pub surface: Option<Arc<ToolSurface>>,
    /// Per-server attach outcome in manifest order: `true` when the
    /// server spawned, handshook, and answered `tools/list` (individual
    /// tools may still have been filtered or deferred). Frontends show
    /// this as connection status; failures stay tracing-only here.
    pub servers: Vec<(String, bool)>,
}

/// Attaches every manifest server: spawn, handshake, `tools/list`,
/// hygiene, pinning — then defers everything except `always_load` pins
/// behind the catalog/meta-tool surface. Every failure is contained: a
/// bad manifest or lockfile yields an empty attachment; a bad server
/// contributes nothing while others proceed.
pub async fn attach_mcp(workspace: &Path, budgets: crate::budget::ToolBudgets) -> McpAttachment {
    let manifest_path = workspace.join(MANIFEST_RELATIVE_PATH);
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return McpAttachment::default();
        }
        Err(error) => {
            tracing::warn!(path = %manifest_path.display(), %error, "cannot read MCP manifest");
            return McpAttachment::default();
        }
    };
    // Manifest servers are *spawned processes with declared
    // environments* — the same power class as the command policy's
    // escalations, so the trust gate applies here too (review
    // F-02/F-12): a repo-shipped `.loom/mcp.toml` attaches nothing
    // until the operator pins it (`loom trust`).
    if crate::trust::TrustGate::open().decide(workspace, MANIFEST_RELATIVE_PATH)
        == crate::trust::Trust::Untrusted
    {
        tracing::warn!(
            path = %manifest_path.display(),
            "MCP manifest not pinned in the trust store; attaching no servers \
             (run `loom trust` to review and pin)"
        );
        return McpAttachment::default();
    }
    let manifest = match McpManifest::parse(&text) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(%error, "MCP attach skipped: invalid manifest");
            return McpAttachment::default();
        }
    };
    let mut lock = match load_lock(workspace) {
        Ok(lock) => lock,
        Err(error) => {
            // Fail closed everywhere: an unreadable lock means pins
            // cannot be verified, so no server attaches.
            tracing::warn!(%error, "MCP attach skipped: unreadable lockfile");
            return McpAttachment::default();
        }
    };
    let mut lock_dirty = false;
    let attachment = attach_declared(
        &manifest.servers,
        workspace,
        budgets,
        &mut lock,
        &mut lock_dirty,
        false,
    )
    .await;
    if lock_dirty {
        write_lock(workspace, &lock);
    }
    attachment
}

/// Attaches editor-forwarded servers (roadmap item 37 tail; see
/// `docs/acp-tails-design.md`).
///
/// Same spawn and hygiene pipeline as the manifest path — the OS
/// sandbox, scrubbed environment, and per-server caps all apply — with
/// two deliberate differences. Every surviving tool loads eagerly: the
/// editor is the tool picker, so there is no deferred catalog or menu.
/// And pins live in an ephemeral in-memory lock: nothing is written to
/// the workspace lockfile and no cross-run verification happens,
/// because the trust anchor is the operator flag that let the caller
/// forward these servers at all, not a pinned workspace state.
pub async fn attach_forwarded(
    servers: &BTreeMap<String, McpServerDecl>,
    workspace: &Path,
    budgets: crate::budget::ToolBudgets,
) -> McpAttachment {
    let mut ephemeral = McpLock::default();
    let mut lock_dirty = false;
    attach_declared(
        servers,
        workspace,
        budgets,
        &mut ephemeral,
        &mut lock_dirty,
        true,
    )
    .await
}

/// The shared attach pipeline: concurrent fetch (spawn, handshake,
/// `tools/list`), serial ingestion with hygiene and pinning, then
/// name-collision dedup and the eager/deferred split. `eager_all`
/// loads every surviving tool eagerly (forwarded servers); otherwise
/// only `always_load` pins load and the rest defer behind the catalog
/// meta-tools.
#[allow(clippy::too_many_arguments)]
async fn attach_declared(
    servers: &BTreeMap<String, McpServerDecl>,
    workspace: &Path,
    budgets: crate::budget::ToolBudgets,
    lock: &mut McpLock,
    lock_dirty: &mut bool,
    eager_all: bool,
) -> McpAttachment {
    let server_names: Vec<String> = servers.keys().cloned().collect();
    let mut eager: Vec<Surviving> = Vec::new();
    let mut deferred: Vec<McpCatalogEntry> = Vec::new();

    // Fetch phase (spawn, handshake, tools/list) runs concurrently so one
    // slow server cannot serialize attach behind its full handshake
    // timeout. Hygiene and pinning stay serial in server order so
    // lockfile writes stay deterministic.
    let fetched = futures::future::join_all(servers.iter().map(|(name, decl)| async move {
        let pair = match fetch_server(name, decl, workspace).await {
            Ok(pair) => Some(pair),
            Err(error) => {
                tracing::warn!(%error, "MCP server failed to attach");
                None
            }
        };
        (name, decl, pair)
    }))
    .await;

    // Collected before the ingestion loop moves each fetched triple.
    let servers = fetched
        .iter()
        .map(|(name, _, pair)| ((*name).clone(), pair.is_some()))
        .collect();
    for (name, decl, pair) in fetched {
        let Some((process, raw_tools)) = pair else {
            continue;
        };
        ingest_server(
            name,
            decl,
            &server_names,
            lock,
            lock_dirty,
            &mut eager,
            &mut deferred,
            &process,
            raw_tools,
            eager_all,
        );
    }

    // Name-collision dedup across servers in declaration order: later
    // sanitization collisions drop. Eager pins win over catalog entries.
    let mut taken: std::collections::BTreeSet<String> =
        eager.iter().map(|s| s.model_name.clone()).collect();
    deferred.retain(|entry| taken.insert(entry.model_name.clone()));

    let menu = (!deferred.is_empty()).then(|| render_menu(&deferred));
    let surface = (!deferred.is_empty()).then(|| {
        Arc::new(ToolSurface::new(SurfacePolicy {
            evict_after_idle_turns: budgets.mcp_idle_evict_turns,
        }))
    });

    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if let Some(surface) = &surface {
        let catalog = Arc::new(McpCatalog { entries: deferred });
        tools.push(Arc::new(McpSearchTools {
            catalog: Arc::clone(&catalog),
        }));
        tools.push(Arc::new(McpLoadTools {
            catalog,
            surface: Arc::clone(surface),
        }));
    }
    for entry in eager {
        tools.push(entry.tool);
    }
    McpAttachment {
        tools,
        menu,
        surface,
        servers,
    }
}

/// Spawns one server and fetches its raw tool list. Errors abort that
/// server only (the process handle drops, and `kill_on_drop` reaps it).
async fn fetch_server(
    name: &str,
    decl: &McpServerDecl,
    workspace: &Path,
) -> Result<(Arc<McpServerProcess>, Vec<RawTool>), McpError> {
    let process = Arc::new(McpServerProcess::spawn(name, decl, workspace)?);
    process.handshake().await?;
    let raw_tools = process.tools().await?;
    Ok((process, raw_tools))
}

/// Runs one raw tool through the ingest filters (empty name,
/// description sanitize, cross-server reference, deny glob, schema
/// cap, pin-hash quarantine). Logs the skip reason and returns `None`
/// when the tool does not make it into the plan. The parameter list
/// mirrors `ingest_server`'s ingest context on purpose: every entry is
/// a distinct filter input, not a grouping opportunity.
#[allow(clippy::too_many_arguments)]
fn admit_tool(
    name: &str,
    decl: &McpServerDecl,
    server_names: &[String],
    deny: &[regex::Regex],
    pinned: &mut BTreeMap<String, String>,
    lock_dirty: &mut bool,
    process: &Arc<McpServerProcess>,
    raw: RawTool,
) -> Option<Surviving> {
    if raw.name.is_empty() {
        tracing::warn!(server = name, "MCP tool with empty name skipped");
        return None;
    }
    let desc = truncate_description(&sanitize_text(&raw.description));
    let desc = if desc.is_empty() {
        "(no description provided)".to_owned()
    } else {
        desc
    };
    if contains_cross_server_reference(&desc, name, server_names) {
        tracing::warn!(server = name, tool = %raw.name, "MCP tool skipped: cross-server reference (shadowing class)");
        return None;
    }
    if deny.iter().any(|re| re.is_match(&raw.name)) {
        tracing::warn!(server = name, tool = %raw.name, "MCP tool skipped: denied by manifest glob");
        return None;
    }
    let Ok(schema) = serde_json::to_string(&raw.input_schema) else {
        tracing::warn!(server = name, tool = %raw.name, "MCP tool skipped: schema is not JSON");
        return None;
    };
    if schema.len() > SCHEMA_MAX_BYTES {
        tracing::warn!(
            server = name,
            tool = %raw.name,
            bytes = schema.len(),
            cap = SCHEMA_MAX_BYTES,
            "MCP tool skipped: input schema over cap (truncating a schema would misdescribe the tool)"
        );
        return None;
    }
    let hash = pin_hash(name, &decl.command, &raw.name, &desc, &schema);
    match pinned.get(&raw.name) {
        Some(known) if *known != hash => {
            tracing::warn!(
                server = name,
                tool = %raw.name,
                "MCP tool quarantined: definition or server command changed since it was pinned (delete its entry in .loom/mcp.lock to re-approve)"
            );
            return None;
        }
        None => {
            pinned.insert(raw.name.clone(), hash);
            *lock_dirty = true;
        }
        _ => {}
    }
    let desc = if contains_imperative(&desc) {
        // PolicyVerdict is #[non_exhaustive]; an equality test
        // avoids a wildcard match arm.
        if decl.policy.unknown != PolicyVerdict::Allow {
            tracing::warn!(server = name, tool = %raw.name, "MCP tool skipped: imperative phrasing in description (verdict is ask/deny)");
            return None;
        }
        tracing::warn!(server = name, tool = %raw.name, "MCP tool loaded with a warning: imperative phrasing in description");
        format!("{IMPERATIVE_WARNING} {desc}")
    } else {
        desc
    };
    let model_name = mcp_tool_name(name, &raw.name);
    Some(Surviving {
        tool: Arc::new(McpTool {
            process: Arc::clone(process),
            server: name.to_owned(),
            remote_name: raw.name.clone(),
            spec: ToolSpec {
                name: model_name.clone(),
                description: desc,
                parameters: raw.input_schema,
            },
        }),
        model_name,
    })
}

/// Ingests one server's advertisement. Errors abort that server's tools
/// only; admitted tools split into `eager` (`always_load` pins, or every
/// tool when `eager_all`) and `deferred` (catalog).
#[allow(clippy::too_many_arguments)]
fn ingest_server(
    name: &str,
    decl: &McpServerDecl,
    server_names: &[String],
    lock: &mut McpLock,
    lock_dirty: &mut bool,
    eager: &mut Vec<Surviving>,
    deferred: &mut Vec<McpCatalogEntry>,
    process: &Arc<McpServerProcess>,
    raw_tools: Vec<RawTool>,
    eager_all: bool,
) {
    let deny: Vec<regex::Regex> = decl
        .policy
        .deny
        .iter()
        .filter_map(|pattern| regex::Regex::new(&glob_to_regex(pattern)).ok())
        .collect();
    // Item 2.5: a fetch-and-run runner without a version pin makes
    // the pin below hash a moving target — say so at attach time.
    warn_if_unpinned_runner(name, &decl.command);
    let pinned = lock.servers.entry(name.to_owned()).or_default();

    // Filters run over the full advertisement; the per-server cap is
    // applied LAST, to what survived them. Capping first would let
    // denied tools consume slots that allowed tools should fill.
    let mut admitted: Vec<Surviving> = Vec::new();

    for raw in raw_tools {
        if let Some(surviving) = admit_tool(
            name,
            decl,
            server_names,
            &deny,
            pinned,
            lock_dirty,
            process,
            raw,
        ) {
            admitted.push(surviving);
        }
    }
    if admitted.len() > TOOLS_MAX_PER_SERVER {
        tracing::warn!(
            server = name,
            count = admitted.len(),
            cap = TOOLS_MAX_PER_SERVER,
            "MCP server has more tools passing ingest filters than the cap; the rest are dropped"
        );
        admitted.truncate(TOOLS_MAX_PER_SERVER);
    }
    for entry in admitted {
        // `always_load` pins match either the remote name or the
        // sanitized model-facing name; everything else defers.
        // Forwarded servers (`eager_all`) load everything: the editor
        // already picked these tools, so there is no catalog to defer
        // behind.
        let pinned = eager_all
            || decl
                .always_load
                .iter()
                .any(|pin| *pin == entry.tool.remote_name || *pin == entry.model_name);
        if pinned {
            eager.push(entry);
        } else {
            deferred.push(McpCatalogEntry {
                one_liner: one_liner(&entry.tool.spec.description),
                tool: entry.tool,
                model_name: entry.model_name,
            });
        }
    }
}
