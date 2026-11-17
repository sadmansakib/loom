//! The `replace_all` tool: one workspace-wide regex rewrite.
//!
//! `edit_file` refuses an `old_string` that occurs more than once, so a
//! rename or codemod spanning N files costs N calls and N results. This
//! is the one-call form — the same write pipeline as `edit_file` (parse
//! guard, rewind pre-images, atomic writes, all-or-nothing), the same
//! gitignore-aware walk as `search`, and per-file counts instead of an
//! echo, because the result is meant to be cheap.
//!
//! `touched_paths` is deliberately undeclared (the [`Tool`] default,
//! `None`): the target set is only known after the walk, so the
//! scheduler treats the call as a wildcard and serializes it against
//! the rest of the turn — correct for a workspace-wide mutation.

use crate::budget::{ToolBudgets, head_only};
use crate::confinement::{ensure_mutable, resolve_within};
use crate::policy::CommandPolicy;
use futures::future::BoxFuture;
use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::{
    BeforeImage, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

/// Cap on replacements in one call — the fast-path rename's budget, for
/// the same reason: a pattern that broad is a model mistake, not a
/// codemod.
const MAX_MATCHES: usize = 500;

/// One workspace-wide regex rewrite, bounded and all-or-nothing.
#[derive(Debug, Default)]
pub struct ReplaceAllTool {
    /// Output budgets (item 20.2): how much of the per-file summary
    /// survives before the standard omission marker.
    budgets: ToolBudgets,
    /// Whether a failing fused verify attempts one machine-applicable
    /// fix round (roadmap item 33); set from the engine's config.
    autofix: bool,
    /// Workspace command policy consulted by the fused-verify path.
    policy: CommandPolicy,
}

impl ReplaceAllTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// registration path).
    #[must_use]
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// Enables the fused-verify autofix round (roadmap item 33).
    #[must_use]
    pub fn with_autofix(mut self, autofix: bool) -> Self {
        self.autofix = autofix;
        self
    }

    /// Sets the workspace policy consulted by fused verify (item 2.6).
    #[must_use]
    pub fn with_policy(mut self, policy: CommandPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct ReplaceAllInput {
    /// Regular expression (Rust syntax) matched against whole file text.
    pattern: String,
    /// Replacement text; `$1` / `${name}` expand capture groups.
    replacement: String,
    /// Directory or file to rewrite; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Optional cargo run after the rewrite: check or test.
    #[serde(default)]
    verify: Option<crate::verify::Scope>,
}

impl Tool for ReplaceAllTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "replace_all".to_owned(),
            description: "Rewrites every match of a Rust-syntax regex across the \
                          workspace in one call; `edit_file` cannot, since it needs a \
                          unique `old_string`. `path` scopes the walk (default: \
                          workspace root); `verify` runs check/test after the write. \
                          Writes atomically under `edit_file`'s parse guard; refuses if \
                          any rewritten file would stop parsing or the matches exceed \
                          500. Returns per-file counts, not the text."
                .to_owned(),
            parameters: crate::schema::parameters::<ReplaceAllInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: ReplaceAllInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let regex = Regex::new(&input.pattern)
                .map_err(|err| ToolError::InvalidInput(format!("invalid regex: {err}")))?;
            let display = input.path.unwrap_or_else(|| ".".to_owned());
            let root = ctx.workspace_root.clone();
            let scope = resolve_within(&root, &display)?;
            ensure_mutable(&root, &scope)?;
            // A miss is an answer, not a fault (same contract as search).
            if !scope.exists() {
                return Ok(ToolOutput::new(format!(
                    "path \"{display}\" does not exist"
                )));
            }
            let rel_base = root.canonicalize().unwrap_or_else(|_| root.clone());
            let replacement = input.replacement;
            let artifacts = ctx.artifacts.clone();
            // Child token + drop guard: dropping this future (engine
            // timeout or cancellation) stops the blocking walk at its
            // next per-file check instead of chewing through the tree.
            let walk_token = ctx.cancellation.child_token();
            let _walk_guard = walk_token.clone().drop_guard();
            let task_root = root.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                rewrite_tree(
                    &task_root,
                    &rel_base,
                    &scope,
                    &regex,
                    &replacement,
                    artifacts.as_deref(),
                    &walk_token,
                )
            })
            .await
            .map_err(|err| ToolError::Execution(format!("replace task failed: {err}")))??;
            let scope = input.verify;
            // A no-match sweep wrote nothing, so there is nothing to
            // prove — skip the check rather than spend the turn on it.
            let wrote = outcome.occurrences > 0;
            let target = outcome
                .per_file
                .first()
                .map_or_default(|(path, _)| path.clone());
            let mut output = render(outcome, &self.budgets);
            if let Some(scope) = scope.filter(|_| wrote) {
                let (rendered, fused_effects) =
                    crate::fusion::run_fused(scope, ctx, &target, self.autofix, &self.policy)
                        .await?;
                output.content =
                    format!("{}\n\n--- verify ---\n{}", output.content, rendered.content);
                output.effects = output.effects.merge(fused_effects);
                output.notices = rendered.notices;
                if rendered.truncated {
                    output.truncated = true;
                }
            }
            Ok(output)
        })
    }
}

/// Everything a finished rewrite reports: per-file counts in walk order
/// (deterministic), the replacement total, how many files the edit
/// pipeline could not read, and the captured pre-images.
struct Rewrite {
    per_file: Vec<(String, usize)>,
    occurrences: usize,
    skipped: usize,
    before_images: Vec<BeforeImage>,
}

/// Walks the scope, rewrites every matching file in memory, then hands
/// the whole candidate set to the shared write pipeline. Nothing is
/// written until the cap, the parse guard, and the pre-images are all
/// settled.
fn rewrite_tree(
    root: &Path,
    rel_base: &Path,
    scope: &Path,
    regex: &Regex,
    replacement: &str,
    artifacts: Option<&dyn ArtifactStore>,
    cancellation: &CancellationToken,
) -> Result<Rewrite, ToolError> {
    let mut candidates: Vec<(PathBuf, String, String)> = Vec::new();
    let mut per_file: Vec<(String, usize)> = Vec::new();
    let mut occurrences = 0usize;
    let mut skipped = 0usize;
    let mut over_cap = false;
    crate::search::walk_files(scope, rel_base, cancellation, |path, rel| {
        // A file the edit pipeline cannot read (permissions, binary,
        // over the size cap) is skipped and counted, not fatal: one
        // unreadable file must not abort a whole-workspace rewrite.
        let Ok(text) = crate::edit_file::load_text(path) else {
            skipped += 1;
            return Ok(true);
        };
        let count = regex.find_iter(&text).count();
        if count == 0 {
            return Ok(true);
        }
        occurrences += count;
        if occurrences > MAX_MATCHES {
            over_cap = true;
            return Ok(false);
        }
        per_file.push((rel.display().to_string(), count));
        candidates.push((path.to_path_buf(), text, String::new()));
        Ok(true)
    })
    .map_err(ToolError::Execution)?;
    if over_cap {
        return Err(ToolError::InvalidInput(format!(
            "more than {MAX_MATCHES} matches; narrow `pattern` or `path`"
        )));
    }
    let mut before_images = Vec::new();
    if !candidates.is_empty() {
        // `replace_all` expands captures against the loaded text; the
        // candidate vector already holds it, so rewrite in place.
        for (_, text, new_text) in &mut candidates {
            *new_text = regex.replace_all(text, replacement).into_owned();
        }
        let (written, images) = crate::edit_file::write_touched(root, &candidates, artifacts)?;
        debug_assert_eq!(written.len(), per_file.len());
        before_images = images;
    }
    Ok(Rewrite {
        per_file,
        occurrences,
        skipped,
        before_images,
    })
}

/// Shapes the model-facing summary: counts only, bounded.
fn render(outcome: Rewrite, budgets: &ToolBudgets) -> ToolOutput {
    let mut effects = ToolEffects::none();
    for (path, _) in &outcome.per_file {
        effects = effects.with_file_written(path.clone());
    }
    for image in outcome.before_images {
        effects = effects.with_before_image(image);
    }
    if outcome.occurrences == 0 {
        let skipped = skipped_note(outcome.skipped);
        return ToolOutput::new(format!("no matches; nothing written{skipped}"))
            .with_effects(effects);
    }
    let mut text = format!(
        "rewrote {} occurrence(s) across {} file(s){}",
        outcome.occurrences,
        outcome.per_file.len(),
        skipped_note(outcome.skipped)
    );
    for (path, count) in &outcome.per_file {
        let _ = write!(text, "\n  {path}: {count}");
    }
    let (content, truncated) = head_only(&text, budgets.replace_all_summary_max_bytes);
    let output = ToolOutput::new(content).with_effects(effects);
    if truncated {
        output.with_truncated()
    } else {
        output
    }
}

/// The skip note, or nothing when every file was readable.
fn skipped_note(skipped: usize) -> String {
    if skipped == 0 {
        String::new()
    } else {
        format!("; {skipped} file(s) skipped (unreadable, not UTF-8, or over the edit size cap)")
    }
}

#[cfg(test)]
mod tests;
