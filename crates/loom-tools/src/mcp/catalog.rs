//! The deferred catalog and the two meta-tools (slice D2):
//! `mcp_search_tools` (deterministic scoring) and `mcp_load_tools`
//! (queueing onto the session surface).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::OnceLock;

use loom_kernel::agent::ToolSurface;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::Value;

use super::IMPERATIVE_WARNING;
use super::tool::McpTool;

// ---------------------------------------------------------------------------
// Deferred catalog + meta-tools (slice D2)
// ---------------------------------------------------------------------------

/// One cataloged (deferred) tool: the constructed [`McpTool`] plus the
/// one-line menu rendering of its description.
#[derive(Clone)]
pub(super) struct McpCatalogEntry {
    pub(super) tool: Arc<McpTool>,
    pub(super) model_name: String,
    pub(super) one_liner: String,
}

/// The deferred tools across every attached server, in deterministic
/// attach order (server name, then server list order).
pub(super) struct McpCatalog {
    pub(super) entries: Vec<McpCatalogEntry>,
}

impl McpCatalog {
    pub(super) fn find(&self, model_name: &str) -> Option<&McpCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.model_name == model_name)
    }
}

/// Caps a menu line: first sentence, else first 80 characters. Strips
/// the imperative warning prefix — the menu names tools, it does not
/// carry warnings (the full description keeps them).
pub(super) fn one_liner(desc: &str) -> String {
    let clean = desc
        .strip_prefix(IMPERATIVE_WARNING)
        .map_or(desc, str::trim_start);
    let sentence_end = clean.find(". ").map(|index| index + 1);
    let cut = sentence_end.unwrap_or(clean.len()).min(
        clean
            .char_indices()
            .nth(80)
            .map_or(clean.len(), |(index, _)| index),
    );
    clean[..cut].trim_end().to_owned()
}

/// Menu entries shown before "+N more tools" takes over.
const MENU_MAX_ENTRIES: usize = 64;

/// Renders the facts-block menu for the deferred catalog.
pub(super) fn render_menu(entries: &[McpCatalogEntry]) -> String {
    let mut out = String::from(
        "MCP servers attached; their tools are deferred to keep context small. \
         Find tools with mcp_search_tools, load what you need with mcp_load_tools:",
    );
    for (shown, entry) in entries.iter().enumerate() {
        if shown == MENU_MAX_ENTRIES {
            let _ = write!(
                out,
                "\n+ {} more tools; use mcp_search_tools to see them",
                entries.len() - shown
            );
            break;
        }
        let _ = write!(out, "\n{} — {}", entry.model_name, entry.one_liner);
    }
    out
}

/// Deterministic relevance scoring of one catalog entry against the
/// query words: name word-match (5) > name substring (3) > description
/// word-match (2) > description substring (1); a whole-query
/// subsequence of the name rescues zero-scored entries at 1 so partial
/// names still surface.
fn score_entry(entry: &McpCatalogEntry, words: &[String], query: &str) -> u32 {
    let name = entry.model_name.to_lowercase();
    let desc = entry.tool.spec.description.to_lowercase();
    let mut score = 0;
    for word in words {
        let name_hit = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|part| part == word);
        if name_hit {
            score += 5;
        } else if name.contains(word) {
            score += 3;
        }
        let desc_hit = desc
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|part| part == word);
        if desc_hit {
            score += 2;
        } else if desc.contains(word) {
            score += 1;
        }
    }
    if score == 0 {
        // Subsequence: every query character appears in name order.
        let mut cursor = 0;
        let matched = query.chars().filter(char::is_ascii_alphanumeric).all(|c| {
            if let Some(found) = name[cursor..].find(c) {
                cursor += found + c.len_utf8();
                true
            } else {
                false
            }
        });
        if matched {
            score = 1;
        }
    }
    score
}

/// `mcp_search_tools`: ranks the deferred catalog against a query. The
/// model never sees the ~55K tokens of eager schemas — it searches a
/// name+one-liner index and loads only what scores.
pub struct McpSearchTools {
    pub(super) catalog: Arc<McpCatalog>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchInput {
    /// Free-text query; each whitespace-separated word is scored
    /// against every deferred tool's name and description.
    query: String,
    /// Maximum entries to return (default 10).
    #[serde(default)]
    limit: Option<usize>,
}

/// Results per `mcp_search_tools` response.
const SEARCH_DEFAULT_LIMIT: usize = 10;

impl Tool for McpSearchTools {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "mcp_search_tools".to_owned(),
            description: "Search the deferred MCP tool catalog by keyword. \
                Returns matching tool names with one-line descriptions; load \
                the ones you need with mcp_load_tools (they become callable \
                on the following turn)."
                .to_owned(),
            parameters: crate::schema::parameters::<SearchInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
    ) -> futures::future::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        let catalog = Arc::clone(&self.catalog);
        Box::pin(async move {
            let input: SearchInput = serde_json::from_value(input)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            let words: Vec<String> = input
                .query
                .to_lowercase()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            if words.is_empty() {
                return Err(ToolError::InvalidInput(
                    "query must contain at least one keyword".to_owned(),
                ));
            }
            let query = input.query.to_lowercase().replace(' ', "");
            let mut scored: Vec<(u32, &McpCatalogEntry)> = catalog
                .entries
                .iter()
                .filter_map(|entry| {
                    let score = score_entry(entry, &words, &query);
                    (score > 0).then_some((score, entry))
                })
                .collect();
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.model_name.cmp(&b.1.model_name))
            });
            let limit = input.limit.unwrap_or(SEARCH_DEFAULT_LIMIT).clamp(1, 50);
            if scored.is_empty() {
                return Ok(ToolOutput::new(format!(
                    "No deferred MCP tools match {:?}.",
                    input.query
                )));
            }
            let mut out = format!(
                "{} matching deferred tool(s), top {} shown (load with mcp_load_tools):",
                scored.len(),
                scored.len().min(limit)
            );
            for (_, entry) in scored.iter().take(limit) {
                let _ = write!(out, "\n{} — {}", entry.model_name, entry.one_liner);
            }
            Ok(ToolOutput::new(out))
        })
    }
}

/// `mcp_load_tools`: queues named catalog tools onto the session
/// surface. The agent loop promotes queued tools between turns
/// (append-only, cache-stable) and evicts them again after
/// `mcp_idle_evict_turns` unused turns.
pub struct McpLoadTools {
    pub(super) catalog: Arc<McpCatalog>,
    pub(super) surface: Arc<ToolSurface>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LoadInput {
    /// Deferred tool names exactly as `mcp_search_tools` showed them
    /// (e.g. `mcp_mock_greet`).
    tools: Vec<String>,
}

impl Tool for McpLoadTools {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "mcp_load_tools".to_owned(),
            description: "Load deferred MCP tools onto the session tool \
                surface by their exact names (as shown by \
                mcp_search_tools). Loaded tools become callable on the \
                following turn and are evicted again after several idle \
                turns."
                .to_owned(),
            parameters: crate::schema::parameters::<LoadInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call(
        &self,
        input: Value,
        _ctx: &ToolContext,
    ) -> futures::future::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        let catalog = Arc::clone(&self.catalog);
        let surface = Arc::clone(&self.surface);
        Box::pin(async move {
            let input: LoadInput = serde_json::from_value(input)
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            if input.tools.is_empty() {
                return Err(ToolError::InvalidInput(
                    "tools must name at least one deferred tool".to_owned(),
                ));
            }
            let mut unknown = Vec::new();
            for name in &input.tools {
                if catalog.find(name).is_none() {
                    unknown.push(name.as_str());
                }
            }
            if !unknown.is_empty() {
                let known: Vec<&str> = catalog
                    .entries
                    .iter()
                    .map(|entry| entry.model_name.as_str())
                    .take(8)
                    .collect();
                return Err(ToolError::InvalidInput(format!(
                    "unknown tool name(s): {}. Deferred tools include: {}. Use \
                     mcp_search_tools to list them.",
                    unknown.join(", "),
                    known.join(", ")
                )));
            }
            let mut loaded = Vec::new();
            let mut skipped = Vec::new();
            for name in &input.tools {
                let Some(entry) = catalog.find(name) else {
                    continue;
                };
                let tool: Arc<dyn Tool> = entry.tool.clone();
                match surface.queue(tool) {
                    Ok(()) => loaded.push(name.as_str()),
                    Err(_) => skipped.push(name.as_str()),
                }
            }
            let mut out = format!(
                "Loaded {} tool(s) onto the session surface; they are callable \
                 from the next turn: {}.",
                loaded.len(),
                loaded.join(", ")
            );
            if !skipped.is_empty() {
                let _ = write!(out, " Already loaded (skipped): {}.", skipped.join(", "));
            }
            Ok(ToolOutput::new(out))
        })
    }
}
