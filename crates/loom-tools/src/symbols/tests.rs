use super::*;
use loom_kernel::tool::{Tool, ToolContext};

fn ctx_for(root: &std::path::Path) -> ToolContext {
    ToolContext::new(
        root.to_owned(),
        tokio_util::sync::CancellationToken::new(),
        std::time::Duration::from_secs(30),
    )
}

fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    (dir, root)
}

async fn call_overview(root: &std::path::Path, input: serde_json::Value) -> String {
    SymbolsOverviewTool::default()
        .call(input, &ctx_for(root))
        .await
        .expect("overview succeeds")
        .content
}

async fn call_replace(root: &std::path::Path, input: serde_json::Value) -> String {
    ReplaceSymbolTool::default()
        .call(input, &ctx_for(root))
        .await
        .expect("replace succeeds")
        .content
}

fn err_of(root: &std::path::Path, input: serde_json::Value) -> String {
    futures::executor::block_on(ReplaceSymbolTool::default().call(input, &ctx_for(root)))
        .expect_err("replace must fail")
        .to_string()
}

// ---- name-path semantics ----

#[test]
fn name_path_matches_suffix_and_absolute() {
    assert!(name_path_matches("method", "Class/method"));
    assert!(name_path_matches("Class/method", "a/b/Class/method"));
    assert!(name_path_matches("Class/method", "Class/method"));
    assert!(!name_path_matches("Class", "Class/method"));
    assert!(!name_path_matches("method", "Class/other"));
    assert!(name_path_matches("/Class/method", "Class/method"));
    assert!(!name_path_matches("/Class/method", "a/Class/method"));
}

// ---- Rust ----

const RUST_SRC: &str = r#"//! module docs
use std::fmt;

const LIMIT: usize = 10;

pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name }
    }

    fn helper(&self) -> usize {
        LIMIT
    }
}

pub fn main() {
    let inner = || 1;
    let _ = inner();
}
"#;

#[tokio::test]
async fn rust_overview_lists_top_level_and_children() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "lib.rs", "depth": 1})).await;
    assert!(out.contains("const LIMIT (L4-L4)"), "{out}");
    assert!(out.contains("struct Config (L6-L8)"), "{out}");
    assert!(out.contains("impl Config (L10-L18)"), "{out}");
    assert!(out.contains("function Config/new (L11-L13)"), "{out}");
    assert!(out.contains("function main (L20-L23)"), "{out}");
    drop(dir);
}

#[tokio::test]
async fn rust_overview_depth_zero_counts_children() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "lib.rs"})).await;
    assert!(out.contains("impl Config (L10-L18) [2 children"), "{out}");
    assert!(!out.contains("Config/new"), "{out}");
    drop(dir);
}

#[tokio::test]
async fn rust_replace_function_and_impl_method() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
    call_replace(
        &root,
        serde_json::json!({
            "path": "lib.rs",
            "name_path": "Config/helper",
            "body": "    fn helper(&self) -> usize {\n        LIMIT + 1\n    }"
        }),
    )
    .await;
    let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
    assert!(text.contains("LIMIT + 1"), "{text}");
    assert!(text.contains("pub fn new"), "sibling untouched");
    assert_eq!(
        text.matches("fn helper").count(),
        1,
        "one definition remains"
    );

    // Top-level by bare name.
    call_replace(
        &root,
        serde_json::json!({
            "path": "lib.rs",
            "name_path": "main",
            "body": "pub fn main() {\n    println!(\"replaced\");\n}"
        }),
    )
    .await;
    let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
    assert!(text.contains("replaced"), "{text}");
    drop(dir);
}

#[tokio::test]
async fn rust_ambiguous_and_missing_match_errors() {
    let (dir, root) = temp_root();
    let src = "fn dup() {}\nmod a {\n    fn dup() {}\n}\n";
    std::fs::write(root.join("lib.rs"), src).expect("seed");
    let err = err_of(
        &root,
        serde_json::json!({"path": "lib.rs", "name_path": "dup", "body": "fn dup() {}"}),
    );
    assert!(err.contains("matches 2 symbols"), "{err}");
    assert!(err.contains("a/dup"), "{err}");

    let err = err_of(
        &root,
        serde_json::json!({"path": "lib.rs", "name_path": "nope", "body": "fn nope() {}"}),
    );
    assert!(err.contains("no symbol matches"), "{err}");
    assert!(err.contains("dup"), "{err}");
    assert!(err.contains("a"), "{err}");
    drop(dir);
}

#[tokio::test]
async fn rust_absolute_path_anchors_top_level() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
    // `new` alone resolves; `/new` must not (it lives under Config).
    let err = err_of(
        &root,
        serde_json::json!({"path": "lib.rs", "name_path": "/new", "body": "fn new() {}"}),
    );
    assert!(err.contains("no symbol matches"), "{err}");
    drop(dir);
}

// ---- Python ----

const PYTHON_SRC: &str = r#"
import os


def top_level(x):
    return x


class Greeter:
    """A greeter."""

    def greet(self):
        def nested():
            return "hi"
        return nested()

    @property
    def name(self):
        return "g"


@decorator
def decorated():
    return 1
"#;

#[tokio::test]
async fn python_overview_and_nested_defs() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "app.py", "depth": 2})).await;
    assert!(out.contains("function top_level (L5-L6)"), "{out}");
    assert!(out.contains("class Greeter (L9-L19)"), "{out}");
    assert!(out.contains("function Greeter/greet (L12-L15)"), "{out}");
    assert!(
        out.contains("function Greeter/greet/nested (L13-L14)"),
        "{out}"
    );
    drop(dir);
}

#[tokio::test]
async fn python_replace_includes_decorators_in_span() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
    call_replace(
        &root,
        serde_json::json!({
            "path": "app.py",
            "name_path": "Greeter/name",
            "body": "    @property\n    def name(self):\n        return \"ga\""
        }),
    )
    .await;
    let text = std::fs::read_to_string(root.join("app.py")).expect("read");
    assert!(text.contains("\"ga\""), "{text}");
    assert_eq!(
        text.matches("@property").count(),
        1,
        "one decorator remains"
    );
    drop(dir);
}

#[tokio::test]
async fn python_replacing_decorated_symbol_requires_body_with_decorator() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
    // Body without the decorator: the old decorator (inside the
    // replaced span) disappears with it — the model's body is the
    // whole definition.
    call_replace(
        &root,
        serde_json::json!({
            "path": "app.py",
            "name_path": "decorated",
            "body": "def decorated():\n    return 2"
        }),
    )
    .await;
    let text = std::fs::read_to_string(root.join("app.py")).expect("read");
    assert!(text.contains("return 2"), "{text}");
    assert!(
        !text.contains("@decorator"),
        "decorator was part of the span"
    );
    drop(dir);
}

// ---- TypeScript / TSX ----

const TS_SRC: &str = r#"
export interface Point {
    x: number;
    y: number;
}

export type Id = string;

export enum Color {
    Red,
    Green,
}

export class Canvas {
    draw(p: Point): void {
        const helper = () => 1;
        helper();
    }

    get size(): number {
        return 0;
    }
}

export function render(id: Id): string {
    return id;
}

const makePoint = (x: number, y: number): Point => ({ x, y });

let plain = 3;
"#;

#[tokio::test]
async fn typescript_overview_kinds() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("canvas.ts"), TS_SRC).expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "canvas.ts", "depth": 1})).await;
    assert!(out.contains("interface Point (L2-L5)"), "{out}");
    assert!(out.contains("type Id (L7-L7)"), "{out}");
    assert!(out.contains("enum Color (L9-L12)"), "{out}");
    assert!(out.contains("class Canvas (L14-L23)"), "{out}");
    assert!(out.contains("method Canvas/draw (L15-L18)"), "{out}");
    assert!(out.contains("method Canvas/size (L20-L22)"), "{out}");
    assert!(out.contains("function render (L25-L27)"), "{out}");
    assert!(out.contains("function makePoint (L29-L29)"), "{out}");
    assert!(!out.contains("plain"), "plain variables are not symbols");
    drop(dir);
}

#[tokio::test]
async fn typescript_replace_arrow_function_symbol() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("canvas.ts"), TS_SRC).expect("seed");
    call_replace(
        &root,
        serde_json::json!({
            "path": "canvas.ts",
            "name_path": "makePoint",
            "body": "const makePoint = (x: number, y: number): Point => ({ x, y, z: 0 } as Point);"
        }),
    )
    .await;
    let text = std::fs::read_to_string(root.join("canvas.ts")).expect("read");
    assert!(text.contains("z: 0"), "{text}");
    drop(dir);
}

#[tokio::test]
async fn tsx_file_dispatches_to_tsx_grammar() {
    let (dir, root) = temp_root();
    std::fs::write(
        root.join("widget.tsx"),
        "export function App() {\n    return <div>hi</div>;\n}\n",
    )
    .expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "widget.tsx"})).await;
    assert!(out.contains("function App (L1-L3)"), "{out}");
    drop(dir);
}

// ---- shared behavior ----

#[tokio::test]
async fn unsupported_extension_and_missing_file_error() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("x.go"), "package x\n").expect("seed");
    let err = err_of(
        &root,
        serde_json::json!({"path": "x.go", "name_path": "x", "body": "y"}),
    );
    assert!(err.contains(".rs, .py"), "{err}");

    let err = err_of(
        &root,
        serde_json::json!({"path": "nope.rs", "name_path": "x", "body": "y"}),
    );
    assert!(err.contains("does not exist"), "{err}");
    drop(dir);
}

#[tokio::test]
async fn replacement_that_breaks_parsing_is_refused() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("lib.rs"), "fn ok() {}\n").expect("seed");
    let err = err_of(
        &root,
        serde_json::json!({"path": "lib.rs", "name_path": "ok", "body": "fn broken( {"}),
    );
    assert!(err.contains("refusing to write"), "{err}");
    // Nothing reached the disk.
    let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
    assert_eq!(text, "fn ok() {}\n");
    drop(dir);
}

#[tokio::test]
async fn confinement_escape_is_refused() {
    let (dir, root) = temp_root();
    std::fs::write(root.join("inside.rs"), "fn a() {}\n").expect("seed");
    let err = err_of(
        &root,
        serde_json::json!({"path": "../outside.rs", "name_path": "a", "body": "fn a() {}"}),
    );
    assert!(err.to_lowercase().contains("workspace"), "{err}");
    drop(dir);
}

#[tokio::test]
async fn overview_falls_back_to_kind_counts_on_huge_listings() {
    let (dir, root) = temp_root();
    // 6_000 functions: each line ~30 bytes → ~180 KB, far over the
    // 8 KB budget; the fallback keeps the response bounded.
    let mut src = String::new();
    for i in 0..6_000 {
        src.push_str(&format!("fn f{i}() {{}}\n"));
    }
    std::fs::write(root.join("big.rs"), src).expect("seed");
    let out = call_overview(&root, serde_json::json!({"path": "big.rs"})).await;
    assert!(out.contains("too many symbols to list"), "{out}");
    assert!(out.contains("functions: 6000"), "{out}");
    drop(dir);
}

#[tokio::test]
async fn overview_reports_read_touch_replace_reports_write_touch() {
    let read_touch = SymbolsOverviewTool::default()
        .touched_paths(&serde_json::json!({"path": "lib.rs"}))
        .expect("touches");
    assert_eq!(read_touch.len(), 1);
    assert!(matches!(
        read_touch[0].access,
        loom_kernel::tool::PathAccess::Read
    ));
    let write_touch = ReplaceSymbolTool::default()
        .touched_paths(&serde_json::json!({"path": "lib.rs", "name_path": "a", "body": "b"}))
        .expect("touches");
    assert!(matches!(
        write_touch[0].access,
        loom_kernel::tool::PathAccess::Write
    ));
}
