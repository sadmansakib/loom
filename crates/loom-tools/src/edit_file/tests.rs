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
    assert_eq!(output.effects.repairs.len(), 0);
    assert_eq!(
        std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
        "fn add() { 1 }\n"
    );
}

#[tokio::test]
async fn crlf_file_matches_an_lf_old_string_and_keeps_crlf() {
    // Review item 3.1: the model's LF-only old_string must match a
    // CRLF file, and the written file keeps CRLF throughout.
    let (guard, root) = temp_root();
    std::fs::write(guard.path().join("f.rs"), b"fn a() {}\r\nfn b() {}\r\n").expect("seed");
    let output = edit(
        &root,
        json!({
            "path": "f.rs",
            "old_string": "fn a() {}\nfn b() {}",
            "new_string": "fn a() { 1 }\nfn b() { 2 }"
        }),
    )
    .await
    .expect("edit ok");
    assert!(
        output
            .content
            .contains("matched after normalizing line endings"),
        "{}",
        output.content
    );
    assert_eq!(
        std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
        "fn a() { 1 }\r\nfn b() { 2 }\r\n"
    );
}

#[tokio::test]
async fn a_bom_file_edits_and_the_bom_survives() {
    let (guard, root) = temp_root();
    std::fs::write(
        guard.path().join("f.rs"),
        b"\xef\xbb\xbffrid fn main() {}\n",
    )
    .expect("seed");
    edit(
        &root,
        json!({
            "path": "f.rs",
            "old_string": "fn main() {}",
            "new_string": "fn main() { 42 }"
        }),
    )
    .await
    .expect("edit ok");
    let written = std::fs::read(guard.path().join("f.rs")).expect("read");
    assert_eq!(
        written,
        b"\xef\xbb\xbffrid fn main() { 42 }\n".to_vec(),
        "the BOM must survive the edit untouched"
    );
}

#[tokio::test]
async fn mixed_endings_are_never_rewritten() {
    // A mixed-endings file: only the matched span changes, and the
    // model's LF new_string stays LF (no wholesale rewrite).
    let (guard, root) = temp_root();
    std::fs::write(
        guard.path().join("f.rs"),
        b"fn a() {}\r\nfn b() {}\nfn c() {}\r\n",
    )
    .expect("seed");
    edit(
        &root,
        json!({
            "path": "f.rs",
            "old_string": "fn b() {}\nfn c() {}",
            "new_string": "fn b() { 1 }\nfn c() { 2 }"
        }),
    )
    .await
    .expect("edit ok");
    assert_eq!(
        std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
        "fn a() {}\r\nfn b() { 1 }\nfn c() { 2 }\r\n"
    );
}

#[tokio::test]
async fn a_file_without_a_trailing_newline_keeps_it_that_way() {
    let (guard, root) = temp_root();
    std::fs::write(guard.path().join("f.rs"), b"fn a() {}").expect("seed");
    edit(
        &root,
        json!({
            "path": "f.rs",
            "old_string": "fn a() {}",
            "new_string": "fn a() { 1 }"
        }),
    )
    .await
    .expect("edit ok");
    assert_eq!(
        std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
        "fn a() { 1 }",
        "no trailing newline may be introduced"
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
    // ToolError is #[non_exhaustive]: the wildcard arm is required.
    #[allow(clippy::wildcard_enum_match_arm)]
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
    // ToolError is #[non_exhaustive]: the wildcard arm is required.
    #[allow(clippy::wildcard_enum_match_arm)]
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
        output
            .content
            .contains("f.rs: lines 1-1 of 3\n1\tfn uno() {}\n2\tfn two() {}\n3\tfn tres() {}\n"),
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
async fn an_edit_that_swallows_an_earlier_edits_region_drops_its_stale_span() {
    // Review E1: edit 2's old_string contains edit 1's
    // replacement, so edit 1's recorded span points into bytes
    // that no longer exist once edit 2 lands. The stale span used
    // to survive with shifted offsets and panic the echo *after*
    // the file had already been written.
    let (guard, root) = temp_root();
    std::fs::write(
        guard.path().join("f.rs"),
        "fn main() {\n    let x = compute(value);\n    let y = 2;\n}\n",
    )
    .expect("seed");
    let output = edit(
            &root,
            json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "compute(value)", "new_string": "compute"},
                    {"old_string": "let x = compute;\n    let y = 2;", "new_string": "let x = compute(2);"}
                ]
            }),
        )
        .await
        .expect("ok");
    assert_eq!(
        std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
        "fn main() {\n    let x = compute(2);\n}\n"
    );
    // Exactly one echoed region: the swallowed edit's stale span
    // is dropped rather than rendered from bogus offsets.
    assert_eq!(
        output.content.matches("f.rs: lines").count(),
        1,
        "{}",
        output.content
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
        root.clone(),
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
