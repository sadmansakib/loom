//! The `search` tool: gitignore-aware regex line search.
//!
//! Ignore handling is delegated to the `ignore` crate, with ignore
//! files between the workspace root and the scope applied by hand so a
//! scoped search matches a whole-workspace search. On nested
//! negations the walk now matches git (a nested `!gen/` beats a root
//! `gen/` because no *parent directory* of the path is excluded;
//! git's real cannot-re-include rule bites only under an excluded
//! parent dir, where this walk prunes descent like git) — pinned by
//! `whole_tree_walk_matches_git_on_nested_negation` and
//! `repo_shaped_tree_scoped_and_whole_agree` (item 16). One deliberate
//! divergence remains and is documented rather than fixed: the scoped
//! walk consults only ignore files *down to the scope*, so a whitelist
//! in a file *inside* the scope cannot re-include what an ancestor
//! rule excludes — stricter than git's descent. See §9.2 below and the
//! `scoped_whitelist_below_scope_is_pruned` test for the concrete
//! case.

mod cues;
mod render;
mod walk;

#[cfg(test)]
use cues::TEST_CUES_DISABLED;
#[cfg(test)]
use render::{MAX_MATCH_LINE_BYTES, SUMMARY_THRESHOLD};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

use cues::symbol_cues_for;
use futures::future::BoxFuture;
use loom_kernel::tool::{PathTouch, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use regex::Regex;
use render::render;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use walk::search_tree;

pub(crate) use walk::walk_files;

use crate::confinement::resolve_within;

/// Default cap on reported matching lines.
const DEFAULT_MAX_RESULTS: usize = 200;
/// Per-file read cap: files larger than this are searched only up to
/// this many bytes, cut on a UTF-8 char boundary (keeps one giant
/// file from dominating a run).
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// One matching line, before rendering.
struct Match {
    /// Workspace-relative path.
    rel: String,
    /// 1-based line number.
    line: usize,
    /// The matched line (already length-capped).
    text: String,
}

/// Regex line search over the workspace tree: `.gitignore`-aware,
/// hidden files skipped, binary files skipped, cancellation checked
/// per file.
#[derive(Debug, Default)]
pub struct SearchTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct SearchInput {
    /// Regular expression (Rust syntax).
    pattern: String,
    /// Directory or file to search; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Cap on reported matching lines; minimum 1, default 200.
    #[serde(default)]
    #[schemars(range(min = 1))]
    max_results: Option<usize>,
}

impl Tool for SearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "search".to_owned(),
            description: "Searches file contents by regular expression across the \
                          workspace (gitignored, hidden, and binary files skipped). \
                          Returns `path:line: text`, with a `[in symbol]` cue for the \
                          enclosing declaration; large sets are summarized per file."
                .to_owned(),
            parameters: crate::schema::parameters::<SearchInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        Some(match serde_json::from_value::<SearchInput>(input.clone()) {
            Ok(parsed) => vec![PathTouch::read(
                parsed.path.unwrap_or_else(|| ".".to_owned()),
            )],
            Err(_) => Vec::new(),
        })
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: SearchInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let regex = Regex::new(&input.pattern)
                .map_err(|err| ToolError::InvalidInput(format!("invalid regex: {err}")))?;
            let max_results = match input.max_results {
                None => DEFAULT_MAX_RESULTS,
                Some(n) if n >= 1 => n,
                Some(_) => {
                    return Err(ToolError::InvalidInput(
                        "max_results must be at least 1".to_owned(),
                    ));
                }
            };
            let display = input.path.unwrap_or_else(|| ".".to_owned());
            let scope = resolve_within(&ctx.workspace_root, &display)?;
            // A miss is an answer, not a fault: the caller asked whether
            // anything matches there, and there is nothing to search (§13).
            if !scope.exists() {
                return Ok(ToolOutput::new(format!(
                    "path \"{display}\" does not exist"
                )));
            }
            // Reported paths are relative to the workspace root (not the
            // search scope) so the model can feed them straight back to
            // read_file.
            let rel_base = ctx
                .workspace_root
                .canonicalize()
                .unwrap_or_else(|_| ctx.workspace_root.clone());
            // Child token + drop guard: dropping this future (engine
            // timeout or cancellation) cancels the child, stopping the
            // blocking walk at its next per-file check instead of
            // letting it chew through the whole tree.
            let walk_token = ctx.cancellation.child_token();
            let _walk_guard = walk_token.clone().drop_guard();
            let entries = tokio::task::spawn_blocking(move || {
                let found = search_tree(&regex, &scope, max_results, &rel_base, &walk_token);
                // Cue building re-reads and tree-sitter-parses each hit
                // file: blocking work, so it stays on this worker
                // (maintenance bar) rather than the async path.
                let cues = match &found {
                    Ok((matches, _)) => symbol_cues_for(matches, &rel_base),
                    Err(_) => HashMap::new(),
                };
                found.map(|(matches, truncated)| (matches, truncated, cues))
            })
            .await
            .map_err(|err| ToolError::Execution(format!("search task failed: {err}")))?;

            match entries {
                Ok((matches, truncated, cues)) => {
                    let (content, truncated) = render(&matches, truncated, max_results, &cues);
                    let mut output = ToolOutput::new(content);
                    if truncated {
                        output = output.with_truncated();
                    }
                    Ok(output)
                }
                Err(msg) => Err(ToolError::Execution(msg)),
            }
        })
    }
}

/// Reads at most `MAX_FILE_BYTES + 1` bytes (the extra byte only
/// signals the cap); `None` for binary (NUL byte) or non-UTF-8
/// content, which search skips. Oversized files are cut on a UTF-8
/// char boundary and searched up to the cap.
fn read_capped_text(path: &Path) -> Option<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let mut bytes = Vec::new();
    let mut file = file.take(MAX_FILE_BYTES + 1);
    file.read_to_end(&mut bytes).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let cap = usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX);
    crate::text::decode_capped(bytes, cap)
}

#[cfg(test)]
mod tests;
