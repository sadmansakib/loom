use super::*;
use loom_kernel::tool::Tool;
use serde_json::json;
use std::time::Duration;

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    (dir, root)
}

fn ctx(root: &Path, token: &CancellationToken) -> ToolContext {
    ToolContext::new(root.to_path_buf(), token.clone(), Duration::from_secs(10))
}

async fn replace(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
    let token = CancellationToken::new();
    ReplaceAllTool::default()
        .call(args, &ctx(root, &token))
        .await
}

#[tokio::test]
async fn rewrites_every_file_in_one_call() {
    let (guard, root) = temp_root();
    let base = guard.path();
    std::fs::create_dir(base.join("src")).expect("mkdir");
    std::fs::write(base.join("src/a.rs"), "fn alpha() { alpha(); }\n").expect("write");
    std::fs::write(base.join("src/b.rs"), "fn beta() { alpha(); }\n").expect("write");

    let output = replace(&root, json!({"pattern": "alpha", "replacement": "omega"}))
        .await
        .expect("ok");

    assert_eq!(
        output.content,
        "rewrote 3 occurrence(s) across 2 file(s)\n  src/a.rs: 2\n  src/b.rs: 1"
    );
    assert!(!output.truncated);
    assert_eq!(
        std::fs::read_to_string(base.join("src/a.rs")).expect("read"),
        "fn omega() { omega(); }\n"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("src/b.rs")).expect("read"),
        "fn beta() { omega(); }\n"
    );
    assert_eq!(output.effects.files_written.len(), 2);
}

#[tokio::test]
async fn a_path_argument_scopes_the_walk() {
    let (guard, root) = temp_root();
    let base = guard.path();
    std::fs::create_dir(base.join("src")).expect("mkdir");
    std::fs::create_dir(base.join("docs")).expect("mkdir");
    std::fs::write(base.join("src/a.rs"), "alpha\n").expect("write");
    std::fs::write(base.join("docs/notes.md"), "alpha\n").expect("write");

    let output = replace(
        &root,
        json!({"pattern": "alpha", "replacement": "omega", "path": "src"}),
    )
    .await
    .expect("ok");

    assert_eq!(
        output.content,
        "rewrote 1 occurrence(s) across 1 file(s)\n  src/a.rs: 1"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("docs/notes.md")).expect("read"),
        "alpha\n"
    );
}

#[tokio::test]
async fn no_match_writes_nothing() {
    let (guard, root) = temp_root();
    let base = guard.path();
    std::fs::write(base.join("a.txt"), "nothing here\n").expect("write");

    let output = replace(&root, json!({"pattern": "absent", "replacement": "x"}))
        .await
        .expect("ok");

    assert_eq!(output.content, "no matches; nothing written");
    assert_eq!(
        std::fs::read_to_string(base.join("a.txt")).expect("read"),
        "nothing here\n"
    );
    assert!(output.effects.files_written.is_empty(), "{output:?}");
}

#[tokio::test]
async fn over_the_match_cap_refuses_without_writing() {
    let (guard, root) = temp_root();
    let base = guard.path();
    // One past the cap: 501 matches.
    std::fs::write(base.join("a.txt"), "x ".repeat(501)).expect("write");

    let error = replace(&root, json!({"pattern": "x", "replacement": "y"}))
        .await
        .expect_err("over cap");

    assert!(
        error.to_string().contains("more than 500 matches"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("a.txt")).expect("read"),
        "x ".repeat(501)
    );
}

#[tokio::test]
async fn a_rewrite_that_breaks_a_file_refuses_the_whole_batch() {
    let (guard, root) = temp_root();
    let base = guard.path();
    // The first file would stay valid, the second would stop parsing;
    // nothing may land.
    std::fs::write(base.join("a.rs"), "fn alpha() {}\n").expect("write");
    std::fs::write(base.join("b.rs"), "fn alpha() {}\n").expect("write");

    let error = replace(&root, json!({"pattern": "\\{\\}", "replacement": "{"}))
        .await
        .expect_err("unbalanced");

    assert!(error.to_string().contains("refusing to write"), "{error}");
    assert_eq!(
        std::fs::read_to_string(base.join("a.rs")).expect("read"),
        "fn alpha() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("b.rs")).expect("read"),
        "fn alpha() {}\n"
    );
}

#[tokio::test]
async fn unreadable_files_are_skipped_and_counted() {
    let (guard, root) = temp_root();
    let base = guard.path();
    std::fs::write(base.join("a.txt"), "alpha\n").expect("write");
    std::fs::write(base.join("bin.dat"), [0x66u8, 0x66, 0xff]).expect("write");

    let output = replace(&root, json!({"pattern": "alpha", "replacement": "omega"}))
        .await
        .expect("ok");

    assert_eq!(
        output.content,
        "rewrote 1 occurrence(s) across 1 file(s); 1 file(s) skipped \
         (unreadable, not UTF-8, or over the edit size cap)\n  a.txt: 1"
    );
}

#[tokio::test]
async fn an_escaping_path_is_refused() {
    let (_guard, root) = temp_root();
    let error = replace(
        &root,
        json!({"pattern": "a", "replacement": "b", "path": "../outside"}),
    )
    .await
    .expect_err("escape");

    assert!(error.to_string().contains("escapes"), "{error}");
}

#[tokio::test]
async fn a_missing_path_is_an_informational_answer() {
    let (_guard, root) = temp_root();
    let output = replace(
        &root,
        json!({"pattern": "a", "replacement": "b", "path": "nope"}),
    )
    .await
    .expect("ok");

    assert_eq!(output.content, "path \"nope\" does not exist");
}

#[test]
fn the_schema_and_the_parser_agree() {
    crate::schema::agreement::check::<super::ReplaceAllInput>(json!({
        "pattern": "a",
        "replacement": "b",
        "verify": "check"
    }));
}

#[tokio::test]
async fn verify_runs_the_check_in_the_same_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    std::fs::create_dir(root.join("src")).expect("src");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fusion-replace\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
    )
    .expect("Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 { 1  }\n").expect("seed");

    let output = crate::replace_all::ReplaceAllTool::default()
        .call(
            json!({
                "pattern": "1  ",
                "replacement": "1 ",
                "path": "src",
                "verify": "check"
            }),
            &ToolContext::new(
                root.clone(),
                CancellationToken::new(),
                Duration::from_secs(120),
            ),
        )
        .await
        .expect("ok");

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
    assert_eq!(output.effects.commands_run.len(), 1);
}

#[tokio::test]
async fn a_no_match_sweep_skips_verify() {
    let (_guard, root) = temp_root();
    let output = replace(
        &root,
        json!({"pattern": "zzz", "replacement": "q", "verify": "check"}),
    )
    .await
    .expect("ok");

    assert!(
        !output.content.contains("--- verify ---"),
        "{}",
        output.content
    );
    assert!(output.effects.commands_run.is_empty(), "{output:?}");
}
