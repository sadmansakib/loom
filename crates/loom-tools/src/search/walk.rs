//! The gitignore-aware workspace walk and ancestor-ignore matching
//! behind the `search` tool.

use ignore::Match as IgnoreMatch;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::Regex;
use std::path::Path;
use tokio_util::sync::CancellationToken;

use super::Match;
use super::read_capped_text;
use super::render::capped_line;

/// Walks `scope` and collects matching lines. Entries are sorted by
/// name within each directory, so the walk order — and therefore
/// which matches survive `max_results` truncation — is the same on
/// every machine (§8.1.3 determinism). `.gitignore` files between
/// the workspace root and `scope` apply, so a scoped search matches
/// a whole-workspace search. Matched lines longer than
/// [`MAX_MATCH_LINE_BYTES`] are truncated.
pub(super) fn search_tree(
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
