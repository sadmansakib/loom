//! editbench — a deterministic, token-free `edit_file` micro-benchmark
//! (efficiency-eval phase 2, recommendation 3).
//!
//! The live corpus's largest file is 614 lines (BIG1), and most field
//! tasks never exceed 129 — yet real repositories hand the editor
//! thousand-line files with near-duplicate blocks, multi-byte text, and
//! batched cross-file edits. PR-Edit (arXiv 2604.26102) showed
//! editor-level success is (a) per-task decidable, (b) trainable in
//! isolation, and (c) correlates ~0.98 with downstream resolve rate —
//! so the editor earns its own benchmark that costs zero tokens.
//!
//! Every case is generated deterministically (loops, no RNG), applied
//! to a scratch workspace under `target/eval/editbench`, and graded on
//! the filesystem state — the same ground truth the bugfix gates use.
//! The offline test asserts the correctness invariants (a regression
//! fails the suite) and always writes `target/eval/report-editbench.json`
//! for inspection; the interesting numbers are the per-family counts
//! and the size buckets the live corpus never reaches.
//!
//! Families:
//!
//! * `apply` — unique-anchor replacement across size buckets
//!   (40 / 240 / 1 200 / 3 000 lines): must apply, old text gone, new
//!   text present exactly once.
//! * `ambiguity` — the anchor occurs twice: must be refused as invalid
//!   input, file byte-identical. A wrong-location apply is the
//!   silent-corruption failure mode; it must never happen.
//! * `not_found` — anchor absent, no trimmed match: refused, file
//!   unchanged.
//! * `repair` — exact anchor absent but a whitespace-trimmed match is
//!   unique: applies with a recorded repair (the repair machinery is
//!   a harness lever; its fire rate belongs in the report).
//! * `batch_atomic` — one bad spec among good ones refuses the whole
//!   batch, no partial writes.
//! * `cross_file` — a good multi-file batch applies everywhere.
//! * `unicode` — multi-byte anchors (CJK, emoji) round-trip exactly.
//! * `oversize` — a file past the 8 MiB cap is refused cleanly, never
//!   panicking or hanging.
//! * `python_indent` — an indented block replacement keeps inner
//!   indentation byte-exact (Python has no delimiters to save you).
//! * `json_strict` — an edit inside escaped quotes and nested braces
//!   leaves the document parseable with the intended value.
//! * `yaml_indent` — exact-indent anchors inside nested mappings.
//! * `toml_keys` — values under dotted and nested tables.
//! * `js_template` — inside template literals, `${…}` and backslashes.
//! * `shell_quote` — quoted strings and heredocs in `.sh`.
//! * `crlf` — a CRLF anchor applies; the same anchor with bare `\n`
//!   is `not found` (no cross-ending fuzzy match), bytes untouched.
//! * `no_extension` — Makefile-style paths with hard tabs.
//! * `polyglot_batch` — one batch atomically touching
//!   `.py`/`.js`/`.sh`/`.json`/`.yaml`/`.toml`.
//! * `go_tabs` — Go's mandatory tab indentation round-trips exactly.
//! * `c_buffer` — `sprintf` → `snprintf` with `sizeof` sizing in C.
//! * `java_equals` — reference `==` → `a.equals(b)` with null guard.
//! * `csharp_coalesce` — `?? string.Empty` null-coalescing edit.
//! * `ruby_interp` — single→double quoted interpolation plus a
//!   squiggly heredoc (`<<~`) in the same file.
//! * `php_interp` — `$var` interpolation needs double quotes.
//! * `ts_annotation` — typed-parameter anchors in TypeScript.
//! * `kotlin_elvis` — `!!` force-unwrap → `?:` elvis edit.
//! * `swift_guard` — force unwrap → `guard let … else` block.
//! * `sql_dialects` — Postgres `$$`-quoted function bodies and MySQL
//!   backtick-quoted identifiers.
//! * `shells_modern` — `zsh` `setopt null_glob` and `fish`
//!   `set -gx` (bashisms are bugs there).
//! * `ps1_subexpr` — PowerShell subexpression interpolation
//!   `$($row.Name)` with a backtick line continuation in the file.

use loom_core::kernel::tool::{Tool, ToolContext, ToolError};
use loom_core::tools::EditFileTool;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

struct Case {
    family: &'static str,
    lines: u64,
}

/// The eval artifact dir (same rule as `eval.rs`: `LOOM_EVAL_DIR`, else
/// `<workspace>/target/eval`).
fn eval_dir() -> PathBuf {
    std::env::var("LOOM_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("target")
                .join("eval")
        })
}

/// Scratch root for every case: `<eval dir>/editbench` (wiped per run).
fn bench_root() -> PathBuf {
    let root = eval_dir().join("editbench");
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("wipe prior editbench workspace");
    }
    std::fs::create_dir_all(&root).expect("create editbench workspace");
    root
}

/// One deterministic fixture: `lines` numbered `fn` stubs plus the
/// anchor block at a position that is not the first or last line, so a
/// wrong-location match would be detectable.
fn fixture(root: &Path, name: &str, lines: u64) -> (PathBuf, String, String, String) {
    let path = root.join(name);
    let anchor = format!("fn anchor_{name}() -> u32 {{\n    {name}_SEED // target line\n}}\n");
    let mut text = String::with_capacity(64 * lines as usize);
    for i in 0..lines {
        if i == lines / 2 {
            text.push_str(&anchor);
        }
        text.push_str(&format!("fn filler_{name}_{i:06}() -> u32 {{ {i} }}\n"));
    }
    std::fs::write(&path, &text).expect("write fixture");
    let replacement = format!("fn anchor_{name}() -> u32 {{\n    {name}_EDITED // replaced\n}}\n");
    (path, text, anchor, replacement)
}

/// Calls the real tool with a real context bound to the scratch root.
async fn call_edit(root: &Path, input: serde_json::Value) -> Result<String, ToolError> {
    let tool = EditFileTool;
    let ctx = ToolContext::new(
        root.to_path_buf(),
        CancellationToken::new(),
        std::time::Duration::from_secs(30),
    );

    tool.call(input, &ctx)
        .await
        .map(|output| output.content.clone())
}

/// Grading is filesystem truth: applied means old gone + new once.
fn grade_applied(text_after: &str, old: &str, new: &str) -> bool {
    !text_after.contains(old) && text_after.matches(new).count() == 1
}

#[tokio::test]
async fn editbench_the_editor_holds_its_invariants() {
    let root = bench_root();
    let mut cases: Vec<Case> = Vec::new();
    let mut family_totals: std::collections::BTreeMap<&'static str, (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut record =
        |family: &'static str, _bucket: &'static str, outcome: &'static str, lines: u64| {
            cases.push(Case { family, lines });
            let entry = family_totals.entry(family).or_default();
            entry.0 += 1;
            if outcome == "ok" {
                entry.1 += 1;
            }
        };

    // -- apply: unique anchor across size buckets -----------------------
    for (bucket, lines) in [
        ("small", 40u64),
        ("medium", 240),
        ("large", 1_200),
        ("xlarge", 3_000),
    ] {
        let name = format!("apply_{bucket}.rs");
        let (path, _text, anchor, replacement) = fixture(&root, &name, lines);
        let input = json!({
            "path": name,
            "old_string": anchor,
            "new_string": replacement,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        let ok = result.is_ok() && grade_applied(&after, &anchor, &replacement);
        assert!(
            ok,
            "apply/{bucket}: unique anchor must apply exactly once ({result:?})"
        );
        record("apply", bucket, "ok", lines);
    }

    // -- ambiguity: anchor twice, must refuse, must not corrupt ---------
    {
        let (path, _text, _anchor, _replacement) = fixture(&root, "ambiguous.rs", 240);
        let dup_at = 30usize;
        let dup_line =
            "fn anchor_ambiguous.rs() -> u32 {\n    ambiguous.rs_SEED // target line\n}\n";
        let mut text = std::fs::read_to_string(&path).expect("fixture readable");
        let dup_insert = dup_line.to_string();
        text.insert_str(dup_at * 40, &dup_insert);
        std::fs::write(&path, &text).expect("write duplicated fixture");
        let anchor = "fn anchor_ambiguous.rs() -> u32 {\n    ambiguous.rs_SEED // target line\n}\n";
        let input = json!({
            "path": "ambiguous.rs",
            "old_string": anchor,
            "new_string": "// replaced\n",
        });
        let result = call_edit(&root, input).await;
        let refused =
            matches!(&result, Err(ToolError::InvalidInput(msg)) if msg.contains("appears 2 times"));
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            refused && after == text,
            "ambiguity: must refuse with `appears 2 times` and leave bytes identical ({result:?})"
        );
        record("ambiguity", "medium", "ok", 240);
    }

    // -- not_found: absent anchor, no trimmed rescue ---------------------
    {
        let (path, _text, _anchor, _replacement) = fixture(&root, "missing.rs", 240);
        let before = std::fs::read_to_string(&path).expect("fixture readable");
        let input = json!({
            "path": "missing.rs",
            "old_string": "fn totally_absent_marker() {}",
            "new_string": "// replaced\n",
        });
        let result = call_edit(&root, input).await;
        let refused =
            matches!(&result, Err(ToolError::InvalidInput(msg)) if msg.contains("not found"));
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            refused && after == before,
            "not_found: must refuse with `not found` and leave bytes identical ({result:?})"
        );
        record("not_found", "medium", "ok", 240);
    }

    // -- repair: whitespace-padded anchor finds the trimmed match -------
    {
        let (path, _text, anchor, replacement) = fixture(&root, "repair.rs", 240);
        let padded = format!("  {}  ", anchor.trim_end_matches('\n'));
        let input = json!({
            "path": "repair.rs",
            "old_string": padded,
            "new_string": replacement,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        let repaired = matches!(&result, Ok(content) if content.contains("trimming whitespace"));
        assert!(
            repaired && grade_applied(&after, &anchor, &replacement),
            "repair: padded anchor must apply via the trimmed match with a recorded repair ({result:?})"
        );
        record("repair", "medium", "ok", 240);
    }

    // -- batch_atomic: one bad spec refuses the whole batch -------------
    {
        let (path_a, _text, anchor_a, replacement_a) = fixture(&root, "batch_a.rs", 240);
        let before_a = std::fs::read_to_string(&path_a).expect("fixture readable");
        let input = json!({
            "path": "batch_a.rs",
            "edits": [
                {"path": "batch_a.rs", "old_string": anchor_a, "new_string": replacement_a},
                {"path": "batch_a.rs", "old_string": "fn absent_in_batch() {}", "new_string": "// x\n"},
            ]
        });
        let result = call_edit(&root, input).await;
        let refused = matches!(&result, Err(ToolError::InvalidInput(msg)) if msg.contains("edit 2 of 2") && msg.contains("not found"));
        let after_a = std::fs::read_to_string(&path_a).expect("fixture readable");
        assert!(
            refused && after_a == before_a,
            "batch_atomic: a failing spec must void the whole batch, no partial write ({result:?})"
        );
        record("batch_atomic", "medium", "ok", 240);
    }

    // -- cross_file: a good batch applies in every file ------------------
    {
        let (path_x, _tx, anchor_x, repl_x) = fixture(&root, "cross_x.rs", 120);
        let (path_y, _ty, anchor_y, repl_y) = fixture(&root, "cross_y.rs", 480);
        let (path_z, _tz, anchor_z, repl_z) = fixture(&root, "cross_z.rs", 1_200);
        let input = json!({
            "path": "cross_x.rs",
            "edits": [
                {"path": "cross_x.rs", "old_string": anchor_x, "new_string": repl_x},
                {"path": "cross_y.rs", "old_string": anchor_y, "new_string": repl_y},
                {"path": "cross_z.rs", "old_string": anchor_z, "new_string": repl_z},
            ]
        });
        let result = call_edit(&root, input).await;
        let ok = result.is_ok()
            && grade_applied(
                &std::fs::read_to_string(&path_x).expect("r"),
                &anchor_x,
                &repl_x,
            )
            && grade_applied(
                &std::fs::read_to_string(&path_y).expect("r"),
                &anchor_y,
                &repl_y,
            )
            && grade_applied(
                &std::fs::read_to_string(&path_z).expect("r"),
                &anchor_z,
                &repl_z,
            );
        assert!(ok, "cross_file: every good spec must apply ({result:?})");
        record("cross_file", "mixed", "ok", 1_800);
    }

    // -- unicode: multi-byte anchors round-trip byte-exactly ------------
    {
        let path = root.join("unicode.rs");
        let anchor = "fn 前端() -> &str {\n    \"🐍 emoji inside\" // 目标行\n}\n";
        let replacement = "fn 前端() -> &str {\n    \"✅ replaced\" // 替换后\n}\n";
        let mut text = String::from("// unicode fixture\n");
        text.push_str(anchor);
        for i in 0..40u64 {
            text.push_str(&format!("fn filler_unicode_{i:03}() -> u32 {{ {i} }}\n"));
        }
        std::fs::write(&path, &text).expect("write unicode fixture");
        let input = json!({
            "path": "unicode.rs",
            "old_string": anchor,
            "new_string": replacement,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, anchor, replacement),
            "unicode: multi-byte anchor must round-trip exactly ({result:?})"
        );
        record("unicode", "small", "ok", 42);
    }

    // -- oversize: past the 8 MiB cap, refused cleanly -------------------
    {
        let path = root.join("oversize.rs");
        // ~8.4 MiB of ASCII lines: comfortably past MAX_EDIT_FILE_BYTES.
        let chunk =
            "fn oversize_padding() -> u32 { 0 } // 0123456789012345678901234567890123456789\n";
        let repeats = (8usize * 1024 * 1024 + chunk.len()) / chunk.len();
        let text = chunk.repeat(repeats);
        std::fs::write(&path, &text).expect("write oversize fixture");
        let input = json!({
            "path": "oversize.rs",
            "old_string": "fn oversize_padding() -> u32 { 0 }",
            "new_string": "fn replaced() -> u32 { 1 }",
        });
        let result = call_edit(&root, input).await;
        assert!(
            result.is_err(),
            "oversize: a file past the cap must be refused, not edited"
        );
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert_eq!(after.len(), text.len(), "oversize: bytes must be untouched");
        record("oversize", "xlarge", "ok", repeats as u64);
    }

    // -- python_indent: block replacement keeps inner indentation ------
    {
        let path = root.join("pipeline.py");
        let old_block = "def run(items):\n    total = 0\n    for item in items:\n        total += item\n    return total\n";
        let new_block = "def run(items):\n    total = 0\n    skipped = 0\n    for item in items:\n        if item is None:\n            skipped += 1\n            continue\n        total += item\n    return total, skipped\n";
        let mut text = String::from("# pipeline\n\n");
        text.push_str(old_block);
        text.push_str("\ndef main() -> None:\n    run([])\n");
        std::fs::write(&path, &text).expect("write python fixture");
        let input = json!({
            "path": "pipeline.py",
            "old_string": old_block,
            "new_string": new_block,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old_block, new_block),
            "python_indent: indented block must apply byte-exactly ({result:?})"
        );
        record("python_indent", "small", "ok", 8);
    }

    // -- json_strict: inside escaped quotes and nested braces ----------
    {
        let path = root.join("config.json");
        let old = "\"summary\": \"quota \\\"hard\\\" limit reached\"";
        let new = "\"summary\": \"quota \\\"soft\\\" limit raised\"";
        let text = format!(
            "{{\n  \"nested\": {{\n    {old},\n    \"depth\": 3\n  }},\n  \"items\": [1, 2, {{\"k\": \"v\"}}]\n}}\n"
        );
        std::fs::write(&path, &text).expect("write json fixture");
        let input = json!({
            "path": "config.json",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        let parsed = serde_json::from_str::<serde_json::Value>(&after).ok();
        let ok = result.is_ok()
            && parsed
                .as_ref()
                .and_then(|v| v.pointer("/nested/summary"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == "quota \"soft\" limit raised");
        assert!(
            ok,
            "json_strict: escaped-quote edit must parse with the intended value ({result:?})"
        );
        record("json_strict", "small", "ok", 6);
    }

    // -- yaml_indent: exact-indent anchors in nested mappings ----------
    {
        let path = root.join("service.yaml");
        let old = "  limits:\n    cpu: 500m\n    memory: 1Gi";
        let new = "  limits:\n    cpu: 1000m\n    memory: 2Gi\n    ephemeral: 512Mi";
        let text = format!(
            "apiVersion: v1\nkind: Pod\nspec:\n  containers:\n  - name: web\n{old}\n    ports:\n    - 8080\n"
        );
        std::fs::write(&path, &text).expect("write yaml fixture");
        let input = json!({
            "path": "service.yaml",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "yaml_indent: nested mapping edit must apply exactly ({result:?})"
        );
        record("yaml_indent", "small", "ok", 8);
    }

    // -- toml_keys: dotted and nested tables ---------------------------
    {
        let path = root.join("Cargo.toml");
        let old = "[profile.release]\nlto = false\nopt-level = 3";
        let new = "[profile.release]\nlto = \"thin\"\nopt-level = 2\ncodegen-units = 1";
        let text =
            format!("[package]\nname = \"x\"\n\n{old}\n\ndependencies.loomy.path = \"../loomy\"\n");
        std::fs::write(&path, &text).expect("write toml fixture");
        let input = json!({
            "path": "Cargo.toml",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "toml_keys: table edit must apply exactly ({result:?})"
        );
        record("toml_keys", "small", "ok", 7);
    }

    // -- js_template: template literals, ${}, backslashes --------------
    {
        let path = root.join("render.js");
        let old = "const url = `${base}/v${version}/users?filter=${q}`;";
        let new = "const url = `${base}/v${version}/users?filter=${encodeURIComponent(q)}&safe=1`;";
        let text = format!(
            "// render\n{old}\nmodule.exports = {{ render: url }};\nconst re = /\\\\d+/; // backslash safety\n"
        );
        std::fs::write(&path, &text).expect("write js fixture");
        let input = json!({
            "path": "render.js",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "js_template: template-literal edit must apply exactly ({result:?})"
        );
        record("js_template", "small", "ok", 4);
    }

    // -- shell_quote: quoted strings and heredocs ----------------------
    {
        let path = root.join("deploy.sh");
        let old = "rsync -az $SRC $DEST/";
        let new = "rsync -az \"$SRC\" \"$DEST/\"";
        let text = format!(
            "#!/usr/bin/env bash\nset -euo pipefail\n{old}\n\ncat <<EOF\nlocked heredoc body: $SRC\nEOF\n"
        );
        std::fs::write(&path, &text).expect("write sh fixture");
        let input = json!({
            "path": "deploy.sh",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "shell_quote: quoted-string edit must apply exactly ({result:?})"
        );
        record("shell_quote", "small", "ok", 7);
    }

    // -- crlf: \r\n anchor applies; bare \n must not -------------------
    {
        // (a) CRLF old_string on a CRLF file: applies.
        let path = root.join("notes_crlf.txt");
        let old = "status: PENDING\r\nreview: none";
        let new = "status: DONE\r\nreview: ok";
        let mut text = String::from("log\r\n\r\n");
        text.push_str(old);
        text.push_str("\r\nfooter\r\n");
        std::fs::write(&path, &text).expect("write crlf fixture");
        let input = json!({
            "path": "notes_crlf.txt",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "crlf: a CRLF anchor must apply on a CRLF file ({result:?})"
        );

        // (b) Bare-\n anchor on a CRLF file: no exact match, and the
        // whole-string trim repair cannot rescue it (trim never touches
        // interior bytes) — refused, file untouched.
        let path2 = root.join("notes_crlf2.txt");
        let bare = "status: PENDING\nreview: none";
        let mut text2 = String::from("log\r\n\r\n");
        text2.push_str("status: PENDING\r\nreview: none\r\nfooter\r\n");
        std::fs::write(&path2, &text2).expect("write crlf2 fixture");
        let input2 = json!({
            "path": "notes_crlf2.txt",
            "old_string": bare,
            "new_string": "status: DONE\nreview: ok",
        });
        let result2 = call_edit(&root, input2).await;
        let after2 = std::fs::read_to_string(&path2).expect("fixture readable");
        let refused =
            matches!(&result2, Err(ToolError::InvalidInput(msg)) if msg.contains("not found"));
        assert!(
            refused && after2 == text2,
            "crlf: bare-\\n anchor must not match a CRLF file, bytes untouched ({result2:?})"
        );
        record("crlf", "small", "ok", 7);
    }

    // -- no_extension: Makefile paths with hard tabs -------------------
    {
        let path = root.join("Makefile");
        let old = "build:\n    cargo build --release";
        let new = "build:\n\tcargo build --release --locked";
        let text = format!(".PHONY: build test\n\n{old}\n\ntest:\n\tcargo test\n");
        std::fs::write(&path, &text).expect("write makefile fixture");
        let input = json!({
            "path": "Makefile",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "no_extension: tab-indented anchor must apply exactly ({result:?})"
        );
        record("no_extension", "small", "ok", 7);
    }

    // -- polyglot_batch: one atomic batch across six languages ---------
    {
        let files = [
            (
                "app.py",
                "def add(a, b):\n    return a + b\n",
                "def add(a, b):\n    return a + b  # sum\n",
            ),
            (
                "util.js",
                "const id = (x) => x;\n",
                "const id = (x) => x; // identity\n",
            ),
            ("run.sh", "echo start\n", "echo \"start\"\n"),
            ("cfg.json", "{\"on\": true}\n", "{\"on\": false}\n"),
            ("cfg.yaml", "on: true\n", "on: false\n"),
            ("cfg.toml", "on = true\n", "on = false\n"),
        ];
        for (name, seed, _edited) in files {
            std::fs::write(root.join(name), seed).expect("write polyglot fixture");
        }
        let edits: Vec<serde_json::Value> = files
            .iter()
            .map(|(name, seed, edited)| {
                json!({"path": name, "old_string": seed, "new_string": edited})
            })
            .collect();
        let input = json!({"path": files[0].0, "edits": edits});
        let result = call_edit(&root, input).await;
        let all_applied = files.iter().all(|(name, seed, edited)| {
            std::fs::read_to_string(root.join(name))
                .is_ok_and(|after| grade_applied(&after, seed, edited))
        });
        assert!(
            result.is_ok() && all_applied,
            "polyglot_batch: every language file must carry its edit ({result:?})"
        );
        record("polyglot_batch", "mixed", "ok", 6);
    }

    // -- go_tabs: Go's mandatory tab indentation round-trips ---------
    {
        let path = root.join("main.go");
        let old = "\tdata, _ := os.ReadFile(path)\n\treturn string(data)";
        let new = "\tdata, err := os.ReadFile(path)\n\tif err != nil {\n\t\treturn \"\"\n\t}\n\treturn string(data)";
        let mut text = String::from(
            "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc load(path string) string {\n",
        );
        text.push_str(old);
        text.push_str("\n}\n\nfunc main() {\n\tfmt.Println(load(\"input.txt\"))\n}\n");
        std::fs::write(&path, &text).expect("write go fixture");
        let input = json!({
            "path": "main.go",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "go_tabs: tab-indented Go block must apply byte-exactly ({result:?})"
        );
        record("go_tabs", "small", "ok", 5);
    }

    // -- c_buffer: sprintf -> snprintf with sizeof sizing -------------
    {
        let path = root.join("buffer.c");
        let old = "    sprintf(buf, \"hello %s\", name);";
        let new = "    snprintf(buf, sizeof buf, \"hello %s\", name);";
        let text = format!(
            "#include <stdio.h>\n\nvoid greet(char *name) {{\n    char buf[16];\n{old}\n    puts(buf);\n}}\n"
        );
        std::fs::write(&path, &text).expect("write c fixture");
        let input = json!({
            "path": "buffer.c",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "c_buffer: snprintf sizing edit must apply exactly ({result:?})"
        );
        record("c_buffer", "small", "ok", 5);
    }

    // -- java_equals: reference == -> .equals with null guard ---------
    {
        let path = root.join("Main.java");
        let old = "        return a == b;";
        let new = "        return a != null && a.equals(b);";
        let text = format!(
            "public class Main {{\n    static boolean same(String a, String b) {{\n{old}\n    }}\n}}\n"
        );
        std::fs::write(&path, &text).expect("write java fixture");
        let input = json!({
            "path": "Main.java",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "java_equals: equals edit must apply exactly ({result:?})"
        );
        record("java_equals", "small", "ok", 4);
    }

    // -- csharp_coalesce: ?? string.Empty -----------------------------
    {
        let path = root.join("Profile.cs");
        let old = "        return Display;";
        let new = "        return Display ?? string.Empty;";
        let text = format!(
            "public class Profile\n{{\n    public string? Display;\n\n    public string Label()\n    {{\n{old}\n    }}\n}}\n"
        );
        std::fs::write(&path, &text).expect("write csharp fixture");
        let input = json!({
            "path": "Profile.cs",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "csharp_coalesce: coalescing edit must apply exactly ({result:?})"
        );
        record("csharp_coalesce", "small", "ok", 4);
    }

    // -- ruby_interp: quotes + squiggly heredoc -----------------------
    {
        let path = root.join("greet.rb");
        let old = "  puts 'Hello #{name}!'";
        let new = "  puts \"Hello #{name}!\"";
        let text = format!(
            "# frozen_string_literal: true\n\ndef greet(name)\n{old}\nend\n\ndoc = <<~DESC\n  heredoc keeps its indent when squiggly.\n  name: #{{name}}\nDESC\n"
        );
        std::fs::write(&path, &text).expect("write ruby fixture");
        let input = json!({
            "path": "greet.rb",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "ruby_interp: interpolation edit must apply exactly ({result:?})"
        );
        record("ruby_interp", "small", "ok", 5);
    }

    // -- php_interp: $var needs double quotes -------------------------
    {
        let path = root.join("hello.php");
        let old = "echo 'Hello $name!';";
        let new = "echo \"Hello $name!\";";
        let text = format!("<?php\n$name = 'world';\n{old}\n");
        std::fs::write(&path, &text).expect("write php fixture");
        let input = json!({
            "path": "hello.php",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "php_interp: double-quote edit must apply exactly ({result:?})"
        );
        record("php_interp", "small", "ok", 3);
    }

    // -- ts_annotation: typed parameters ------------------------------
    {
        let path = root.join("format.ts");
        let old = "function greet(name) {";
        let new = "function greet(name: string) {";
        let text = format!("{old}\n  return `hi ${{name}}`;\n}}\n\nexport {{ greet }};\n");
        std::fs::write(&path, &text).expect("write ts fixture");
        let input = json!({
            "path": "format.ts",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "ts_annotation: typed-parameter edit must apply exactly ({result:?})"
        );
        record("ts_annotation", "small", "ok", 3);
    }

    // -- kotlin_elvis: !! force-unwrap -> ?: elvis --------------------
    {
        let path = root.join("User.kt");
        let old = "    return user!!.city";
        let new = "    return user?.city ?: \"unknown\"";
        let text = format!(
            "data class Address(val city: String)\n\nfun cityOf(user: Address?): String {{\n{old}\n}}\n"
        );
        std::fs::write(&path, &text).expect("write kotlin fixture");
        let input = json!({
            "path": "User.kt",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "kotlin_elvis: elvis edit must apply exactly ({result:?})"
        );
        record("kotlin_elvis", "small", "ok", 3);
    }

    // -- swift_guard: force unwrap -> guard let -----------------------
    {
        let path = root.join("fetch.swift");
        let old = "    let url = URL(string: raw)!\n    return url";
        let new = "    guard let url = URL(string: raw) else { return nil }\n    return url";
        let text = format!("import Foundation\n\nfunc load(raw: String) -> URL? {{\n{old}\n}}\n");
        std::fs::write(&path, &text).expect("write swift fixture");
        let input = json!({
            "path": "fetch.swift",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "swift_guard: guard-let edit must apply exactly ({result:?})"
        );
        record("swift_guard", "small", "ok", 4);
    }

    // -- sql_dialects: pg $$ bodies and mysql backticks ---------------
    {
        // (a) Postgres dollar-quoted function body.
        let pg = root.join("paid_total.sql");
        let old = "WHERE paid = true\n$$";
        let new = "WHERE paid = true AND total > 0\n$$";
        let text = format!(
            "CREATE FUNCTION paid_total()\nRETURNS bigint AS $$\n  SELECT COALESCE(SUM(total), 0) FROM orders\n{old}\nLANGUAGE SQL IMMUTABLE;\n"
        );
        std::fs::write(&pg, &text).expect("write pg fixture");
        let input = json!({
            "path": "paid_total.sql",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&pg).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "sql_dialects: pg $$-body edit must apply exactly ({result:?})"
        );

        // (b) MySQL backtick-quoted reserved-word table.
        let my = root.join("paid_orders.sql");
        let old2 = "FROM order WHERE";
        let new2 = "FROM `order` WHERE";
        let text2 = format!("SELECT id, status, total\n{old2}\n status = 'paid';\n");
        std::fs::write(&my, &text2).expect("write mysql fixture");
        let input2 = json!({
            "path": "paid_orders.sql",
            "old_string": old2,
            "new_string": new2,
        });
        let result2 = call_edit(&root, input2).await;
        let after2 = std::fs::read_to_string(&my).expect("fixture readable");
        assert!(
            result2.is_ok() && grade_applied(&after2, old2, new2),
            "sql_dialects: mysql backtick edit must apply exactly ({result2:?})"
        );
        record("sql_dialects", "small", "ok", 6);
    }

    // -- shells_modern: zsh setopt + fish set -gx ---------------------
    {
        // (a) zsh: no-match globs must vanish, not error.
        let zsh = root.join("setup.zsh");
        let old = "set -e\n\nfor f in *.tar.gz; do";
        let new = "set -e\nsetopt null_glob\n\nfor f in *.tar.gz; do";
        let text = format!(
            "#!/usr/bin/env zsh\n{old}\n\nfor f in *.tar.gz; do\n  print \"unpacking $f\"\n  tar -xzf \"$f\"\ndone\n"
        );
        std::fs::write(&zsh, &text).expect("write zsh fixture");
        let input = json!({
            "path": "setup.zsh",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&zsh).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "shells_modern: zsh setopt edit must apply exactly ({result:?})"
        );

        // (b) fish: `export` is a bashism; fish says `set -gx`.
        let fish = root.join("env.fish");
        let old2 = "export API_PORT=8080";
        let new2 = "set -gx API_PORT 8080";
        let text2 = format!("#!/usr/bin/env fish\n\n{old2}\nexport LOG_LEVEL=info\n");
        std::fs::write(&fish, &text2).expect("write fish fixture");
        let input2 = json!({
            "path": "env.fish",
            "old_string": old2,
            "new_string": new2,
        });
        let result2 = call_edit(&root, input2).await;
        let after2 = std::fs::read_to_string(&fish).expect("fixture readable");
        assert!(
            result2.is_ok() && grade_applied(&after2, old2, new2),
            "shells_modern: fish set -gx edit must apply exactly ({result2:?})"
        );
        record("shells_modern", "small", "ok", 5);
    }

    // -- ps1_subexpr: $($row.Name) + backtick continuation ------------
    {
        let path = root.join("bootstrap.ps1");
        let old = "Write-Host \"Table: $row.Name\"";
        let new = "Write-Host \"Table: $($row.Name)\"";
        let text = format!(
            "param(\n    [string]$ComputerName = \"localhost\"\n)\n\n$row = Get-Content -Raw config.json | ConvertFrom-Json\n{old}\nWrite-Host \"step one `\nfinished on $ComputerName\"\n"
        );
        std::fs::write(&path, &text).expect("write ps1 fixture");
        let input = json!({
            "path": "bootstrap.ps1",
            "old_string": old,
            "new_string": new,
        });
        let result = call_edit(&root, input).await;
        let after = std::fs::read_to_string(&path).expect("fixture readable");
        assert!(
            result.is_ok() && grade_applied(&after, old, new),
            "ps1_subexpr: subexpression edit must apply exactly ({result:?})"
        );
        record("ps1_subexpr", "small", "ok", 6);
    }

    // -- report ----------------------------------------------------------
    let families: Vec<serde_json::Value> = family_totals
        .iter()
        .map(|(family, (total, ok))| {
            let entries: Vec<&Case> = cases.iter().filter(|c| c.family == *family).collect();
            let max_lines = entries.iter().map(|c| c.lines).max().unwrap_or(0);
            json!({
                "family": family,
                "cases": total,
                "ok": ok,
                "ok_rate": *ok as f64 / *total as f64,
                "largest_file_lines": max_lines,
            })
        })
        .collect();
    let report = json!({
        "harness": "editbench 2H",
        "note": "deterministic offline editor bench; ok=behavior held, a failing assert fails the suite; largest live-corpus file is 614 lines for comparison",
        "cases": cases.len(),
        "families": families,
    });
    let dir = eval_dir();
    std::fs::create_dir_all(&dir).expect("eval dir");
    std::fs::write(
        dir.join("report-editbench.json"),
        serde_json::to_string_pretty(&report).expect("serialize report"),
    )
    .expect("write editbench report");
}
