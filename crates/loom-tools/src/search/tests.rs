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
        "src/a.rs:1: fn alpha() {}  [in alpha]\nsrc/a.rs:2: fn beta() {}  [in beta]"
    );
    assert!(!output.truncated);
}

#[tokio::test]
async fn cues_name_the_deepest_enclosing_symbol() {
    let (guard, root) = temp_root();
    let base = guard.path();
    std::fs::create_dir(base.join("src")).expect("mkdir");
    std::fs::write(
        base.join("src/a.rs"),
        "use std::fmt;\nstruct Outer;\nimpl Outer {\n    fn method(&self) -> i32 {\n        7\n    }\n}\n",
    )
    .expect("write");

    let output = search(&root, json!({"pattern": "7|fmt"}))
        .await
        .expect("ok");
    assert!(
        output.content.contains("src/a.rs:1: use std::fmt;"),
        "top-level line carries no cue"
    );
    assert!(
        output
            .content
            .contains("src/a.rs:5:         7  [in Outer/method]"),
        "nested line names the deepest symbol: {}",
        output.content
    );
}

#[tokio::test]
async fn cues_can_be_disabled_per_call() {
    let (guard, root) = temp_root();
    std::fs::write(guard.path().join("a.rs"), "fn alpha() {}\n").expect("write");
    TEST_CUES_DISABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let output = search(&root, json!({"pattern": "alpha"}))
        .await
        .expect("ok");
    TEST_CUES_DISABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(output.content, "a.rs:1: fn alpha() {}");
}

#[tokio::test]
async fn summary_form_lines_carry_cues_too() {
    let (guard, root) = temp_root();
    let base = guard.path();
    let mut source = String::new();
    for i in 0..SUMMARY_THRESHOLD + 1 {
        source.push_str(&format!("fn f{i}() {{ needle }}\n"));
    }
    std::fs::create_dir(base.join("src")).expect("mkdir");
    std::fs::write(base.join("src/big.rs"), source).expect("write");

    let output = search(&root, json!({"pattern": "needle"}))
        .await
        .expect("ok");
    assert!(
        output.content.contains("  1: fn f0() { needle }  [in f0]"),
        "summary preview carries the cue: {}",
        output.content
    );
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
    assert_eq!(output.content, "src/a.rs:1: fn alpha() {}  [in alpha]");
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
