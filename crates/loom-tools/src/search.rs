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

use futures::future::BoxFuture;
use ignore::Match as IgnoreMatch;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use loom_kernel::tool::{PathTouch, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use crate::confinement::resolve_within;

/// Default cap on reported matching lines.
const DEFAULT_MAX_RESULTS: usize = 200;
/// Per-file read cap: files larger than this are searched only up to
/// this many bytes, cut on a UTF-8 char boundary (keeps one giant
/// file from dominating a run).
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Cap on each reported matching line: a file that is one huge line
/// (minified JS, lockfiles) would otherwise buffer up to
/// [`MAX_FILE_BYTES`] per match — 200 matches ≈ 200 MiB — before the
/// engine's 64 KiB output bound ever sees it.
const MAX_MATCH_LINE_BYTES: usize = 256;
/// Above this many matches, results are summarized per file (count plus
/// a preview) instead of listing every matching line — the tool result is
/// the dominant input-token cost (ADR-014, V8).
///
/// Kept high: the default `max_results` cap already bounds a raw listing
/// (200 matches at ≤256 bytes each). Summarizing sooner hides hits, which
/// pushed the model into extra refine/re-search turns on B1 — the summary
/// should only replace a listing too large to present, not a routine one.
const SUMMARY_THRESHOLD: usize = 100;
/// How many matching lines to show per file in the summarized form.
const PER_FILE_PREVIEW: usize = 5;

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
                          Returns `path:line: text`; large sets are summarized per file \
                          with a preview and a hint to narrow the search."
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
                search_tree(&regex, &scope, max_results, &rel_base, &walk_token)
            })
            .await
            .map_err(|err| ToolError::Execution(format!("search task failed: {err}")))?;

            match entries {
                Ok((matches, truncated)) => {
                    let (content, truncated) = render(&matches, truncated, max_results);
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

/// Walks `scope` and collects matching lines. Entries are sorted by
/// name within each directory, so the walk order — and therefore
/// which matches survive `max_results` truncation — is the same on
/// every machine (§8.1.3 determinism). `.gitignore` files between
/// the workspace root and `scope` apply, so a scoped search matches
/// a whole-workspace search. Matched lines longer than
/// [`MAX_MATCH_LINE_BYTES`] are truncated.
fn search_tree(
    regex: &Regex,
    scope: &Path,
    max_results: usize,
    rel_base: &Path,
    cancellation: &CancellationToken,
) -> Result<(Vec<Match>, bool), String> {
    // Ancestor ignore files, shallowest (workspace root) first, so the
    // nearest file wins like git. Only ignore files INSIDE the workspace
    // participate; global ignores and parent lookups above the root
    // would make results machine-dependent. `.parents(false)` keeps the
    // crate from auto-loading this chain itself (it would root them at
    // the CWD and mis-anchor path-style rules), so it is loaded by hand
    // and each file is compiled against its own directory (§9.2).
    let mut ancestors = Vec::new();
    let mut dir = scope.parent();
    while let Some(d) = dir {
        if d.starts_with(rel_base) {
            ancestors.push(d.join(".gitignore"));
        }
        if d == rel_base {
            break;
        }
        dir = d.parent();
    }
    // Collected deepest-first (walking up from the scope) — the chain
    // order `build_ancestor_matchers` preserves: the DEEPEST ancestor
    // file is consulted first, so a nearer `!gen/` beats a root `gen/`
    // exactly like git's nearest-file-wins. Before this chain existed
    // as a per-file pass, a root `gen/` rule overrode a nearer `!gen/`
    // and scoped searches silently dropped files git — and a
    // whole-tree search — keeps.
    let ancestor_matchers = build_ancestor_matchers(&ancestors)?;

    // Discovery determinism: only ignore files INSIDE the workspace
    // participate. `.parents(false)` (the default) prevents the crate
    // from climbing past the root and loading ignore files above it;
    // with `.parents(true)` the crate roots ancestor files at the CWD,
    // which would make results machine-dependent. The crate still
    // auto-loads ignore files *below* the scope — for the whole-tree
    // walk these are rooted at their own directories and matched
    // relative to them, which is correct git behavior. `.ignore`,
    // `.git/info/exclude`, and `hidden` keep their default behavior.
    let mut builder = ignore::WalkBuilder::new(scope);
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b));
    if !ancestor_matchers.is_empty() {
        // Each matcher strips its own root from the absolute entry
        // path, so a root `src/gen/` rule anchors at the workspace root
        // and a nested `/gen/` rule anchors at its own directory — the
        // same anchoring the whole-tree walk uses. The directory (not
        // its contents) is what the filter sees; filtering the entry
        // prunes the whole subtree before descent. Pinned by the item-16
        // tests (`nested_path_styled_rule_anchors_at_its_own_directory`,
        // `repo_shaped_tree_scoped_and_whole_agree`).
        let matchers = ancestor_matchers.clone();
        builder.filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
            // Deepest ancestor with an opinion wins; a shallower file
            // cannot overrule it, mirroring the walker's own
            // per-directory precedence (and git).
            //
            // `matched_path_or_any_parents` (not `matched`): a dir-only
            // rule like `/gen/` never matches a bare file entry, so
            // `matched` would leak `gen/out.rs` in a scoped walk — the
            // whole-tree walk only prunes it because it hits the `gen`
            // directory entry first. Matching the entry *and* each parent
            // (as a directory) replicates the walker's prune-at-dir
            // semantics after the fact for scoped roots whose ignored
            // ancestors sit at or above the walk root.
            let verdict = matchers
                .iter()
                .map(|matcher| matcher.matched_path_or_any_parents(entry.path(), is_dir))
                .find(|verdict| !verdict.is_none());
            !matches!(verdict, Some(IgnoreMatch::Ignore(_)))
        });
    }
    let walker = builder.build();

    let mut hits: Vec<Match> = Vec::new();
    let mut truncated = false;
    for entry in walker {
        if cancellation.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let entry = match entry {
            Ok(entry) => entry,
            // Unreadable entries (permission denials, racing deletes)
            // are skipped, not fatal: search reports what it can see.
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ty| ty.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(text) = read_capped_text(path) {
            let rel = path.strip_prefix(rel_base).unwrap_or(path);
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    if hits.len() == max_results {
                        truncated = true;
                        return Ok((hits, truncated));
                    }
                    hits.push(Match {
                        rel: rel.display().to_string(),
                        line: index + 1,
                        text: capped_line(line),
                    });
                }
            }
        }
    }
    Ok((hits, truncated))
}

/// Renders matches: `path:line: text` lines when the set is small, or a
/// per-file summary (count plus a short preview each) above
/// [`SUMMARY_THRESHOLD`]. A search stopped at `max_results` carries an
/// explicit marker rather than silently dropping the remainder (V9).
fn render(matches: &[Match], truncated: bool, max_results: usize) -> (String, bool) {
    if matches.is_empty() {
        return (String::new(), false);
    }
    if matches.len() <= SUMMARY_THRESHOLD {
        let mut text = matches
            .iter()
            .map(|m| format!("{}:{}: {}", m.rel, m.line, m.text))
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            text.push('\n');
            text.push_str(&cap_hint(max_results));
        }
        return (text, truncated);
    }
    // Over the threshold: one block per file, in first-seen (walk) order,
    // so the file that matched first is the one the model reads first.
    let mut files: Vec<(&str, Vec<&Match>)> = Vec::new();
    for found in matches {
        match files.iter_mut().find(|(name, _)| *name == found.rel) {
            Some((_, group)) => group.push(found),
            None => files.push((found.rel.as_str(), vec![found])),
        }
    }
    let mut lines = vec![format!(
        "{} matches in {} {}{}",
        matches.len(),
        files.len(),
        if files.len() == 1 { "file" } else { "files" },
        if truncated {
            format!(" (stopped at the {max_results}-match cap)")
        } else {
            String::new()
        }
    )];
    let mut shown = 0usize;
    for (name, group) in &files {
        lines.push(format!("{name} ({} matches)", group.len()));
        for found in group.iter().take(PER_FILE_PREVIEW) {
            lines.push(format!("  {}: {}", found.line, found.text));
            shown += 1;
        }
    }
    let hidden = matches.len() - shown;
    if hidden > 0 {
        lines.push(format!(
            "... [{hidden} more matches; refine the pattern or pass path to narrow the search]"
        ));
    }
    (lines.join("\n"), hidden > 0 || truncated)
}

/// The marker appended when a search stops at its `max_results` cap.
fn cap_hint(max_results: usize) -> String {
    format!(
        "... [stopped at the {max_results}-match cap; refine the pattern or pass path to narrow the search]"
    )
}

/// Truncates a matched line to [`MAX_MATCH_LINE_BYTES`] on a UTF-8
/// char boundary, marking the cut with an ellipsis. Short lines are
/// returned verbatim.
fn capped_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_owned();
    }
    let mut cut = MAX_MATCH_LINE_BYTES;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &line[..cut])
}

/// Builds one matcher per ancestor ignore file, deepest first, each
/// compiled against its own directory — the workspace root's file
/// against the workspace root, a nested file against its own parent
/// directory — exactly where git anchors them. A single merged matcher
/// (item 16) anchored everything at the workspace root, so a path-style
/// rule in a *nested* ancestor file (`src/.gitignore` saying `/gen/`)
/// matched `<root>/gen` instead of `src/gen`: a scoped search inside
/// `src/gen` then reported files git — and a whole-tree search —
/// ignore. Per-file rooting keeps scoped and whole-tree results
/// consistent regardless of the CWD or the scope's depth.
///
/// One deliberate divergence remains (§9.2): the filter only consults
/// files between the workspace root and the scope. A whitelist in an
/// ignore file *inside* the scope (say `src/.gitignore` `!keep.log`
/// under a root `*.log`, scope `src`) cannot rescue the file the way
/// git's descent would — the ancestor filter prunes it first. The
/// scoped walk is therefore stricter than git about negations; pinned
/// by `scoped_whitelist_below_scope_is_pruned` (item 16).
///
/// Empty when no ancestor ignore file exists.
fn build_ancestor_matchers(ancestors: &[std::path::PathBuf]) -> Result<Vec<Gitignore>, String> {
    let mut matchers = Vec::new();
    for file in ancestors {
        if !file.is_file() {
            continue;
        }
        // Anchor matchers at ABSOLUTE roots: entry paths from the walker
        // are absolute, and `matched()` strips each matcher's own root as
        // a common prefix. A relative root (e.g. `src` for scope
        // `src/gen`) leaves that prefix unaligned, so anchored `/gen/`
        // rules silently never match in scoped searches (§9.2).
        let abs = file
            .canonicalize()
            .map_err(|_| format!("cannot resolve {}", file.display()))?;
        let root = abs.parent().unwrap_or_else(|| Path::new("."));
        let mut builder = GitignoreBuilder::new(root);
        if let Some(err) = builder.add(&abs) {
            return Err(format!("cannot read ignore file {}: {err}", abs.display()));
        }
        let matcher = builder
            .build()
            .map_err(|err| format!("cannot compile ignore files: {err}"))?;
        matchers.push(matcher);
    }
    Ok(matchers)
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
    crate::text::decode_capped(bytes, MAX_FILE_BYTES as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn ctx(root: &Path, token: &CancellationToken) -> ToolContext {
        ToolContext::new(root.to_path_buf(), token.clone(), Duration::from_secs(10))
    }

    async fn search(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        let token = CancellationToken::new();
        SearchTool.call(args, &ctx(root, &token)).await
    }

    #[tokio::test]
    async fn finds_matching_lines_with_numbers() {
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::create_dir(base.join("src")).expect("mkdir");
        std::fs::write(base.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").expect("write");
        std::fs::write(base.join("src/b.rs"), "struct Gamma;\n").expect("write");

        let output = search(&root, json!({"pattern": "fn \\w+"}))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            "src/a.rs:1: fn alpha() {}\nsrc/a.rs:2: fn beta() {}"
        );
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn gitignored_files_are_skipped() {
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "secret.txt\n").expect("write");
        std::fs::write(base.join("secret.txt"), b"needle here\n").expect("write");
        std::fs::write(base.join("open.txt"), b"needle too\n").expect("write");

        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "open.txt:1: needle too");
    }

    #[tokio::test]
    async fn hidden_files_are_skipped() {
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".hidden"), b"needle\n").expect("write");
        std::fs::write(base.join("visible"), b"clean\n").expect("write");

        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "");
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("blob"), b"needle\x00binary\n").expect("write");
        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "");
    }

    #[tokio::test]
    async fn invalid_regex_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = search(&root, json!({"pattern": "*("}))
            .await
            .expect_err("bad regex");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("regex")));
    }

    #[tokio::test]
    async fn max_results_truncates() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("many.txt"), "hit\nhit\nhit\nhit\n").expect("write");
        let output = search(&root, json!({"pattern": "hit", "max_results": 2}))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            "many.txt:1: hit\nmany.txt:2: hit\n\
             ... [stopped at the 2-match cap; refine the pattern or pass path to narrow the search]"
        );
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn many_matches_are_summarized_per_file() {
        let (guard, root) = temp_root();
        let hits = |n: usize| "hit\n".repeat(n);
        std::fs::write(guard.path().join("a.txt"), hits(130)).expect("write");
        std::fs::write(guard.path().join("b.txt"), hits(25)).expect("write");
        let output = search(&root, json!({"pattern": "hit"})).await.expect("ok");
        let content = &output.content;
        assert!(content.starts_with("155 matches in 2 files\n"), "{content}");
        assert!(content.contains(
            "a.txt (130 matches)\n  1: hit\n  2: hit\n  3: hit\n  \
             4: hit\n  5: hit\n"
        ));
        assert!(content.contains(
            "b.txt (25 matches)\n  1: hit\n  2: hit\n  3: hit\n  \
             4: hit\n  5: hit\n"
        ));
        assert!(content.contains("... [145 more matches; refine the pattern or pass path"));
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn a_capped_search_says_it_stopped_early() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("a.txt"), "hit\n".repeat(40)).expect("write");
        std::fs::write(guard.path().join("b.txt"), "hit\n".repeat(40)).expect("write");
        // Under the summary threshold the hits are listed raw, with the cap
        // marker naming what the cap dropped.
        let output = search(&root, json!({"pattern": "hit", "max_results": 25}))
            .await
            .expect("ok");
        assert!(
            output.content.starts_with("a.txt:1: hit\n"),
            "{}",
            output.content
        );
        assert!(
            output
                .content
                .contains("... [stopped at the 25-match cap; refine the pattern"),
            "{}",
            output.content
        );
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn zero_max_results_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = search(&root, json!({"pattern": "x", "max_results": 0}))
            .await
            .expect_err("zero");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("max_results")));
    }

    #[tokio::test]
    async fn scope_escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = search(&root, json!({"pattern": "x", "path": ".."}))
            .await
            .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[tokio::test]
    async fn pre_cancelled_token_is_execution_error() {
        let (_guard, root) = temp_root();
        let token = CancellationToken::new();
        token.cancel();
        let err = SearchTool
            .call(json!({"pattern": "x"}), &ctx(&root, &token))
            .await
            .expect_err("cancelled");
        assert!(matches!(err, ToolError::Execution(msg) if msg == "cancelled"));
    }

    #[tokio::test]
    async fn scoped_search_limits_paths() {
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::create_dir(base.join("a")).expect("mkdir");
        std::fs::create_dir(base.join("b")).expect("mkdir");
        std::fs::write(base.join("a/f"), b"needle\n").expect("write");
        std::fs::write(base.join("b/f"), b"needle\n").expect("write");

        let output = search(&root, json!({"pattern": "needle", "path": "a"}))
            .await
            .expect("ok");
        // Workspace-relative even under a scope, so read_file can consume it.
        assert_eq!(output.content, "a/f:1: needle");
    }

    #[tokio::test]
    async fn file_scope_reports_workspace_relative_path() {
        let (guard, root) = temp_root();
        std::fs::create_dir(guard.path().join("src")).expect("mkdir");
        std::fs::write(guard.path().join("src/a.rs"), "fn alpha() {}\n").expect("write");
        let output = search(&root, json!({"pattern": "alpha", "path": "src/a.rs"}))
            .await
            .expect("ok");
        // A file scope must still name the file, not strip to an empty path.
        assert_eq!(output.content, "src/a.rs:1: fn alpha() {}");
    }

    #[tokio::test]
    async fn missing_scope_is_informational() {
        let (_guard, root) = temp_root();
        let output = search(&root, json!({"pattern": "x", "path": "nope"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "path \"nope\" does not exist");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn oversized_multibyte_file_is_searched_not_skipped() {
        let (guard, root) = temp_root();
        // An early match, then ASCII filler up to one byte before the
        // cap, then a two-byte `é` whose first byte is the last byte
        // the cap admits — the cut lands mid-character. The file must
        // still be searched (not silently skipped), and the match
        // beyond the cap must not be reported.
        let mut bytes = Vec::with_capacity(MAX_FILE_BYTES as usize + 32);
        bytes.extend_from_slice(b"early needle\n");
        bytes.extend(std::iter::repeat_n(
            b'a',
            MAX_FILE_BYTES as usize - bytes.len() - 1,
        ));
        bytes.extend_from_slice("é plus tail needle\n".as_bytes());
        assert!(bytes.len() > MAX_FILE_BYTES as usize);
        std::fs::write(guard.path().join("big.txt"), &bytes).expect("write");

        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "big.txt:1: early needle");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn scoped_search_honors_workspace_gitignore() {
        // Round-4: `.parents(false)` kept the crate from loading the
        // root `.gitignore` when the scope is a subfolder — scoped
        // results must not depend on which scope the caller picked.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "*.log\n").expect("write");
        std::fs::create_dir(base.join("src")).expect("mkdir");
        std::fs::write(base.join("src/foo.log"), b"needle\n").expect("write");
        std::fs::write(base.join("src/bar.txt"), b"needle\n").expect("write");

        let output = search(&root, json!({"pattern": "needle", "path": "src"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "src/bar.txt:1: needle");
    }

    #[tokio::test]
    async fn scoped_search_honors_path_styled_workspace_gitignore() {
        // Round-5: an ancestor rule that names a path (`src/gen/`)
        // must anchor against the workspace root, not the CWD — the
        // scoped search has to exclude `src/gen/` exactly like the
        // whole-tree search does.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "src/gen/\n").expect("write");
        std::fs::create_dir_all(base.join("src/gen")).expect("mkdir");
        std::fs::write(base.join("src/gen/out.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");

        let output = search(&root, json!({"pattern": "needle", "path": "src"}))
            .await
            .expect("ok");
        assert_eq!(output.content, "src/lib.rs:1: needle");
    }

    #[tokio::test]
    async fn scoped_and_whole_search_agree_on_path_rules() {
        // Round-5: scoped and whole-tree searches must produce the
        // same filtered set for a path-style ignore rule.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "generated/\n").expect("write");
        std::fs::create_dir(base.join("generated")).expect("mkdir");
        std::fs::write(base.join("generated/out.rs"), b"needle\n").expect("write");
        std::fs::create_dir(base.join("src")).expect("mkdir");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");

        let token = CancellationToken::new();
        let scoped = SearchTool
            .call(
                json!({"pattern": "needle", "path": "src"}),
                &ctx(&root, &token),
            )
            .await
            .expect("ok");
        let whole = SearchTool
            .call(json!({"pattern": "needle"}), &ctx(&root, &token))
            .await
            .expect("ok");
        assert_eq!(scoped.content, "src/lib.rs:1: needle");
        assert_eq!(whole.content, "src/lib.rs:1: needle");
    }

    #[tokio::test]
    async fn scoped_search_nearest_ignore_file_wins() {
        // Round-6: with two ancestor ignore files, git precedence says
        // the DEEPER file wins (`!gen/` re-includes what root `gen/`
        // excluded). Before the ancestor chain was consulted
        // deepest-first, the root rule won and the scoped search
        // dropped `src/gen/`, diverging from git and the whole-tree
        // search. Scope sits two levels below the root, so the nested
        // file's rules must also anchor against its own directory,
        // not the workspace root.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "gen/\n").expect("write");
        std::fs::create_dir_all(base.join("src/gen")).expect("mkdir");
        std::fs::write(base.join("src/.gitignore"), "!gen/\n").expect("write");
        std::fs::write(base.join("src/gen/out.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");

        let token = CancellationToken::new();
        let scoped = SearchTool
            .call(
                json!({"pattern": "needle", "path": "src/gen"}),
                &ctx(&root, &token),
            )
            .await
            .expect("ok");
        let whole = SearchTool
            .call(json!({"pattern": "needle"}), &ctx(&root, &token))
            .await
            .expect("ok");
        // Deeper `!gen/` must win in the scoped search (nearest-file
        // wins in the ancestor chain). The whole-tree walk agrees:
        // real git also keeps `src/gen/out.rs` here (verified with
        // `git check-ignore`) — git's cannot-re-include rule applies
        // only when a *parent directory* of the path is excluded, and
        // neither `src` nor the root is. The walk pins that parity in
        // `whole_tree_walk_matches_git_on_nested_negation`.
        assert_eq!(scoped.content, "src/gen/out.rs:1: needle");
        assert_eq!(
            whole.content,
            "src/gen/out.rs:1: needle\nsrc/lib.rs:1: needle"
        );
    }

    #[tokio::test]
    async fn whole_tree_walk_matches_git_on_nested_negation() {
        // Item 16: pin the whole-tree walk against real git for the
        // documented negation case — root `gen/` plus `src/.gitignore`
        // `!gen/`. `git check-ignore` keeps `src/gen/out.rs` (the
        // nested whitelist wins; no parent directory of the path is
        // excluded), so both hits survive. The header's old claim that
        // the `ignore` crate is looser than git here was wrong: git
        // only refuses to re-include under an excluded parent
        // directory, which is pinned in
        // `repo_shaped_tree_scoped_and_whole_agree` below.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "gen/\n").expect("write");
        std::fs::create_dir_all(base.join("src/gen")).expect("mkdir");
        std::fs::write(base.join("src/.gitignore"), "!gen/\n").expect("write");
        std::fs::write(base.join("src/gen/out.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");

        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            "src/gen/out.rs:1: needle\nsrc/lib.rs:1: needle"
        );
    }

    #[tokio::test]
    async fn scoped_whitelist_below_scope_is_pruned() {
        // Item 16, §9.2 — the one DELIBERATE divergence from git, now
        // pinned: a whitelist in an ignore file *inside* the scope
        // (`src/.gitignore` `!keep.log`) cannot rescue a file an
        // ancestor rule excludes (root `*.log`). The ancestor filter
        // only consults files down to the scope and prunes first, so
        // the scoped walk is STRICTER than git, which re-includes
        // `src/keep.log` during descent. Whole-tree and git agree.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "*.log\n").expect("write");
        std::fs::create_dir(base.join("src")).expect("mkdir");
        std::fs::write(base.join("src/.gitignore"), "!keep.log\n").expect("write");
        std::fs::write(base.join("src/keep.log"), b"needle\n").expect("write");
        std::fs::write(base.join("keep.log"), b"needle\n").expect("write");

        let whole = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        // Nested whitelist wins during descent: git parity.
        assert_eq!(whole.content, "src/keep.log:1: needle");

        let scoped = search(&root, json!({"pattern": "needle", "path": "src"}))
            .await
            .expect("ok");
        // §9.2 divergence: the root `*.log` ancestor filter prunes
        // `src/keep.log`; the below-scope whitelist never runs.
        assert_eq!(scoped.content, "");
    }

    #[tokio::test]
    async fn chained_ignores_subdir_overrides_parent() {
        // Item 16: chained ignore files — a subdir's `.gitignore`
        // overriding its parent's. Git (verified): the deeper
        // `!build/` wins, so `src/build/gen.rs` is kept while the
        // root `build/` dir stays ignored.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::write(base.join(".gitignore"), "build/\n").expect("write");
        std::fs::create_dir_all(base.join("src/build")).expect("mkdir");
        std::fs::create_dir(base.join("build")).expect("mkdir");
        std::fs::write(base.join("src/.gitignore"), "!build/\n").expect("write");
        std::fs::write(base.join("src/build/gen.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("build/rootgen.rs"), b"needle\n").expect("write");

        let whole = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert_eq!(
            whole.content,
            "src/build/gen.rs:1: needle\nsrc/lib.rs:1: needle"
        );

        // Scope inside the whitelist's reach: both ignore files are
        // ancestors, nearest-file-wins in the ancestor chain keeps the
        // file — same as git and the whole-tree walk.
        let deep = search(&root, json!({"pattern": "needle", "path": "src/build"}))
            .await
            .expect("ok");
        assert_eq!(deep.content, "src/build/gen.rs:1: needle");

        // Scope at `src`: the `!build/` whitelist lives below the
        // scope, so the root `build/` ancestor filter prunes
        // `src/build` — the §9.2 strictness divergence.
        let scoped = search(&root, json!({"pattern": "needle", "path": "src"}))
            .await
            .expect("ok");
        assert_eq!(scoped.content, "src/lib.rs:1: needle");
    }

    #[tokio::test]
    async fn nested_path_styled_rule_anchors_at_its_own_directory() {
        // Item 16 bug fix: a path-style rule in a NESTED ignore file
        // anchors at that file's directory, in every scope. Git
        // (verified): `src/.gitignore` `/gen/` ignores `src/gen/` but
        // leaves a root-level `gen/` alone. The pre-fix merged matcher
        // anchored every ancestor rule at the workspace root, so a
        // `src/gen` scoped search leaked `src/gen/out.rs` that git —
        // and the whole-tree walk — ignore.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::create_dir_all(base.join("src/gen")).expect("mkdir");
        std::fs::create_dir(base.join("gen")).expect("mkdir");
        std::fs::write(base.join("src/.gitignore"), "/gen/\n").expect("write");
        std::fs::write(base.join("src/gen/out.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("src/lib.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("gen/rootgen.rs"), b"needle\n").expect("write");

        let whole = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        // `src/gen/out.rs` ignored (git parity); root `gen/` untouched.
        assert_eq!(
            whole.content,
            "gen/rootgen.rs:1: needle\nsrc/lib.rs:1: needle"
        );

        // The nested rule is an ancestor of `src/gen` — the scoped
        // walk must prune exactly like the whole-tree walk.
        let scoped = search(&root, json!({"pattern": "needle", "path": "src/gen"}))
            .await
            .expect("ok");
        assert_eq!(scoped.content, "");

        // At `src` the rule sits below the scope and the walker loads
        // it itself (its own anchoring), so this scope agrees too.
        let here = search(&root, json!({"pattern": "needle", "path": "src"}))
            .await
            .expect("ok");
        assert_eq!(here.content, "src/lib.rs:1: needle");
    }

    #[tokio::test]
    async fn repo_shaped_tree_scoped_and_whole_agree() {
        // Item 16: a real-repo-shaped tree — root ignores `target/`
        // and `*.log`, `vendor/.gitignore` anchors `/third_party/`
        // and whitelists `!kept.rs`, and `target/.gitignore` attempts
        // a re-include. All assertions verified against real git.
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::create_dir(base.join("src")).expect("mkdir");
        std::fs::create_dir_all(base.join("target/debug")).expect("mkdir");
        std::fs::create_dir_all(base.join("vendor/third_party")).expect("mkdir");
        std::fs::write(base.join(".gitignore"), "target/\n*.log\n").expect("write");
        std::fs::write(base.join("vendor/.gitignore"), "/third_party/\n!kept.rs\n").expect("write");
        std::fs::write(base.join("target/.gitignore"), "!important.rs\n").expect("write");
        std::fs::write(base.join("src/main.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("target/debug/x.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("target/important.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("vendor/third_party/a.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("vendor/kept.rs"), b"needle\n").expect("write");
        std::fs::write(base.join("app.log"), b"needle\n").expect("write");

        let whole = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        // `target/` is pruned before descent, so `target/important.rs`
        // cannot re-include — git's cannot-re-include-under-excluded-
        // parent rule, which the walk reproduces. `vendor/third_party/`
        // is ignored by its own anchored rule, `vendor/kept.rs`
        // re-included by its own whitelist.
        assert_eq!(
            whole.content,
            "src/main.rs:1: needle\nvendor/kept.rs:1: needle"
        );

        let vendor = search(&root, json!({"pattern": "needle", "path": "vendor"}))
            .await
            .expect("ok");
        assert_eq!(vendor.content, "vendor/kept.rs:1: needle");

        // Pre-fix, this scope leaked `vendor/third_party/a.rs`: the
        // merged ancestor matcher anchored `/third_party/` at the
        // workspace root. Per-file anchoring prunes it, like git and
        // the whole-tree walk.
        let third_party = search(
            &root,
            json!({"pattern": "needle", "path": "vendor/third_party"}),
        )
        .await
        .expect("ok");
        assert_eq!(third_party.content, "");
    }

    #[tokio::test]
    async fn long_matched_lines_are_truncated() {
        // Round-4: a single-line file must not buffer megabytes per
        // match before the engine's output bound sees it.
        let (guard, root) = temp_root();
        let mut line = String::from("needle");
        line.extend(std::iter::repeat_n('x', 500));
        std::fs::write(guard.path().join("big-line.txt"), &line).expect("write");

        let output = search(&root, json!({"pattern": "needle"}))
            .await
            .expect("ok");
        assert!(output.content.starts_with("big-line.txt:1: needle"));
        assert!(output.content.ends_with('…'));
        assert_eq!(
            output.content.len(),
            "big-line.txt:1: ".len() + MAX_MATCH_LINE_BYTES + '…'.len_utf8()
        );
    }

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<SearchInput>(json!({"pattern": "fn main"}));
    }
}
