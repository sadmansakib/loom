use crate::budget::ToolBudgets;
use crate::confinement::{resolve_within, workspace_relative};
use futures::future::BoxFuture;
use loom_kernel::tool::{
    BeforeImage, PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;

use super::{
    FlatSymbol, Symbol, SymbolLanguage, name_path_matches, parse_symbols, read_symbol_source,
    replaced_head,
};

// ---------------------------------------------------------------------------
// replace_symbol
// ---------------------------------------------------------------------------

/// Rewrites one symbol's definition by name-path — the one-call
/// replacement for the read→edit chain when the change is "rewrite this
/// function".
#[derive(Debug, Default)]
pub struct ReplaceSymbolTool {
    /// Output budgets (item 20.2): the replaced-definition echo head.
    pub(crate) budgets: ToolBudgets,
}

impl ReplaceSymbolTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// registration path; item 20.2).
    #[must_use]
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct ReplaceInput {
    /// Workspace-relative path of the file holding the symbol.
    #[serde(default)]
    path: Option<String>,
    /// Name-path addressing the symbol (`Class/method`). Suffix match by
    /// default; a leading `/` anchors at the top level.
    #[serde(default)]
    name_path: Option<String>,
    /// The full replacement definition, including the signature and
    /// decorators where the language uses them. Must parse on its own.
    #[serde(default)]
    body: Option<String>,
    /// Batch: several symbols rewritten in order, stopping at the first
    /// failure (earlier changes stay).
    #[serde(default)]
    changes: Option<Vec<SymbolChange>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct SymbolChange {
    /// Workspace-relative path of the file holding the symbol.
    path: String,
    /// Name-path addressing the symbol.
    name_path: String,
    /// The full replacement definition.
    body: String,
}

impl ReplaceInput {
    /// Splits the call into ordered changes, rejecting the missing and
    /// mixed forms once, up front.
    fn batch(self) -> Result<Vec<SymbolChange>, ToolError> {
        match (self.path, self.name_path, self.body, self.changes) {
            (Some(path), Some(name_path), Some(body), None) => Ok(vec![SymbolChange {
                path,
                name_path,
                body,
            }]),
            (None, None, None, Some(changes)) => {
                if changes.is_empty() {
                    return Err(ToolError::InvalidInput(String::from(
                        "changes must contain at least one change",
                    )));
                }
                Ok(changes)
            }
            _ => Err(ToolError::InvalidInput(String::from(
                "pass either path + name_path + body (single symbol) or changes (batch), \
                 not a mix",
            ))),
        }
    }
}

impl Tool for ReplaceSymbolTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "replace_symbol".to_owned(),
            description: "Replaces one symbol's whole definition in a Rust, \
                          Python, or TypeScript file, addressed by name-path \
                          (e.g. `Class/method`, suffix-matched; leading `/` \
                          anchors at top level). `body` is the complete new \
                          definition including its signature; `changes` does \
                          several in order, stopping at the first failure. One \
                          atomic write per file; the file must still parse. Find \
                          name-paths with `symbols_overview`."
                .to_owned(),
            parameters: crate::schema::parameters::<ReplaceInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        Some(
            match serde_json::from_value::<ReplaceInput>(input.clone()) {
                Ok(parsed) => {
                    let mut paths: Vec<String> = Vec::new();
                    if let Ok(changes) = parsed.batch() {
                        for change in changes {
                            if !paths.contains(&change.path) {
                                paths.push(change.path);
                            }
                        }
                    }
                    paths.into_iter().map(PathTouch::write).collect()
                }
                Err(_) => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "call `symbols_overview` on the file first; address the \
                 symbol by its listed name-path (`Class/method` — extend it \
                 if several symbols share the suffix), and pass the complete \
                 new definition including the signature as `body`."
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
            let input: ReplaceInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let batch = input.batch()?;
            let root = ctx.workspace_root.clone();
            let artifacts = ctx.artifacts.clone();
            let head_bytes = self.budgets.replace_symbol_head_bytes;
            // Applied one at a time, each reread from disk, so several
            // changes to one file compose. A single change keeps the
            // previous-definition echo; a batch reports one line each
            // instead, because the model wrote every body it sent.
            let mut applied: Vec<(String, String, usize, String)> = Vec::with_capacity(batch.len());
            let mut effects = ToolEffects::none();
            for change in batch {
                let root = root.clone();
                let artifacts = artifacts.clone();
                let result = tokio::task::spawn_blocking(move || {
                    replace_symbol(
                        &root,
                        &change.path,
                        &change.name_path,
                        &change.body,
                        artifacts.as_ref(),
                    )
                })
                .await
                .map_err(|err| ToolError::Execution(format!("replace task failed: {err}")))?;
                let (relative, new_text, replaced, matches, before_images) = match result {
                    Ok(value) => value,
                    // Stop at the first failure; say what already landed
                    // so the model does not redo it.
                    Err(err) if !applied.is_empty() => {
                        return Err(ToolError::Execution(format!(
                            "{err} ({} earlier change(s) were applied)",
                            applied.len()
                        )));
                    }
                    Err(err) => return Err(err),
                };
                effects = effects.with_file_written(relative.clone());
                for image in before_images {
                    effects = effects.with_before_image(image);
                }
                applied.push((relative, matches.display(), new_text.len(), replaced));
            }

            let content = if let [(relative, display, len, replaced)] = applied.as_slice() {
                format!(
                    "replaced {display} in {relative} ({len} byte body); previous \
                     definition started with:\n{}",
                    replaced_head(replaced, head_bytes),
                )
            } else {
                applied
                    .iter()
                    .map(|(relative, display, len, _)| {
                        format!("replaced {display} in {relative} ({len} byte body)")
                    })
                    .collect::<Vec<String>>()
                    .join("\n")
            };
            Ok(ToolOutput::new(content).with_effects(effects))
        })
    }
}

/// The whole replace pipeline, minus the write: resolves, parses,
/// matches, splices, and guards. Returns the workspace-relative path,
/// the candidate text, the replaced original text, the matched
/// symbol's display, and the tier-2 pre-image of the touched file.
fn replace_symbol(
    root: &Path,
    display_path: &str,
    name_path: &str,
    body: &str,
    artifacts: Option<&std::sync::Arc<dyn loom_kernel::store::ArtifactStore>>,
) -> Result<(String, String, String, Symbol, Vec<BeforeImage>), ToolError> {
    let path = resolve_within(root, display_path)?;
    // Symbol splicing mutates the file, so control state is off-limits
    // like any other write.
    crate::confinement::ensure_mutable(root, &path)?;
    let relative = workspace_relative(root, &path);
    let lang = SymbolLanguage::for_path(&path).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "symbol tools serve .rs, .py, and .ts/.tsx files; \
             \"{relative}\" is not one"
        ))
    })?;
    let source = read_symbol_source(&path, &relative)?;
    let (_, flat) = parse_symbols(lang, &source).map_err(ToolError::Execution)?;

    let matches: Vec<&FlatSymbol> = flat
        .iter()
        .filter(|symbol| name_path_matches(name_path, &symbol.path))
        .collect();
    let hit = match matches.as_slice() {
        [] => {
            let available: Vec<String> = flat
                .iter()
                .filter(|s| s.depth == 0)
                .map(|s| s.path.clone())
                .collect();
            return Err(ToolError::InvalidInput(format!(
                "no symbol matches name_path \"{name_path}\" in {relative}; \
                 top-level symbols: {}",
                summarize_paths(&available)
            )));
        }
        [one] => *one,
        many => {
            let listed: Vec<String> = many.iter().map(|s| s.display()).collect();
            return Err(ToolError::InvalidInput(format!(
                "name_path \"{name_path}\" matches {} symbols: {}; \
                 extend the path to disambiguate",
                many.len(),
                listed.join(", ")
            )));
        }
    };

    let replaced = source[hit.start_byte..hit.end_byte].to_owned();
    let mut new_text = String::with_capacity(source.len() - replaced.len() + body.len());
    new_text.push_str(&source[..hit.start_byte]);
    new_text.push_str(body);
    new_text.push_str(&source[hit.end_byte..]);

    // Same gate as edit_file: refuse a candidate that introduces a
    // parse error the original did not have.
    crate::syntax::guard_against(&source, &new_text, &path).map_err(|err| {
        ToolError::InvalidInput(format!(
            "refusing to write \"{relative}\": the replacement body does \
             not parse — {err}"
        ))
    })?;

    // Tier-2 rewind capture (roadmap gap 1): the loaded original is
    // exactly the pre-image of the touched file.
    let before_images = crate::pre_image::capture_before_image(
        artifacts.map(|arc| &**arc),
        &relative,
        Some(source.as_bytes()),
    )
    .into_iter()
    .collect();

    // Atomic replace (review item 2.3): Serena's two-write version
    // corrupted files when the process died between delete and
    // insert; the pre-atomic direct write truncated on crash. The
    // parent is re-verified confined first (review item 2.2).
    crate::confinement::verify_parent_within(root, &path)?;
    crate::atomic::atomic_write(&path, new_text.as_bytes())
        .map_err(|err| ToolError::Execution(format!("cannot write \"{relative}\": {err}")))?;
    Ok((
        relative,
        new_text,
        replaced,
        symbol_from_flat(hit),
        before_images,
    ))
}

/// Rebuilds a minimal `Symbol` for the result display.
fn symbol_from_flat(flat: &FlatSymbol) -> Symbol {
    Symbol {
        kind: flat.kind,
        path: flat.path.clone(),
        start_byte: flat.start_byte,
        end_byte: flat.end_byte,
        start_line: flat.start_line,
        end_line: flat.end_line,
        children: Vec::new(),
    }
}

/// Compact listing of available paths for a no-match error.
fn summarize_paths(paths: &[String]) -> String {
    const SHOWN: usize = 24;
    if paths.len() <= SHOWN {
        paths.join(", ")
    } else {
        format!("{} … ({} total)", paths[..SHOWN].join(", "), paths.len())
    }
}
