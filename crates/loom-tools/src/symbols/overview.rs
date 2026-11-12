use crate::budget::ToolBudgets;
use futures::future::BoxFuture;
use loom_kernel::tool::{PathTouch, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::OnceLock;

use super::{FlatSymbol, load_symbols};

// ---------------------------------------------------------------------------
// symbols_overview
// ---------------------------------------------------------------------------

/// Lists the symbols a file declares, with name-paths and line ranges —
/// the one-call replacement for the search→read wander.
/// Tree-sitter-powered symbol survey: a bounded, indexed outline of one
/// file's definitions (roadmap item 18).
#[derive(Debug, Default)]
pub struct SymbolsOverviewTool {
    /// Output budgets (item 20.2): the overview byte cap before the
    /// kind-count fallback. `Default` keeps the constants.
    budgets: ToolBudgets,
}

impl SymbolsOverviewTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// entry point).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
struct OverviewInput {
    /// Workspace-relative path of the file to survey.
    path: String,
    /// How many nesting levels below each top-level symbol to list
    /// (0–3; default 0 — top-level only, children counted).
    #[serde(default)]
    depth: Option<u32>,
}

impl Tool for SymbolsOverviewTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "symbols_overview".to_owned(),
            description: "Lists the symbols (functions, classes, structs, \
                          impls, interfaces, …) a Rust, Python, or \
                          TypeScript file declares, as name-paths with line \
                          ranges. One call replaces the search→read chain of \
                          finding what lives in a file. Use the name-path \
                          (e.g. `Class/method`) with `replace_symbol`."
                .to_owned(),
            parameters: crate::schema::parameters::<OverviewInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        Some(
            match serde_json::from_value::<OverviewInput>(input.clone()) {
                Ok(parsed) => vec![PathTouch::read(parsed.path)],
                Err(_) => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "symbols_overview serves .rs, .py, .ts/.mts/.cts, and .tsx \
                 files with a workspace-relative `path`; call it on one file \
                 at a time."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: OverviewInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let depth = input.depth.unwrap_or(0).min(3);
            let root = ctx.workspace_root.clone();
            let display = input.path.clone();
            let (source, symbols, relative) =
                tokio::task::spawn_blocking(move || load_symbols(&root, &display))
                    .await
                    .map_err(|err| {
                        ToolError::Execution(format!("overview task failed: {err}"))
                    })??;

            let visible: Vec<&FlatSymbol> = symbols
                .iter()
                .filter(|symbol| symbol.depth <= depth as usize)
                .collect();
            let content =
                match render_overview(&relative, &visible, depth, symbols.len(), &self.budgets) {
                    Some(rendered) => rendered,
                    None => {
                        // Over budget: fall back to kind counts (Serena's
                        // progressive shortening) instead of truncating the
                        // listing mid-symbol.
                        render_kind_counts(&relative, &symbols)
                    }
                };
            let _ = source;
            Ok(ToolOutput::new(content))
        })
    }
}

/// Renders the visible symbols; `None` when the rendering would exceed
/// the byte budget (caller falls back to kind counts).
fn render_overview(
    relative: &str,
    visible: &[&FlatSymbol],
    depth: u32,
    total: usize,
    budgets: &ToolBudgets,
) -> Option<String> {
    let mut out = format!("{relative} — {total} symbols (depth {depth})\n");
    for symbol in visible {
        let line = format!(
            "{indent}{} {} (L{}-L{}){}\n",
            symbol.kind,
            symbol.path,
            symbol.start_line,
            symbol.end_line,
            if symbol.child_count > 0 && symbol.depth == depth as usize {
                let plural = if symbol.child_count == 1 { "" } else { "ren" };
                format!(" [{} child{plural} — raise depth]", symbol.child_count)
            } else {
                String::new()
            },
            indent = "  ".repeat(symbol.depth + 1),
        );
        if out.len() + line.len() > budgets.symbols_overview_max_bytes {
            return None;
        }
        out.push_str(&line);
    }
    Some(out)
}

/// Kind-count fallback for files whose full listing overflows the
/// budget: names are dropped, buckets remain.
fn render_kind_counts(relative: &str, symbols: &[FlatSymbol]) -> String {
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for symbol in symbols {
        *counts.entry(symbol.kind).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(kind, count)| format!("{kind}s: {count}"))
        .collect();
    format!(
        "{relative} — too many symbols to list; by kind: {}. \
         Narrow with `depth: 0` or edit by name-path with `replace_symbol`.",
        parts.join(", ")
    )
}
