//! The `edit_file` tool proper: input schema, confinement, the
//! atomic apply pipeline, and the [`Tool`] implementation.

use futures::future::BoxFuture;
use loom_kernel::tool::{
    BeforeImage, PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::budget::ToolBudgets;
use crate::confinement::{resolve_within, workspace_relative};
use crate::policy::CommandPolicy;

use super::echo::{edit_detail, render_echo};
use super::matching::{
    EditOutcome, load_edit_files, near_miss_hint, normalized_old, normalized_view, replace_span,
    trimmed_match,
};

/// One edit in a batch. The optional `path` defaults to the top-level
/// `path`, so a batch touching one file repeats only the strings.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct EditSpec {
    /// File to edit; defaults to the call's `path`.
    #[serde(default)]
    path: Option<String>,
    /// Text to replace; must occur exactly once here.
    old_string: String,
    /// Replacement text.
    new_string: String,
}

/// Replaces unique exact-string matches in existing UTF-8 files,
/// confined to the workspace root.
/// Applies precise string replacements and echoes the edited regions.
#[derive(Debug, Default)]
pub struct EditFileTool {
    /// Output budgets (item 20.2): the echo shape (context lines and
    /// byte cap) is this tool's "edit format" knob. `Default` keeps
    /// the documented constants.
    budgets: ToolBudgets,
    /// Whether a failing fused cargo verify attempts one
    /// machine-applicable fix round under a never-worse gate (roadmap
    /// item 33). Off by default; set from the engine's opt-in config.
    autofix: bool,
    /// Workspace command policy consulted by the fused-verify path
    /// (item 2.6); see [`WriteFileTool::policy`].
    policy: CommandPolicy,
}

impl EditFileTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// entry point).
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
pub(super) struct EditInput {
    /// Workspace-relative file path; the default target of every edit.
    path: String,
    /// Exact text to replace in `path`; must occur exactly once.
    #[serde(default)]
    old_string: Option<String>,
    /// Replacement text for `old_string`.
    #[serde(default)]
    new_string: Option<String>,
    /// Atomic batch: every edit matches or nothing is written; applied
    /// in order.
    #[serde(default)]
    edits: Option<Vec<EditSpec>>,
    /// Optional cargo run after the edit: check or test.
    #[serde(default)]
    verify: Option<crate::verify::Scope>,
}

impl EditInput {
    /// Splits the call into ordered `(display_path, old, new)` triples,
    /// rejecting the missing/mixed forms once, up front.
    fn batch(&self) -> Result<Vec<(String, String, String)>, ToolError> {
        let single_old = self.old_string.as_deref();
        let single_new = self.new_string.as_deref();
        let edits = self.edits.as_deref();
        match (single_old, single_new, edits) {
            (Some(old), Some(new), None) => {
                Ok(vec![(self.path.clone(), old.to_owned(), new.to_owned())])
            }
            (None, None, Some(edits)) => {
                if edits.is_empty() {
                    return Err(ToolError::InvalidInput(String::from(
                        "edits must contain at least one edit",
                    )));
                }
                Ok(edits
                    .iter()
                    .map(|edit| {
                        (
                            edit.path.clone().unwrap_or_else(|| self.path.clone()),
                            edit.old_string.clone(),
                            edit.new_string.clone(),
                        )
                    })
                    .collect())
            }
            _ => Err(ToolError::InvalidInput(String::from(
                "pass either old_string + new_string (single edit) or edits (batch), not a mix",
            ))),
        }
    }
}

/// Resolves every edit's display path against the workspace root.
/// Confinement is checked per edit before anything runs: a batch that
/// would escape the workspace — or write workspace control state
/// (`.loom`/`.git`/`.cargo`, [`crate::confinement::ensure_mutable`]) —
/// fails as a whole, before any file is read.
fn resolve_edits(
    root: &Path,
    batch: Vec<(String, String, String)>,
) -> Result<Vec<(PathBuf, String, String)>, ToolError> {
    batch
        .into_iter()
        .map(|(display, old, new)| {
            let path = resolve_within(root, &display)?;
            crate::confinement::ensure_mutable(root, &path)?;
            Ok((path, old, new))
        })
        .collect()
}

/// Turns an applied batch into its effects and the pre-verify output
/// body: the success line, the repair note when Loom salvaged calls,
/// and the echoed regions.
fn assemble_body(outcome: &EditOutcome, budgets: &ToolBudgets) -> (ToolEffects, String, bool) {
    let mut effects = ToolEffects::none();
    for (relative, _) in &outcome.files {
        effects = effects.with_file_written(relative.clone());
    }
    for note in &outcome.repairs {
        effects = effects.with_repair(note.clone());
    }
    for image in &outcome.before_images {
        effects = effects.with_before_image(image.clone());
    }
    let repair_note = if outcome.repairs.is_empty() {
        String::new()
    } else {
        format!("\n(repaired: {})", outcome.repairs.join("; "))
    };
    let (echo, truncated_echo) = render_echo(outcome, budgets);
    let mut content = format!("edited {}{}", outcome.detail, repair_note);
    if !echo.is_empty() {
        content.push_str("\n\n");
        content.push_str(&echo);
    }
    (effects, content, truncated_echo)
}

impl Tool for EditFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "edit_file".to_owned(),
            description: "Replaces exact strings in workspace files. `old_string` \
                          must occur exactly once — include enough context to make it \
                          unique. `edits: [{old_string, new_string, path?}]` applies an \
                          atomic batch. The result shows each edited region — no re-read \
                          needed."
                .to_owned(),
            parameters: crate::schema::parameters::<EditInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        Some(match serde_json::from_value::<EditInput>(input.clone()) {
            Ok(parsed) => match parsed.batch() {
                Ok(batch) => {
                    let mut touches: Vec<PathTouch> = Vec::new();
                    for (path, _, _) in batch {
                        if !touches.iter().any(|touch| touch.path == path) {
                            touches.push(PathTouch::write(path));
                        }
                    }
                    touches
                }
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        })
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: EditInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let batch = input.batch()?;
            let scope = input.verify;
            let edits = resolve_edits(&ctx.workspace_root, batch)?;
            let root = ctx.workspace_root.clone();
            let artifacts = ctx.artifacts.clone();
            let outcome =
                tokio::task::spawn_blocking(move || apply_edits(&root, &edits, artifacts))
                    .await
                    .map_err(|err| ToolError::Execution(format!("edit task failed: {err}")))??;
            let (mut effects, mut content, truncated_echo) = assemble_body(&outcome, &self.budgets);
            let mut truncated = false;
            // Trace-level observations from the fused verify (e.g. the
            // unconfined-runner notice) ride the host output.
            let mut notices = Vec::new();
            if let Some(scope) = scope {
                let verify_target = outcome
                    .files
                    .first()
                    .map(|(relative, _)| relative.clone())
                    .unwrap_or_default();
                let (rendered, fused_effects) = crate::fusion::run_fused(
                    scope,
                    ctx,
                    &verify_target,
                    self.autofix,
                    &self.policy,
                )
                .await?;
                content = format!("{content}\n\n--- verify ---\n{}", rendered.content);
                truncated = rendered.truncated;
                notices = rendered.notices;
                // The fused run's command (and any kept autofix writes,
                // pre-images, and repair notes) fold into this call's
                // effects (roadmap item 33).
                effects = effects.merge(fused_effects);
            }
            let mut output = ToolOutput::new(content).with_effects(effects);
            output.notices = notices;
            if truncated || truncated_echo {
                output = output.with_truncated();
            }
            Ok(output)
        })
    }
}
/// Reads every target file once, applies the edits in order against the
/// evolving texts, and writes all files back only when every edit
/// matched exactly once. `root` is the workspace root, for relative
/// paths in the effects. `artifacts` carries the tier-2 pre-image
/// capture (already-loaded originals, no extra IO).
fn apply_edits(
    root: &Path,
    edits: &[(PathBuf, String, String)],
    artifacts: Option<std::sync::Arc<dyn loom_kernel::store::ArtifactStore>>,
) -> Result<EditOutcome, ToolError> {
    let batch_len = edits.len();
    let prefix = |index: usize| -> String {
        if batch_len > 1 {
            format!("edit {} of {batch_len}: ", index + 1)
        } else {
            String::new()
        }
    };
    let mut files = load_edit_files(edits)?;
    let mut spans: Vec<Vec<(usize, usize)>> = vec![Vec::new(); files.len()];
    let mut repairs: Vec<String> = Vec::new();
    for (index, (path, old, new)) in edits.iter().enumerate() {
        let file_idx = match files.iter().position(|(p, _, _)| p == path) {
            Some(idx) => idx,
            None => {
                // Unreachable: `load_edit_files` inserted every edit's
                // path. Stated as an error rather than a panic so the
                // invariant is checkable, not assumed.
                return Err(ToolError::Execution(String::from(
                    "internal error: edit target was not loaded",
                )));
            }
        };
        let text = &mut files[file_idx].2;
        if old.is_empty() {
            return Err(ToolError::InvalidInput(format!(
                "{}old_string must not be empty",
                prefix(index)
            )));
        }
        match (text.matches(old).count(), text.find(old)) {
            (0, _) => {
                // Review item 3.1: try the CRLF/BOM-normalized view
                // before the whitespace repair — a byte-exact miss is
                // most often a line-ending mismatch, not a stale copy.
                let normalized = normalized_view(text);
                if !normalized.is_identity(text) {
                    let old_norm = normalized_old(old);
                    if let (1, Some(start)) = (
                        normalized.view.matches(&old_norm).count(),
                        normalized.view.find(&old_norm),
                    ) {
                        let (orig_start, orig_len) = normalized.map_span(start, old_norm.len());
                        let adapted = if normalized.all_crlf {
                            new.replace('\n', "\r\n")
                        } else {
                            new.clone()
                        };
                        replace_span(text, &mut spans[file_idx], orig_start, orig_len, &adapted);
                        repairs.push(format!(
                            "{}old_string matched after normalizing line endings (CRLF/BOM); the file keeps its own endings",
                            prefix(index)
                        ));
                        continue;
                    }
                }
                if let Some(start) = trimmed_match(text, old) {
                    replace_span(text, &mut spans[file_idx], start, old.trim().len(), new);
                    repairs.push(format!(
                        "{}old_string matched after trimming whitespace",
                        prefix(index)
                    ));
                } else {
                    let mut message = format!("{}old_string not found in file", prefix(index));
                    if let Some(hint) = near_miss_hint(text, old) {
                        message.push_str(&hint);
                    }
                    return Err(ToolError::InvalidInput(message));
                }
            }
            (1, Some(start)) => {
                replace_span(text, &mut spans[file_idx], start, old.len(), new);
            }
            (n, _) => {
                return Err(ToolError::InvalidInput(format!(
                    "{}old_string appears {n} times; include more surrounding text to make it unique",
                    prefix(index)
                )));
            }
        }
    }
    // Syntax guard (roadmap item 5 phase B): every candidate buffer
    // must parse before anything reaches the disk, so a batch that
    // would leave any file unparsable refuses as a whole — nothing is
    // written, exactly like a failed match.
    for (path, original, text) in &files {
        crate::syntax::guard_against(original, text, path).map_err(|err| {
            ToolError::InvalidInput(format!(
                "refusing to write \"{}\": {err}",
                workspace_relative(root, path)
            ))
        })?;
    }
    // Tier-2 rewind capture (roadmap gap 1): the loaded originals are
    // exactly the pre-image of every touched file — capture them before
    // the writes below, content-addressed into the artifact store.
    let before_images: Vec<BeforeImage> = files
        .iter()
        .filter_map(|(path, original, _)| {
            crate::pre_image::capture_before_image(
                artifacts.as_deref(),
                &workspace_relative(root, path),
                Some(original.as_bytes()),
            )
        })
        .collect();
    // Every edit matched exactly once: write everything back. Each
    // write re-verifies confinement at use time (review item 2.2) and
    // lands atomically (review item 2.3).
    for (path, _, text) in &files {
        crate::confinement::verify_parent_within(root, path)?;
        crate::atomic::atomic_write(path, text.as_bytes())
            .map_err(|err| ToolError::Execution(format!("cannot write: {err}")))?;
    }
    let resolved: Vec<(String, String)> = files
        .iter()
        .map(|(path, _, text)| (workspace_relative(root, path), text.clone()))
        .collect();
    let per_file_counts: Vec<usize> = spans.iter().map(Vec::len).collect();
    let mut echoes = Vec::new();
    for (file_idx, file_spans) in spans.iter().enumerate() {
        for (start, len) in file_spans {
            echoes.push((file_idx, *start, *len));
        }
    }
    Ok(EditOutcome {
        detail: edit_detail(&resolved, &per_file_counts),
        files: resolved,
        spans: echoes,
        repairs,
        before_images,
    })
}
