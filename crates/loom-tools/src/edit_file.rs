//! The `edit_file` tool: exact-match replacement in workspace files.
//!
//! Mutating (architecture §10). Each match must be unique: an ambiguous
//! edit is refused as invalid input rather than guessing which
//! occurrence the model meant. A call is either the classic single edit
//! (`old_string`/`new_string` against `path`) or an atomic batch
//! (`edits: [{old_string, new_string, path?}]`, applied in order — the
//! optional per-edit `path` defaults to the top-level one). Batch edits
//! apply against the evolving text, and nothing is written unless every
//! edit matches exactly once. The result echoes each edited region
//! (±[`ToolBudgets::edit_echo_context_lines`] numbered lines, final file
//! state), so the model never re-reads to confirm what it just wrote.

use futures::future::BoxFuture;
use loom_kernel::tool::{
    PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::budget::{self, ToolBudgets};
use crate::confinement::{resolve_within, workspace_relative};

/// Edits larger than this are refused: the whole file is read into memory and
/// rewritten, so an unbounded read is a memory spike (a large generated file
/// is the realistic case). 8 MiB covers source files with room to spare.
const MAX_EDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;

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
}

impl EditFileTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// entry point).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct EditInput {
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
/// would escape the workspace fails as a whole, before any file is read.
fn resolve_edits(
    root: &Path,
    batch: Vec<(String, String, String)>,
) -> Result<Vec<(PathBuf, String, String)>, ToolError> {
    batch
        .into_iter()
        .map(|(display, old, new)| Ok((resolve_within(root, &display)?, old, new)))
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
            let outcome = tokio::task::spawn_blocking(move || apply_edits(&root, &edits))
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
                let (rendered, command) =
                    crate::fusion::run_fused(scope, ctx, &verify_target).await?;
                content = format!("{content}\n\n--- verify ---\n{}", rendered.content);
                truncated = rendered.truncated;
                notices = rendered.notices;
                if let Some(command) = command {
                    effects = effects.with_command(command);
                }
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

/// Everything the caller needs after a successful batch: the success
/// line, the final text of each distinct file, and where each
/// replacement landed (file index + final byte span) for the echo.
struct EditOutcome {
    detail: String,
    /// `(workspace_relative, final_text)` per distinct file.
    files: Vec<(String, String)>,
    /// `(file index, start, len)` per edit, in application order.
    spans: Vec<(usize, usize, usize)>,
    /// Repair notes for calls Loom salvaged (e.g. a whitespace-padded
    /// `old_string` that only matched after trimming) — surfaced in the
    /// result and recorded in the effects for the session stats.
    repairs: Vec<String>,
}

/// Loads every distinct target file once, enforcing the size cap and the
/// UTF-8 requirement up front.
///
/// Unlike the read tools, a missing file here is an execution error, not an
/// informational "does not exist": a write path that targets nothing is a
/// model mistake (read first, or use write_file), and the loop's failure
/// ledger should refuse a blind repeat (ADR-020).
fn load_edit_files(
    edits: &[(PathBuf, String, String)],
) -> Result<Vec<(PathBuf, String, String)>, ToolError> {
    let mut files: Vec<(PathBuf, String, String)> = Vec::new();
    for (path, _, _) in edits {
        if files.iter().any(|(p, _, _)| p == path) {
            continue;
        }
        let size = std::fs::metadata(path)
            .map_err(|err| {
                ToolError::Execution(format!("cannot read \"{}\": {err}", path.display()))
            })?
            .len();
        if size > MAX_EDIT_FILE_BYTES {
            return Err(ToolError::Execution(format!(
                "file is too large to edit ({size} bytes; limit {MAX_EDIT_FILE_BYTES} bytes)"
            )));
        }
        let bytes = std::fs::read(path).map_err(|err| {
            ToolError::Execution(format!("cannot read \"{}\": {err}", path.display()))
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::Execution(String::from("file is not valid UTF-8")))?;
        // The untouched text rides along so the syntax guard below can
        // tell a genuine breakage from a file that already failed to
        // parse before this edit.
        files.push((path.clone(), text.clone(), text));
    }
    Ok(files)
}

/// Replaces `old_len` bytes at `start` with `new` in `text`, shifting the
/// file's already-recorded spans that sit after the replacement so later
/// edits on the same file move earlier spans instead of orphaning them,
/// and records the replacement's own span.
///
/// A replaced range can never overlap a recorded span: each consumed a
/// distinct unique match.
fn replace_span(
    text: &mut String,
    file_spans: &mut Vec<(usize, usize)>,
    start: usize,
    old_len: usize,
    new: &str,
) {
    text.replace_range(start..start + old_len, new);
    let delta = new.len() as isize - old_len as isize;
    for (recorded_start, _) in file_spans.iter_mut() {
        if start < *recorded_start {
            *recorded_start = (*recorded_start as isize + delta) as usize;
        }
    }
    file_spans.push((start, new.len()));
}

/// The repair probe (roadmap item 6): models pad `old_string` with
/// incidental whitespace far more often than they genuinely misremember
/// the text. Returns the byte offset when exactly one trimmed copy
/// matches (and the trim removed something); anything else stays `None`
/// so the caller keeps the original error.
fn trimmed_match(text: &str, old: &str) -> Option<usize> {
    let trimmed = old.trim();
    if trimmed.is_empty() || text.matches(trimmed).count() != 1 {
        return None;
    }
    text.find(trimmed)
}

/// The near-miss probe (roadmap item 6's second half): an anchor that
/// matches nothing usually means the model remembered the shape of the
/// line but not its current text. Points at where that first line still
/// occurs — or, failing that, at the closest current line — so the next
/// attempt re-reads instead of guessing again. `None` when nothing is
/// close enough, so the caller keeps its plain error.
fn near_miss_hint(text: &str, old: &str) -> Option<String> {
    let probe = old.lines().map(str::trim).find(|line| !line.is_empty())?;
    let wanted = probe.split_whitespace().collect::<Vec<_>>().join(" ");
    if wanted.is_empty() {
        return None;
    }
    let mut occurrences = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.contains(&wanted) {
            occurrences.push(index + 1);
            if occurrences.len() == 3 {
                break;
            }
        }
    }
    if !occurrences.is_empty() {
        let lines = occurrences
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            " (its first line still occurs at line {lines}; re-read the file and copy its current text)"
        ));
    }
    let probe_len = probe.chars().count();
    let mut closest: Option<(usize, usize, &str)> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let common = probe
            .chars()
            .zip(trimmed.chars())
            .take_while(|(anchor, current)| anchor == current)
            .count();
        let shorter = probe_len.min(trimmed.chars().count());
        if common < 8 || common * 2 < shorter {
            continue;
        }
        let better = match closest {
            None => true,
            Some((best, _, _)) => common > best,
        };
        if better {
            closest = Some((common, index + 1, trimmed));
        }
    }
    closest.map(|(_, line, current)| {
        format!(
            " (the nearest line is {line}: `{current}`; re-read the file and copy its current text)"
        )
    })
}

/// The `edited …` success line: one file gets the occurrence count,
/// several get the per-file rollup.
fn edit_detail(resolved: &[(String, String)], per_file_counts: &[usize]) -> String {
    if resolved.len() == 1 {
        let (relative, _) = &resolved[0];
        format!(
            "{relative}: replaced {} occurrence{}",
            per_file_counts[0],
            if per_file_counts[0] == 1 { "" } else { "s" }
        )
    } else {
        let summary = resolved
            .iter()
            .zip(per_file_counts)
            .map(|((relative, _), count)| format!("{relative} ×{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} edits across {} files ({summary})",
            per_file_counts.iter().sum::<usize>(),
            resolved.len()
        )
    }
}

/// Reads every target file once, applies the edits in order against the
/// evolving texts, and writes all files back only when every edit
/// matched exactly once. `root` is the workspace root, for relative
/// paths in the effects.
fn apply_edits(root: &Path, edits: &[(PathBuf, String, String)]) -> Result<EditOutcome, ToolError> {
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
    // Every edit matched exactly once: write everything back.
    for (path, _, text) in &files {
        std::fs::write(path, text)
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
    })
}

/// Renders the edited regions from the final file state: one numbered
/// snippet per edit, ±`edit_echo_context_lines` lines, capped by the echo
/// byte budget. Returns the text and whether the budget cut anything.
fn render_echo(outcome: &EditOutcome, budgets: &ToolBudgets) -> (String, bool) {
    let mut joined = String::new();
    for (file_idx, start, len) in &outcome.spans {
        let (relative, text) = &outcome.files[*file_idx];
        let total_lines = text.lines().count().max(1);
        let line_of =
            |byte: usize| -> usize { 1 + text[..byte.min(text.len())].matches('\n').count() };
        let first_line = line_of(*start);
        let mut last_line = line_of(start + len);
        if *len > 0 && text[..(start + len).min(text.len())].ends_with('\n') {
            last_line -= 1;
        }
        let last_line = last_line.max(first_line);
        let ctx_first = first_line
            .saturating_sub(budgets.edit_echo_context_lines)
            .max(1);
        let ctx_last = (last_line + budgets.edit_echo_context_lines).min(total_lines);
        joined.push_str(&format!(
            "{relative}: lines {first_line}-{last_line} of {total_lines}\n"
        ));
        for (number, line) in text.lines().enumerate().take(ctx_last).skip(ctx_first - 1) {
            joined.push_str(&format!("{}\t{line}\n", number + 1));
        }
    }
    if joined.is_empty() {
        return (String::new(), false);
    }
    let half = budgets.edit_echo_max_bytes / 2;
    budget::head_tail(&joined, half, half)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn ctx(root: &Path) -> ToolContext {
        ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
    }

    async fn edit(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        EditFileTool::default().call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn replaces_unique_occurrence() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let output = edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn add() {}", "new_string": "fn add() { 1 }"}),
        )
        .await
        .expect("ok");
        assert!(
            output
                .content
                .starts_with("edited f.rs: replaced 1 occurrence\n\n"),
            "{}",
            output.content
        );
        // The echo shows the edited region with numbered lines, so the
        // model never re-reads to confirm.
        assert!(
            output
                .content
                .contains("f.rs: lines 1-1 of 1\n1\tfn add() { 1 }"),
            "{}",
            output.content
        );
        assert_eq!(output.effects.files_written, vec!["f.rs".to_string()]);
        assert!(output.effects.repairs.is_empty());
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add() { 1 }\n"
        );
    }

    #[tokio::test]
    async fn a_whitespace_padded_old_string_is_repaired() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        // The model padded the old string with a stray newline and spaces;
        // the file itself has none, so only a trimmed match can succeed.
        let output = edit(
            &root,
            json!({"path": "f.rs", "old_string": "\n  fn add() {}  ", "new_string": "fn add() { 1 }"}),
        )
        .await
        .expect("repaired edit applies");
        assert!(
            output.content.contains("replaced 1 occurrence"),
            "{}",
            output.content
        );
        assert!(
            output
                .content
                .contains("(repaired: old_string matched after trimming whitespace)"),
            "{}",
            output.content
        );
        assert_eq!(
            output.effects.repairs,
            vec!["old_string matched after trimming whitespace".to_string()]
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add() { 1 }\n"
        );
    }

    #[tokio::test]
    async fn a_trimmed_match_that_is_still_ambiguous_keeps_the_error() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn a() {}\nfn a() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": " fn a() {}", "new_string": "fn a() { 1 }"}),
        )
        .await
        .expect_err("two trimmed matches must not be repaired");
        match err {
            ToolError::InvalidInput(msg) => {
                assert!(msg.starts_with("old_string not found in file"), "{msg}");
                assert!(msg.contains("still occurs at line 1, 2"), "{msg}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn a() {}\nfn a() {}\n"
        );
    }

    #[tokio::test]
    async fn a_stale_anchor_reports_the_nearest_current_line() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn add() { 1 }", "new_string": "x"}),
        )
        .await
        .expect_err("stale anchor");
        match err {
            ToolError::InvalidInput(msg) => {
                assert!(msg.starts_with("old_string not found in file"), "{msg}");
                assert!(
                    msg.contains("the nearest line is 1: `fn add() {}`"),
                    "{msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add() {}\n"
        );
    }

    #[tokio::test]
    async fn batch_edits_apply_in_order_and_echo_each_region() {
        let (guard, root) = temp_root();
        std::fs::write(
            guard.path().join("f.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\n",
        )
        .expect("seed");
        let output = edit(
            &root,
            json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "fn one() {}", "new_string": "fn uno() {}"},
                    {"old_string": "fn three() {}", "new_string": "fn tres() {}"}
                ]
            }),
        )
        .await
        .expect("ok");
        assert!(
            output
                .content
                .starts_with("edited f.rs: replaced 2 occurrences"),
            "{}",
            output.content
        );
        // Both regions are echoed from the final state; in a 3-line file
        // each ±3-line window covers everything, so the headers carry the
        // edited span.
        assert!(
            output.content.contains(
                "f.rs: lines 1-1 of 3\n1\tfn uno() {}\n2\tfn two() {}\n3\tfn tres() {}\n"
            ),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("f.rs: lines 3-3 of 3\n"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn uno() {}\nfn two() {}\nfn tres() {}\n"
        );
    }

    #[tokio::test]
    async fn batch_edits_can_span_files() {
        // The fixtures are bare words in `.rs` files — not Rust items,
        // so the syntax guard would refuse them; disabled here only.
        let _disabled = crate::syntax::testing::disable_guard();
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("a.rs"), b"alpha\n").expect("seed a");
        std::fs::write(guard.path().join("b.rs"), b"beta\n").expect("seed b");
        let output = edit(
            &root,
            json!({
                "path": "a.rs",
                "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"path": "b.rs", "old_string": "beta", "new_string": "BETA"}
                ]
            }),
        )
        .await
        .expect("ok");
        assert!(
            output
                .content
                .starts_with("edited 2 edits across 2 files (a.rs ×1, b.rs ×1)"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("a.rs")).expect("read a"),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("b.rs")).expect("read b"),
            "BETA\n"
        );
        assert_eq!(output.effects.files_written.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_batch_edit_writes_nothing() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("a.rs"), b"alpha\n").expect("seed a");
        let err = edit(
            &root,
            json!({
                "path": "a.rs",
                "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "missing", "new_string": "x"}
                ]
            }),
        )
        .await
        .expect_err("second edit fails");
        assert!(
            matches!(&err, ToolError::InvalidInput(msg) if msg.contains("edit 2 of 2") && msg.contains("not found")),
            "{err:?}"
        );
        // Atomic: the first edit matched, but nothing reached the disk.
        assert_eq!(
            std::fs::read_to_string(guard.path().join("a.rs")).expect("read a"),
            "alpha\n"
        );
    }

    // env_lock serializes guard state; holding it across the awaited call is the point.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_edit_producing_broken_rust_is_refused() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn add() {}", "new_string": "fn add( {}"}),
        )
        .await
        .expect_err("guard refuses the unparsable candidate");
        assert!(
            matches!(&err, ToolError::InvalidInput(msg)
                if msg.contains("refusing to write \"f.rs\"") && msg.contains("syntax error at line")),
            "{err:?}"
        );
        // Refused before the write: the file is untouched.
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add() {}\n"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn an_edit_to_legacy_try_macro_code_is_allowed() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        // This grammar rejects every `try!(…)`, so before the fix the
        // guard refused any write here, at a line the model never
        // touched, locking it out of the whole file.
        std::fs::write(
            guard.path().join("f.rs"),
            b"fn a() {\n    let mmap = try!(self.mmap(&paths));\n}\n",
        )
        .expect("seed");
        edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn a()", "new_string": "fn b()"}),
        )
        .await
        .expect("a file the grammar cannot read must stay editable");
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn b() {\n    let mmap = try!(self.mmap(&paths));\n}\n"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_batch_where_any_edit_breaks_syntax_writes_nothing() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), "fn one() {}\nfn two() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "fn one() {}", "new_string": "fn uno() {}"},
                    {"old_string": "fn two() {}", "new_string": "fn two( {}"}
                ]
            }),
        )
        .await
        .expect_err("second edit's candidate fails the guard");
        assert!(
            matches!(&err, ToolError::InvalidInput(msg) if msg.contains("syntax error at line")),
            "{err:?}"
        );
        // Atomic: the first edit matched, but nothing reached the disk.
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn one() {}\nfn two() {}\n"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn non_rust_edits_bypass_the_syntax_guard() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("note.txt"), b"hello\n").expect("seed");
        let output = edit(
            &root,
            json!({"path": "note.txt", "old_string": "hello", "new_string": "(( not rust"}),
        )
        .await
        .expect("guard only knows .rs");
        assert!(
            output.content.starts_with("edited note.txt"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("note.txt")).expect("read"),
            "(( not rust\n"
        );
    }

    #[tokio::test]
    async fn a_disabled_guard_allows_a_broken_edit() {
        let _disabled = crate::syntax::testing::disable_guard();
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let output = edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn add() {}", "new_string": "fn add( {}"}),
        )
        .await
        .expect("disabled guard allows the write");
        assert!(
            output.content.starts_with("edited f.rs"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add( {}\n"
        );
    }

    #[tokio::test]
    async fn mixed_single_and_batch_forms_are_rejected() {
        let (_guard, root) = temp_root();
        let err = edit(
            &root,
            json!({
                "path": "f.rs",
                "old_string": "a",
                "new_string": "b",
                "edits": [{"old_string": "a", "new_string": "b"}]
            }),
        )
        .await
        .expect_err("mixed");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("not a mix")));
    }

    #[tokio::test]
    async fn empty_batch_is_rejected() {
        let (_guard, root) = temp_root();
        let err = edit(&root, json!({"path": "f.rs", "edits": []}))
            .await
            .expect_err("empty");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("at least one edit")));
    }

    #[tokio::test]
    async fn not_found_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "missing", "new_string": "x"}),
        )
        .await
        .expect_err("not found");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("not found")));
    }

    #[tokio::test]
    async fn ambiguous_match_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"a\na\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("ambiguous");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("2 times")));
    }

    #[tokio::test]
    async fn empty_old_string_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"a\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "", "new_string": "b"}),
        )
        .await
        .expect_err("empty");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("must not be empty")));
    }

    #[tokio::test]
    async fn escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = edit(
            &root,
            json!({"path": "../f.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[tokio::test]
    async fn missing_file_is_execution_error() {
        let (_guard, root) = temp_root();
        let err = edit(
            &root,
            json!({"path": "gone.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("missing");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("gone.rs")));
    }

    #[tokio::test]
    async fn oversized_file_is_execution_error() {
        let (guard, root) = temp_root();
        let path = guard.path().join("big.rs");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(MAX_EDIT_FILE_BYTES + 1).expect("set length");
        let err = edit(
            &root,
            json!({"path": "big.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("too large");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("too large")));
    }

    #[tokio::test]
    async fn invalid_utf8_is_execution_error() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("bin"), [0x61, 0xff]).expect("seed");
        let err = edit(
            &root,
            json!({"path": "bin", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("utf8");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("UTF-8")));
    }

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<EditInput>(
            json!({"path": "f.rs", "old_string": "a", "new_string": "b", "verify": "check"}),
        );
        crate::schema::agreement::check::<EditInput>(json!({
            "path": "f.rs",
            "edits": [{"old_string": "a", "new_string": "b"}, {"path": "g.rs", "old_string": "c", "new_string": "d"}]
        }));
    }

    #[tokio::test]
    async fn fused_check_fixes_and_verifies_in_one_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        std::fs::create_dir(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fusion-edit\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
        )
        .expect("Cargo.toml");
        std::fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 { 1  }\n").expect("seed");

        let ctx = ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(120),
        );
        let output = EditFileTool::default()
            .call(
                json!({
                    "path": "src/lib.rs",
                    "old_string": "{ 1  }",
                    "new_string": "{ 1 }",
                    "verify": "check"
                }),
                &ctx,
            )
            .await
            .expect("ok");
        assert!(
            output.content.starts_with("edited src/lib.rs"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("--- verify ---"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("exit status: 0"),
            "{}",
            output.content
        );
        assert_eq!(output.effects.files_written, vec!["src/lib.rs".to_string()]);
        assert_eq!(output.effects.commands_run.len(), 1);
    }
}
