//! The `search` tool: gitignore-aware regex line search.
//!
//! Ignore handling is delegated to the `ignore` crate. One divergence
//! from git is inherited and documented rather than fixed: a negation
//! (`!foo/`) in a nested `.gitignore` re-includes that directory for
//! the whole walk, where git cannot re-include a path under an
//! excluded parent directory. See the `scoped_search_nearest_ignore_file_wins`
//! test for the concrete case.

use futures::future::BoxFuture;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
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

/// Regex line search over the workspace tree: `.gitignore`-aware,
/// hidden files skipped, binary files skipped, cancellation checked
/// per file.
#[derive(Debug, Default)]
pub struct SearchTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    /// Regular expression (Rust syntax).
    pattern: String,
    /// Directory (or file) to search; defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Cap on reported matching lines; defaults to 200, minimum 1.
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for SearchTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "search".to_owned(),
            description: "Searches file contents with a regular expression across the \
                          workspace (gitignored and hidden files are skipped; binary \
                          files are skipped). Returns `path:line: text` entries with \
                          workspace-relative paths."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust regular expression to match per line."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search; defaults to the \
                                        whole workspace."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum matching lines to report (default 200)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        true
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
                Ok((lines, truncated)) => {
                    let mut output = ToolOutput::new(lines.join("\n"));
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
) -> Result<(Vec<String>, bool), String> {
    // Ancestor ignore files, shallowest (workspace root) first, so the
    // nearest file wins like git. Only ignore files INSIDE the workspace
    // participate; global ignores and parent lookups above the root
    // would make results machine-dependent. `.parents(false)` keeps the
    // crate from auto-loading this chain itself (it would root them at
    // the CWD and mis-anchor path-style rules), so it is loaded by hand
    // and compiled against the workspace root.
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
    // Collected deepest-first (walking up from the scope); reverse so the
    // workspace root's file is added first. `ignore`'s `Gitignore` is
    // last-match-wins, so shallowest-first reproduces git's
    // nearest-file-wins precedence. Before this reverse, a root `gen/`
    // rule overrode a nearer `!gen/` and scoped searches silently
    // dropped files git — and a whole-tree search — keeps.
    ancestors.reverse();
    let ancestor_matcher = build_ancestor_matcher(&ancestors, rel_base)?;

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
    if let Some(matcher) = &ancestor_matcher {
        // Root-relative match against absolute entry paths, so a
        // `src/gen/` rule anchors the same way for a whole-tree search
        // and a `src` scoped search. The directory (not its contents) is
        // what the filter sees; filtering the entry prunes the whole
        // subtree before descent.
        let rel_base_owned = rel_base.to_path_buf();
        let matcher = matcher.clone();
        builder.filter_entry(move |entry| {
            let Ok(rel) = entry.path().strip_prefix(&rel_base_owned) else {
                return true;
            };
            let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
            !matcher.matched(rel, is_dir).is_ignore()
        });
    }
    let walker = builder.build();

    let mut hits: Vec<String> = Vec::new();
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
                    hits.push(format!(
                        "{}:{}: {}",
                        rel.display(),
                        index + 1,
                        capped_line(line)
                    ));
                }
            }
        }
    }
    Ok((hits, truncated))
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

/// Builds a single matcher over `ancestor` ignore files (shallowest
/// first), compiled against the workspace root (`rel_base`) — not the
/// shallowest file's own directory — so path-style rules (e.g.
/// `src/gen/`) match the same relative path a whole-tree search
/// produces, keeping scoped and whole-tree results consistent
/// regardless of the CWD or the scope's depth.
///
/// Scope still exists: `ignore`'s `Gitignore` is last-match-wins across
/// all added files, so a root `*.log` + nearer `!keep.log` (git: kept)
/// is pruned by this filter. The scoped walk is therefore stricter than
/// git about negations (§9.2). The whole-tree walk runs inside the
/// walker's own (differently anchored) ignore handling.
///
/// `None` when no ancestor ignore file exists.
fn build_ancestor_matcher(
    ancestors: &[std::path::PathBuf],
    rel_base: &Path,
) -> Result<Option<Gitignore>, String> {
    if ancestors.is_empty() {
        return Ok(None);
    }
    // The matcher root is the workspace root, so every rule in every
    // ancestor file anchors against the same relative path a whole-tree
    // search produces.
    let mut builder = GitignoreBuilder::new(rel_base);
    for file in ancestors {
        if file.is_file()
            && let Some(err) = builder.add(file)
        {
            return Err(format!("cannot read ignore file {}: {err}", file.display()));
        }
    }
    Ok(Some(builder.build().map_err(|err| {
        format!("cannot compile ignore files: {err}")
    })?))
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
        assert_eq!(output.content, "many.txt:1: hit\nmany.txt:2: hit");
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
        // excluded). Before the ancestors.reverse() fix the root rule
        // was compiled last (last-match-wins) and the scoped search
        // dropped `src/gen/`, diverging from git and the whole-tree
        // search. Scope sits two levels below the root, so the matcher
        // must also anchor against the workspace root, not the nearest
        // ancestor dir.
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
        // wins after the ancestors.reverse() fix). The whole-tree walk
        // is left as-is here: `ignore`'s negation is not directory-scoped
        // (its `!gen/` re-includes `gen/` for the whole walk), so it
        // reports both `src/gen/out.rs` and `src/lib.rs`. Real git would
        // prune `src/gen/` (it cannot re-include under an excluded
        // directory), so the whole-tree search is *looser* than git here
        // — that is the `ignore` crate's behavior, not this fix's concern.
        assert_eq!(scoped.content, "src/gen/out.rs:1: needle");
        assert_eq!(
            whole.content,
            "src/gen/out.rs:1: needle\nsrc/lib.rs:1: needle"
        );
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
}
