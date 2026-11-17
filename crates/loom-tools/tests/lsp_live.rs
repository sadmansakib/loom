//! Live LSP smoke tests against real language servers.
//!
//! Every test is `#[ignore]`d, so `cargo test` never spawns a real
//! server. Run on a machine with servers installed:
//!
//! ```sh
//! cargo test -p loom-tools --test lsp_live -- --ignored --test-threads=1
//! ```
//!
//! Each test returns early (a skip, not a failure) when its server
//! binary is not on PATH. TypeScript additionally needs a TypeScript
//! 5.x install the server can find (a workspace `node_modules/typescript`
//! or a global one). The zsh test also covers suffix-based auto-attach:
//! `shell-lsp` has no marker file and is discovered from `*.zsh` alone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use loom_kernel::tool::{Tool, ToolContext};
use loom_tools::lsp::{LspAttachment, attach_lsp};
use tokio_util::sync::CancellationToken;

/// Per-call patience: first-run indexing can take tens of seconds.
const DEADLINE: Duration = Duration::from_secs(60);

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-lsp-live-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn on_path(binary: &str) -> bool {
    std::env::var("PATH").is_ok_and(|path| {
        path.split(':')
            .any(|dir| Path::new(dir).join(binary).is_file())
    })
}

fn skip_unless(binary: &str) -> bool {
    if on_path(binary) {
        true
    } else {
        eprintln!("skipping: {binary} not on PATH");
        false
    }
}

async fn attach(dir: &Path) -> LspAttachment {
    let attachment = attach_lsp(dir).await;
    assert!(
        !attachment.servers.is_empty(),
        "no server attached; menu: {:?}",
        attachment.menu
    );
    attachment
}

/// Calls one tool until `accept` matches (servers index asynchronously).
async fn until(
    tools: &[Box<dyn Tool>],
    dir: &Path,
    name: &str,
    args: serde_json::Value,
    accept: impl Fn(&str) -> bool,
) -> String {
    let tool = tools
        .iter()
        .find(|t| t.spec().name == name)
        .unwrap_or_else(|| panic!("{name} not attached"));
    let ctx = ToolContext::new(dir.to_path_buf(), CancellationToken::new(), DEADLINE);
    let deadline = Instant::now() + DEADLINE;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = tool
            .call(args.clone(), &ctx)
            .await
            .expect("tool call")
            .content;
        if accept(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} never matched within {DEADLINE:?}; last:\n{last}");
}

#[tokio::test]
#[ignore = "live: requires rust-analyzer on PATH"]
async fn rust_definition_and_diagnostics() {
    if !skip_unless("rust-analyzer") {
        return;
    }
    let dir = workspace("rust");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"live\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn helper(value: i64) -> i64 {\n    value + 1\n}\n\npub fn caller() -> i64 {\n    helper(1)\n}\n\npub fn broken() -> i64 {\n    \"not an int\"\n}\n",
    )
    .unwrap();
    let attachment = attach(&dir).await;
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "src/lib.rs", "symbol": "helper"}),
        |out| out.contains("src/lib.rs"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "src/lib.rs"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires gopls and go on PATH"]
async fn go_definition_and_diagnostics() {
    if !skip_unless("gopls") || !skip_unless("go") {
        return;
    }
    let dir = workspace("go");
    std::fs::write(dir.join("go.mod"), "module live\n\ngo 1.21\n").unwrap();
    std::fs::write(
        dir.join("main.go"),
        "package main\n\nfunc helper(v int) int { return v + 1 }\n\nfunc main() { _ = helper(1) }\n\nvar bad int = \"no\"\n",
    )
    .unwrap();
    let attachment = attach(&dir).await;
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "main.go", "symbol": "helper"}),
        |out| out.contains("main.go"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "main.go"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires clangd on PATH"]
async fn c_definition_and_diagnostics() {
    if !skip_unless("clangd") {
        return;
    }
    let dir = workspace("c");
    std::fs::write(
        dir.join("main.c"),
        "int helper(int value) { return value + 1; }\n\nint broken(void) { return \"no\"; }\n\nint main(void) { return helper(1); }\n",
    )
    .unwrap();
    let compile_commands = serde_json::json!([{
        "directory": dir,
        "file": "main.c",
        "command": "clang -c main.c"
    }]);
    std::fs::write(
        dir.join("compile_commands.json"),
        serde_json::to_string(&compile_commands).unwrap(),
    )
    .unwrap();
    let attachment = attach(&dir).await;
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "main.c", "symbol": "helper"}),
        |out| out.contains("main.c"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "main.c"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires typescript-language-server on PATH and a TypeScript 5.x install"]
async fn typescript_definition_and_diagnostics() {
    if !skip_unless("typescript-language-server") {
        return;
    }
    let dir = workspace("typescript");
    std::fs::write(dir.join("package.json"), "{}\n").unwrap();
    std::fs::write(
        dir.join("tsconfig.json"),
        "{\"compilerOptions\": {\"strict\": true, \"noEmit\": true}}\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/index.ts"),
        "export function helper(value: number): number {\n  return value + 1;\n}\n\nexport const bad: number = \"no\";\n",
    )
    .unwrap();
    let attachment = attach_lsp(&dir).await;
    if attachment.servers.is_empty() {
        eprintln!(
            "skipping: typescript-language-server did not attach (needs a TypeScript 5.x install); menu: {:?}",
            attachment.menu
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "src/index.ts", "symbol": "helper"}),
        |out| out.contains("index.ts"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "src/index.ts"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires pyright-langserver on PATH"]
async fn python_definition_and_diagnostics() {
    if !skip_unless("pyright-langserver") {
        return;
    }
    let dir = workspace("python");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"live\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.py"),
        "def helper(value: int) -> int:\n    return value + 1\n\n\ndef caller() -> int:\n    return helper(1)\n\n\nbad: int = \"no\"\n",
    )
    .unwrap();
    let attachment = attach(&dir).await;
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "main.py", "symbol": "helper"}),
        |out| out.contains("main.py"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "main.py"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires bash-language-server on PATH"]
async fn bash_definition_from_call_site() {
    if !skip_unless("bash-language-server") {
        return;
    }
    let dir = workspace("bash");
    std::fs::write(
        dir.join("script.sh"),
        "#!/usr/bin/env bash\nhelper() { echo \"hi $1\"; }\ncaller() { helper world; }\ncaller\n",
    )
    .unwrap();
    let attachment = attach(&dir).await;
    // No diagnostics assertion: bash-language-server reports ShellCheck
    // findings only, and ShellCheck is not part of loom's toolchain.
    let definition = until(
        &attachment.tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "script.sh", "line": 3, "column": 12}),
        |out| out.contains("script.sh:2"),
    )
    .await;
    assert!(definition.contains("script.sh:2"), "{definition}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live: requires shell-lsp on PATH"]
async fn zsh_definition_and_diagnostics() {
    if !skip_unless("shell-lsp") {
        return;
    }
    let dir = workspace("zsh");
    std::fs::write(
        dir.join("script.zsh"),
        "#!/usr/bin/env zsh\nhelper() {\n  print -r -- \"$1\"\n}\ncaller() {\n  helper world\n}\ncaller\nif [[ -n $USER ]]; then\n",
    )
    .unwrap();
    let attachment = attach(&dir).await;
    let tools = &attachment.tools;
    let definition = until(
        tools,
        &dir,
        "lsp_definition",
        serde_json::json!({"path": "script.zsh", "symbol": "helper"}),
        |out| out.contains("script.zsh"),
    )
    .await;
    assert!(definition.contains("definition:"), "{definition}");
    let diagnostics = until(
        tools,
        &dir,
        "lsp_diagnostics",
        serde_json::json!({"path": "script.zsh"}),
        |out| out.contains("error"),
    )
    .await;
    assert!(diagnostics.contains("error"), "{diagnostics}");
    let _ = std::fs::remove_dir_all(&dir);
}
