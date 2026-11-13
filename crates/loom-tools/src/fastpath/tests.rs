use super::*;

// Mirrors `analyze`'s `Option<Intent>` so tests compare directly.
#[allow(clippy::unnecessary_wraps)]
fn rename(from: &str, to: &str) -> Option<Intent> {
    Some(Intent::Rename {
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

#[test]
fn analyze_rename_grammar() {
    assert_eq!(
        analyze("Rename parse_opts to parse_options"),
        rename("parse_opts", "parse_options")
    );
    assert_eq!(
        analyze("please rename the function `parse_opts` to `parse_options`."),
        rename("parse_opts", "parse_options")
    );
    assert_eq!(
        analyze("RENAME the variable count to total"),
        rename("count", "total")
    );
}

#[test]
fn analyze_rename_rejects_noise() {
    assert_eq!(
        analyze("rename parse_opts to parse_options and add tests"),
        None
    );
    assert_eq!(analyze("rename fn to match"), None);
    assert_eq!(analyze("rename parse_opts to parse_opts"), None);
    assert_eq!(analyze("renamed parse_opts to parse_options"), None);
}

#[tokio::test]
async fn decline_reasons_distinguish_no_match_from_unsolved() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_decline\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse_opts() -> u32 {\n    7\n}\n",
    )
    .expect("lib");
    let policy = CommandPolicy::cargo_only();

    // No grammar match: the decline names the absence of an intent.
    let attempt = try_fast_path(&workspace, "Refactor the parser for clarity.", &policy, &[]).await;
    assert_eq!(attempt.decline, Some("no-intent-match"));
    assert!(attempt.outcome.is_none());

    // A rename of a symbol the file does not carry: the intent
    // matched, the solver's zero-occurrence guard said no — a
    // specific reason, not the coarse fallback (roadmap item 34).
    let attempt = try_fast_path(
        &workspace,
        "Rename missing_symbol to renamed_symbol",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-rename-zero-occurrences"));

    // A dependency bump for a crate the manifest does not carry.
    let attempt = try_fast_path(
        &workspace,
        "Update the dependency tiny-dep to 2.0.0",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-dependency-unsolved"));
}

#[tokio::test]
async fn rename_declines_classify_their_guard() {
    // Roadmap item 34's precondition: the coarse
    // `intent-rename-unsolved` splits into per-cause reasons so the
    // LSP-rename revisit trigger (target-name collision declines)
    // is measurable from recorded events.
    let policy = CommandPolicy::cargo_only();

    // Collision inside a touched file: a blind replace would
    // shadow the existing target name.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_decline_cls\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse_opts() -> u32 {\n    7\n}\npub fn parse_options() -> u32 {\n    8\n}\n",
    )
    .expect("lib");
    let attempt = try_fast_path(
        &workspace,
        "Rename parse_opts to parse_options",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-rename-collision"));

    // Collision only in an *untouched* workspace file: the
    // second-pass whole-workspace guard catches it.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_decline_far\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse_opts() -> u32 {\n    7\n}\n",
    )
    .expect("lib");
    std::fs::write(
        workspace.join("src/other.rs"),
        "pub fn parse_options() -> u32 {\n    9\n}\n",
    )
    .expect("other");
    let attempt = try_fast_path(
        &workspace,
        "Rename parse_opts to parse_options",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-rename-collision"));

    // More than 500 occurrences: above the mechanical cap.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_decline_cap\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    let mut heavy = String::from("// occurrences of the word below:\n");
    for _ in 0..501 {
        heavy.push_str("// aaa aaa\n");
    }
    std::fs::write(workspace.join("src/lib.rs"), heavy).expect("lib");
    let attempt = try_fast_path(&workspace, "Rename aaa to bbb", &policy, &[]).await;
    assert_eq!(attempt.decline, Some("intent-rename-too-many-occurrences"));

    // A pre-existing broken crate: the rewrite itself is sound,
    // but `cargo check` cannot go green, so the attempt reverts
    // and names the verifier (the workspace stays byte-identical).
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_decline_ver\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    let broken = "pub fn parse_opts( {\n";
    std::fs::write(workspace.join("src/lib.rs"), broken).expect("lib");
    let attempt = try_fast_path(
        &workspace,
        "Rename parse_opts to parse_options",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-rename-verify-failed"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/lib.rs")).expect("lib"),
        broken,
        "a declined fast path restores byte-for-byte"
    );
}

#[tokio::test]
async fn npm_dependency_update_declines_to_the_agent_loop() {
    // Only Cargo has an offline manifest-only verifier
    // (`cargo metadata --no-deps --offline`). npm's lock-only mode
    // still resolves against the registry or cache (probed
    // 2026-10-24), so a package.json workspace must decline the
    // dependency intent rather than guess.
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("dirs");
    std::fs::write(
        workspace.join("package.json"),
        "{\"name\":\"fp_npm\",\"dependencies\":{\"lodash\":\"^4.17.20\"}}\n",
    )
    .expect("manifest");
    let policy = CommandPolicy::cargo_only();

    let attempt = try_fast_path(
        &workspace,
        "Update the dependency lodash to 4.17.21",
        &policy,
        &[],
    )
    .await;
    assert_eq!(attempt.decline, Some("intent-dependency-unsolved"));
    assert!(attempt.outcome.is_none());
    // Nothing was touched: the manifest still pins the old range.
    let text = std::fs::read_to_string(workspace.join("package.json")).expect("manifest");
    assert!(text.contains("^4.17.20"));
}

#[test]
fn analyze_constant_grammar() {
    assert_eq!(
        analyze("Change MAX_RETRIES from 3 to 5"),
        Some(Intent::Constant {
            name: "MAX_RETRIES".to_owned(),
            from: Some(Literal::Int("3".to_owned())),
            to: Literal::Int("5".to_owned()),
        })
    );
    assert_eq!(
        analyze("please set the constant GREETING to \"hi\"."),
        Some(Intent::Constant {
            name: "GREETING".to_owned(),
            from: None,
            to: Literal::Str("hi".to_owned()),
        })
    );
    assert_eq!(
        analyze("set ENABLED to true"),
        Some(Intent::Constant {
            name: "ENABLED".to_owned(),
            from: None,
            to: Literal::Bool(true),
        })
    );
    assert_eq!(
        analyze("change OFFSET from -1 to -2"),
        Some(Intent::Constant {
            name: "OFFSET".to_owned(),
            from: Some(Literal::Int("-1".to_owned())),
            to: Literal::Int("-2".to_owned()),
        })
    );
}

#[test]
fn analyze_constant_rejects_noise() {
    assert_eq!(analyze("change everything from broken to working"), None);
    assert_eq!(analyze("set the table for dinner"), None);
    assert_eq!(
        analyze("change MAX_RETRIES from 3 to 5 and update docs"),
        None
    );
}

#[test]
fn analyze_format_grammar() {
    assert_eq!(analyze("run rustfmt"), Some(Intent::FormatRust));
    assert_eq!(analyze("cargo fmt"), Some(Intent::FormatRust));
    assert_eq!(analyze("Please format the codebase."), Some(Intent::Format));
    assert_eq!(analyze("run gofmt"), Some(Intent::FormatGo));
    assert_eq!(analyze("format the go code"), Some(Intent::FormatGo));
    assert_eq!(analyze("format the disk"), None);
}

// Mirrors `analyze`'s `Option<Intent>` so tests compare directly.
#[allow(clippy::unnecessary_wraps)]
fn dependency(name: &str, version: &str) -> Option<Intent> {
    Some(Intent::DependencyUpdate {
        crate_name: name.to_owned(),
        to_version: version.to_owned(),
    })
}

#[test]
fn analyze_dependency_grammar() {
    assert_eq!(
        analyze("update serde to 1.0.300"),
        dependency("serde", "1.0.300")
    );
    assert_eq!(
        analyze("please update the dependency serde to 1.0.300."),
        dependency("serde", "1.0.300")
    );
    assert_eq!(
        analyze("Update crate tiny-dep to 2.0.0"),
        dependency("tiny-dep", "2.0.0")
    );
    assert_eq!(analyze("bump serde to 1"), dependency("serde", "1"));
    assert_eq!(
        analyze("bump the dependency serde to 1.0.0-beta.1"),
        dependency("serde", "1.0.0-beta.1")
    );
}

#[test]
fn analyze_dependency_rejects_noise() {
    assert_eq!(analyze("update serde"), None);
    assert_eq!(analyze("update serde to latest"), None);
    assert_eq!(analyze("update serde to *"), None);
    assert_eq!(
        analyze("update serde to 1.0.300 and regenerate the lock"),
        None
    );
    assert_eq!(analyze("bump the major version"), None);
    assert_eq!(analyze("updated serde to 1.0.300"), None);
    // "everything" parses as a crate token (shape-valid); with no
    // such dependency the apply layer declines. Grammar tests assert
    // shape, apply tests assert the decline.
    assert_eq!(
        analyze("update everything to 2"),
        dependency("everything", "2")
    );
}

#[test]
fn edit_manifest_rewrites_only_dependency_tables() {
    let manifest = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
            [dependencies]\nserde = \"1.0.100\"\ntiny-dep = \"1\"\n\n\
            [dev-dependencies]\nserde = \"1.0.100\"\n\n\
            [build-dependencies]\nserde = \"1.0.100\"\n\n\
            [features]\nserde = [\"full\"]\n";
    let (out, changes) = edit_manifest(manifest, "serde", "1.0.300");
    assert_eq!(changes.len(), 3);
    assert!(
        changes
            .iter()
            .all(|(old, new)| old == "1.0.100" && new == "1.0.300")
    );
    assert_eq!(out.matches("serde = \"1.0.300\"").count(), 3);
    assert_eq!(out.matches("serde = \"1.0.100\"").count(), 0);
    // Non-dependency keys survive untouched.
    assert!(out.contains("version = \"0.1.0\""));
    assert!(out.contains("serde = [\"full\"]"));
    assert!(out.contains("tiny-dep = \"1\""));
}

#[test]
fn edit_manifest_leaves_complex_entries_and_reports_no_changes() {
    let manifest = "[dependencies]\nserde = { version = \"1.0.100\", features = [\"derive\"] }\n\
            other.workspace = true\nother.default-features = false\n";
    let (out, changes) = edit_manifest(manifest, "serde", "1.0.300");
    assert_eq!(changes.len(), 0);
    assert_eq!(out, manifest);

    let (out, changes) = edit_manifest(manifest, "other", "2");
    assert_eq!(changes.len(), 0);
    assert_eq!(out, manifest);

    // A plain entry elsewhere under the same name does not leak in.
    let (out, changes) = edit_manifest(manifest, "version", "9");
    assert_eq!(changes.len(), 0);
    assert_eq!(out, manifest);
}

#[test]
fn edit_manifest_preserves_indent_and_final_newline() {
    let manifest = "[dependencies]\n    serde = \"1.0.100\"\n";
    let (out, changes) = edit_manifest(manifest, "serde", "2");
    assert_eq!(changes, vec![("1.0.100".to_owned(), "2".to_owned())]);
    assert_eq!(out, "[dependencies]\n    serde = \"2\"\n");

    let manifest = "[dependencies]\nserde = \"1.0.100\"";
    let (out, _) = edit_manifest(manifest, "serde", "2");
    assert_eq!(out, "[dependencies]\nserde = \"2\"");
}

#[test]
fn rewrite_dep_line_is_strict_about_shapes() {
    assert_eq!(
        rewrite_dep_line("serde = \"1.0.100\"", "serde", "2"),
        Some(("1.0.100".to_owned(), "serde = \"2\"".to_owned()))
    );
    // Prefix-similar crate names are whole tokens, not substrings.
    assert_eq!(rewrite_dep_line("serde_extra = \"1\"", "serde", "2"), None);
    assert_eq!(rewrite_dep_line("serde2 = \"1\"", "serde", "2"), None);
    // Non-plain values decline.
    assert_eq!(
        rewrite_dep_line("serde = { version = \"1\" }", "serde", "2"),
        None
    );
    assert_eq!(
        rewrite_dep_line("serde = { path = \"..\" }", "serde", "2"),
        None
    );
    assert_eq!(rewrite_dep_line("serde = \"*\"", "serde", "2"), None);
    assert_eq!(rewrite_dep_line("serde = \"latest\"", "serde", "2"), None);
}

#[test]
fn replace_word_respects_boundaries() {
    let (text, count) = replace_word("let a = 1; let ab = 2; // alpha", "a", "z");
    assert_eq!(count, 1);
    assert_eq!(text, "let z = 1; let ab = 2; // alpha");
    let (text, count) = replace_word(
        "parse_opts(); parse_opts_x(); parse_opts()",
        "parse_opts",
        "new_name",
    );
    assert_eq!(count, 2);
    assert_eq!(text, "new_name(); parse_opts_x(); new_name()");
}

#[test]
fn literal_round_trip() {
    assert_eq!(
        parse_literal("42;"),
        Some((Literal::Int("42".to_owned()), ";"))
    );
    assert_eq!(
        parse_literal("\"hi\" rest"),
        Some((Literal::Str("hi".to_owned()), " rest"))
    );
    assert_eq!(parse_literal("true,"), Some((Literal::Bool(true), ",")));
    assert_eq!(parse_literal("truely"), None);
    assert_eq!(parse_literal("\"esc\\\"aped\""), None);
}

#[test]
fn const_on_line_finds_the_literal() {
    let line = "pub const MAX_RETRIES: u32 = 3;";
    let (start, end, literal) = const_on_line(line, "MAX_RETRIES").unwrap();
    assert_eq!(&line[start..end], "3");
    assert_eq!(literal, Literal::Int("3".to_owned()));
    assert!(const_on_line(line, "OTHER").is_none());
    assert!(const_on_line("const fn helper() {}", "helper").is_none());
    let line = "    const GREETING: &str = \"hello\";";
    let (start, end, literal) = const_on_line(line, "GREETING").unwrap();
    assert_eq!(&line[start..end], "\"hello\"");
    assert_eq!(literal, Literal::Str("hello".to_owned()));
}
