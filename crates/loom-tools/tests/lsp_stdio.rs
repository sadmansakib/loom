//! LSP stdio integration tests (docs/lsp-design.md, slice E1).
//!
//! Same re-exec pattern as the MCP tests: the test binary plays the
//! mock LSP server when `LOOM_LSP_MOCK` is set, speaking real
//! Content-Length framed JSON-RPC over the inherited stdio pipes. The
//! manifest names the server `rust_analyzer` so the candidate inherits
//! the known-server suffix map (`.rs`) — manifest names override table
//! commands, which is exactly how a wrapper or pinned binary replaces a
//! discovered one.
//!
//! The mock advertises `utf-16` positions and echoes the request
//! position in its definition response, which lets the tests pin the
//! char↔utf-16 conversion end to end (a surrogate-pair emoji makes
//! pass-through bugs visible).

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_kernel::tool::{Tool, ToolContext};
use loom_tools::ToolBudgets;
use loom_tools::lsp::{LspAttachment, attach_lsp};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- mock

/// Test-entry dispatcher: no-op when not re-executed as a mock.
#[test]
fn lsp_mock_server() {
    if std::env::var("LOOM_LSP_MOCK").is_err() {
        return;
    }
    mock_server_loop();
}

/// Fixture source used by the client-side tests (and read by the mock
/// via `LOOM_LSP_WS`). Line 4 carries a surrogate-pair emoji so a
/// raw-character column and a utf-16 column differ by one.
const LIB_RS: &str = concat!(
    "//! Fixture under LSP.\n",
    "\n",
    "pub fn median(values: &[i64]) -> i64 {\n",
    "    let sorted \u{1f600}x = 1;\n",
    "    let _ = values;\n",
    "    0\n",
    "}\n",
);

fn ws_root() -> String {
    std::env::var("LOOM_LSP_WS").unwrap()
}

fn file_uri(path: &str) -> String {
    format!("file://{}", path)
}

fn write_frame(out: &mut impl Write, body: &str) {
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn send_response(out: &mut impl Write, id: serde_json::Value, result: serde_json::Value) {
    write_frame(
        out,
        &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
    );
}

fn send_notification(out: &mut impl Write, method: &str, params: serde_json::Value) {
    write_frame(
        out,
        &format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#),
    );
}

fn send_request(out: &mut impl Write, id: u64, method: &str, params: serde_json::Value) {
    write_frame(
        out,
        &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#),
    );
}

/// Reads one Content-Length framed message from stdin.
fn read_frame(stdin: &mut impl Read) -> Option<serde_json::Value> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let header = String::from_utf8_lossy(&header);
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())?;
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    v.get(key)
}

fn nested<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for key in path {
        cur = field(cur, key)?;
    }
    Some(cur)
}

/// The mock server main loop. Deterministic responses; the definition
/// handler echoes the request position so the client's encoding
/// conversion is observable.
fn mock_server_loop() -> ! {
    let no_pull = std::env::var("LOOM_LSP_NO_PULL").is_ok();
    let no_rename = std::env::var("LOOM_LSP_NO_RENAME").is_ok();
    let settings_probe = std::env::var("LOOM_LSP_SETTINGS_PROBE").is_ok();
    let mut out = std::io::stdout();
    // libtest writes its progress line without a newline; start on a
    // fresh line so the first frame stays parseable for the client.
    let _ = writeln!(out);
    let _ = out.flush();

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let Some(msg) = read_frame(&mut stdin) else {
            std::process::exit(0);
        };
        let method = field(&msg, "method").and_then(|m| m.as_str()).unwrap_or("");
        let id = field(&msg, "id").cloned();
        // Client responses to server-initiated requests: ingest, drop.
        if method.is_empty() {
            continue;
        }
        match method {
            "initialize" => {
                let mut caps = serde_json::json!({
                    "positionEncoding": "utf-16",
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "documentSymbolProvider": true,
                    "hoverProvider": true,
                    "workspaceSymbolProvider": true,
                    "documentFormattingProvider": true
                });
                if !no_pull {
                    caps["diagnosticProvider"] = serde_json::json!({"interFileDependencies": false, "workspaceDiagnostics": false});
                }
                if !no_rename {
                    caps["renameProvider"] = serde_json::json!(true);
                }
                let result = serde_json::json!({
                    "capabilities": caps,
                    "serverInfo": {"name": "loom-mock"}
                });
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "initialized" => {
                if settings_probe {
                    // E2 settings round-trip: ask the client for its
                    // configuration, read the response, and surface the
                    // served value as a pushed diagnostic on probe.rs.
                    send_request(
                        &mut out,
                        9001,
                        "workspace/configuration",
                        serde_json::json!({"items": [{"section": null}]}),
                    );
                    // Other traffic (didOpen, notifications) may
                    // interleave; wait for OUR response by id.
                    let mut served = String::from("missing");
                    while let Some(frame) = read_frame(&mut stdin) {
                        if frame.get("id").and_then(|i| i.as_u64()) == Some(9001)
                            && frame.get("result").is_some()
                        {
                            served = frame
                                .get("result")
                                .and_then(|r| r.get(0))
                                .and_then(|v| v.get("rust_probe_value"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("missing")
                                .to_string();
                            break;
                        }
                    }
                    let uri = file_uri(&format!("{}/src/probe.rs", ws_root()));
                    send_notification(
                        &mut out,
                        "textDocument/publishDiagnostics",
                        serde_json::json!({
                            "uri": uri,
                            "diagnostics": [{
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 1}
                                },
                                "severity": 2,
                                "message": format!("settings seen: {served}")
                            }]
                        }),
                    );
                    continue;
                }
                // Push one diagnostic for src/other.rs (absolute URI —
                // the handler keys the cache by file path).
                let uri = file_uri(&format!("{}/src/other.rs", ws_root()));
                send_notification(
                    &mut out,
                    "textDocument/publishDiagnostics",
                    serde_json::json!({
                        "uri": uri,
                        "diagnostics": [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1}
                            },
                            "severity": 2,
                            "message": "pushed mock diagnostic"
                        }]
                    }),
                );
            }
            "textDocument/definition" => {
                let uri = nested(&msg, &["params", "textDocument", "uri"])
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .to_string();
                let line = nested(&msg, &["params", "position", "line"])
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0);
                let character = nested(&msg, &["params", "position", "character"])
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0);
                let pos = serde_json::json!({"line": line, "character": character});
                let result = serde_json::json!([{
                    "uri": uri,
                    "range": {"start": pos, "end": pos}
                }]);
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/references" => {
                let uri = nested(&msg, &["params", "textDocument", "uri"])
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .to_string();
                let other = file_uri(&format!("{}/src/other.rs", ws_root()));
                let result = serde_json::json!([
                    {"uri": uri, "range": {"start": {"line": 7, "character": 0}, "end": {"line": 7, "character": 3}}},
                    {"uri": other, "range": {"start": {"line": 9, "character": 2}, "end": {"line": 9, "character": 5}}}
                ]);
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/documentSymbol" => {
                let result = serde_json::json!([{
                    "name": "median",
                    "kind": 12,
                    "range": {"start": {"line": 2, "character": 0}, "end": {"line": 6, "character": 1}},
                    "selectionRange": {"start": {"line": 2, "character": 7}, "end": {"line": 2, "character": 13}},
                    "children": [{
                        "name": "inner",
                        "kind": 8,
                        "range": {"start": {"line": 3, "character": 4}, "end": {"line": 5, "character": 5}},
                        "selectionRange": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                        "children": []
                    }]
                }]);
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "workspace/symbol" => {
                let uri = file_uri(&format!("{}/src/lib.rs", ws_root()));
                let result = serde_json::json!([{
                    "name": "median",
                    "kind": 12,
                    "location": {
                        "uri": uri,
                        "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 10}}
                    },
                    "containerName": null
                }]);
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/hover" => {
                let result = serde_json::json!({
                    "contents": {"kind": "markdown", "value": "fn median(&[i64]) -> i64"}
                });
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/diagnostic" => {
                let result = serde_json::json!({
                    "kind": "full",
                    "items": [{
                        "range": {
                            "start": {"line": 4, "character": 0},
                            "end": {"line": 4, "character": 5}
                        },
                        "severity": 1,
                        "message": "mock error: unused variable"
                    }]
                });
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/rename" => {
                // One text edit (median→mean on lib.rs line 3, pinned to
                // open version 1) plus a file move: exercises both E2
                // paths through loom's write machinery.
                let lib = file_uri(&format!("{}/src/lib.rs", ws_root()));
                let from = file_uri(&format!("{}/src/third.rs", ws_root()));
                let to = file_uri(&format!("{}/src/third_renamed.rs", ws_root()));
                let new_name = nested(&msg, &["params", "newName"])
                    .and_then(|n| n.as_str())
                    .unwrap_or("mean")
                    .to_string();
                let result = serde_json::json!({
                    "documentChanges": [
                        {
                            "textDocument": {"uri": lib, "version": 1},
                            "edits": [{
                                "range": {
                                    "start": {"line": 2, "character": 7},
                                    "end": {"line": 2, "character": 13}
                                },
                                "newText": new_name
                            }]
                        },
                        {"kind": "rename", "oldUri": from, "newUri": to}
                    ]
                });
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "textDocument/formatting" => {
                // Replace the whole first line of third.rs (ASCII, so
                // utf-16 columns == bytes) with trimmed text.
                let result = serde_json::json!([{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 1, "character": 0}
                    },
                    "newText": "fn third() {}\n"
                }]);
                if let Some(id) = id {
                    send_response(&mut out, id, result);
                }
            }
            "shutdown" => {
                if let Some(id) = id {
                    send_response(&mut out, id, serde_json::Value::Null);
                }
                std::process::exit(0);
            }
            _ => {
                if let Some(id) = id {
                    send_response(&mut out, id, serde_json::Value::Null);
                }
            }
        }
    }
}

// -------------------------------------------------------------- harness

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-lsp-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), LIB_RS).unwrap();
    std::fs::write(dir.join("src/other.rs"), "// other\n").unwrap();
    // E2 fixtures: third.rs carries trailing whitespace for the format
    // test and gets moved by the mock's rename response; probe.rs is
    // the settings round-trip target.
    std::fs::write(dir.join("src/third.rs"), "fn third() {   }   \n").unwrap();
    std::fs::write(dir.join("src/probe.rs"), "// probe\n").unwrap();
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
    dir
}

fn mock_command_lines(exe: &str) -> String {
    format!("[{exe:?}, \"--exact\", \"lsp_mock_server\", \"--test-threads=1\", \"--nocapture\"]")
}

fn manifest(dir: &Path, extra_env: &[(&str, &str)]) {
    manifest_with(dir, extra_env, "");
}

/// Manifest writer with extra `[servers.rust_analyzer]` declaration
/// lines appended (e.g. a `settings` table for the E2 probe).
fn manifest_with(dir: &Path, extra_env: &[(&str, &str)], decl_extra: &str) {
    let exe = std::env::current_exe().unwrap().display().to_string();
    let mut env_pairs = vec![
        (String::from("LOOM_LSP_MOCK"), String::from("1")),
        (String::from("LOOM_LSP_WS"), dir.display().to_string()),
    ];
    for (k, v) in extra_env {
        env_pairs.push(((*k).to_string(), (*v).to_string()));
    }
    let env_body = env_pairs
        .iter()
        .map(|(k, v)| format!("{k} = {v:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "[servers.rust_analyzer]\ncommand = {}\nenv = {{ {env_body} }}\n{decl_extra}",
        mock_command_lines(&exe)
    );
    std::fs::write(dir.join(".loom/lsp.toml"), body).unwrap();
}

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn attach(dir: &Path) -> LspAttachment {
    let rt = build_runtime();
    rt.block_on(attach_lsp(dir, ToolBudgets::default()))
}

fn context(dir: &Path) -> ToolContext {
    ToolContext::new(
        dir.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(30),
    )
}

fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|t| t.spec().name == name)
        .map(|t| t.as_ref())
        .unwrap_or_else(|| panic!("tool {name} not attached"))
}

// --------------------------------------------------------------- tests

#[test]
fn missing_manifest_attaches_nothing() {
    let dir = workspace("missing");
    // No marker files for auto-attach and no manifest.
    let _ = std::fs::remove_dir_all(dir.join("src"));
    let _ = std::fs::remove_file(dir.join("Cargo.toml"));
    std::fs::remove_dir_all(dir.join(".loom")).unwrap();
    let attachment = attach(&dir);
    assert!(attachment.tools.is_empty());
    assert!(attachment.menu.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_manifest_fails_closed() {
    let dir = workspace("invalid");
    std::fs::write(dir.join(".loom/lsp.toml"), "not [ valid toml").unwrap();
    let attachment = attach(&dir);
    assert!(attachment.tools.is_empty());
    assert!(attachment.menu.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_binary_skips_server() {
    let dir = workspace("no-binary");
    std::fs::write(
        dir.join(".loom/lsp.toml"),
        "[servers.rust_analyzer]\ncommand = [\"loom-missing-binary-9x\"]\n",
    )
    .unwrap();
    let attachment = attach(&dir);
    assert!(attachment.tools.is_empty());
    assert!(attachment.menu.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_yields_eight_tools_and_menu() {
    let dir = workspace("attach");
    manifest(&dir, &[]);
    let attachment = attach(&dir);
    let names: Vec<&str> = attachment
        .tools
        .iter()
        .map(|t| t.spec().name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "lsp_definition",
            "lsp_references",
            "lsp_symbols",
            "lsp_workspace_symbols",
            "lsp_hover",
            "lsp_diagnostics",
            "lsp_rename",
            "lsp_format",
        ]
    );
    let menu = attachment.menu.as_deref().expect("menu present");
    assert!(menu.contains("rust_analyzer (Rust)"), "menu: {menu}");
    assert!(menu.contains("lsp_definition"));
    assert!(menu.contains("lsp_rename"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn definition_via_symbol_and_utf16_echo() {
    let dir = workspace("definition");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_definition");
    let ctx = context(&dir);

    // Symbol route: "median" sits at line 3, column 8 (chars). The
    // mock echoes the position, so the rendered 1-based form pins the
    // conversion: 7 chars before the symbol = 7 utf-16 units (ASCII).
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median"}),
            &ctx,
        ))
        .expect("definition call");
    assert!(
        out.content.starts_with("[lsp:rust_analyzer]"),
        "{}",
        out.content
    );
    assert!(out.content.contains(":3:8"), "{}", out.content);

    // Surrogate pair: line 4 col 17 (chars) — 15 ASCII chars plus one
    // emoji = 17 utf-16 units before the column, so the echo renders
    // 4:18. A pass-through bug would render 4:17.
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "line": 4, "column": 17}),
            &ctx,
        ))
        .expect("definition call (line/column)");
    assert!(out.content.contains(":4:18"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn references_render_two_locations() {
    let dir = workspace("references");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_references");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median"}),
            &ctx,
        ))
        .expect("references call");
    assert!(out.content.contains(":8:1"), "{}", out.content);
    assert!(out.content.contains("other.rs"), "{}", out.content);
    assert!(out.content.contains(":10:3"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn symbols_outline_nested() {
    let dir = workspace("symbols");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_symbols");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"path": "src/lib.rs"}), &ctx))
        .expect("symbols call");
    assert!(out.content.contains("median"), "{}", out.content);
    assert!(out.content.contains("inner"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workspace_symbols_flat_response() {
    let dir = workspace("workspace-symbols");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_workspace_symbols");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"query": "median"}), &ctx))
        .expect("workspace symbols call");
    assert!(out.content.contains("median"), "{}", out.content);
    assert!(out.content.contains(":3:5"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hover_renders_markup() {
    let dir = workspace("hover");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_hover");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median"}),
            &ctx,
        ))
        .expect("hover call");
    assert!(
        out.content.contains("fn median(&[i64]) -> i64"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diagnostics_pull_preferred() {
    let dir = workspace("diagnostics-pull");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_diagnostics");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"path": "src/lib.rs"}), &ctx))
        .expect("diagnostics call");
    assert!(
        out.content
            .contains("error 5:1 mock error: unused variable"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diagnostics_fall_back_to_pushed_cache() {
    let dir = workspace("diagnostics-push");
    manifest(&dir, &[("LOOM_LSP_NO_PULL", "1")]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_diagnostics");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"path": "src/other.rs"}), &ctx))
        .expect("diagnostics call");
    assert!(
        out.content.contains("warning 1:1 pushed mock diagnostic"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unowned_suffix_is_rejected_with_guidance() {
    let dir = workspace("unowned");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_definition");
    let ctx = context(&dir);
    let err = rt
        .block_on(tool.call(serde_json::json!({"path": "data.txt", "line": 1}), &ctx))
        .expect_err("txt not owned");
    match err {
        loom_kernel::tool::ToolError::InvalidInput(msg) => {
            assert!(
                msg.contains("no attached LSP server owns data.txt"),
                "{msg}"
            );
            assert!(msg.contains("rust_analyzer"), "{msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recent_changes_sync_does_not_break_queries() {
    let dir = workspace("sync");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_definition");
    let ctx = context(&dir);
    // Simulate loom's change feed: lib.rs was rewritten (the mock
    // accepts didChange/didChangeWatchedFiles without complaint).
    ctx.recent_changes
        .lock()
        .unwrap()
        .push("src/lib.rs".to_string());
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median"}),
            &ctx,
        ))
        .expect("definition after sync");
    assert!(out.content.contains(":3:8"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_applies_edits_and_moves_files() {
    let dir = workspace("rename");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_rename");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median", "new_name": "mean"}),
            &ctx,
        ))
        .expect("rename call");
    assert!(
        out.content
            .contains("renamed symbol to mean across 2 document change(s)"),
        "{}",
        out.content
    );
    // Text edit landed through loom's write path…
    let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert!(lib.contains("pub fn mean("), "{lib}");
    assert!(!lib.contains("median"), "{lib}");
    // …and the file operation executed: third.rs → third_renamed.rs.
    assert!(dir.join("src/third_renamed.rs").is_file());
    assert!(!dir.join("src/third.rs").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_without_capability_errors() {
    let dir = workspace("rename-unsupported");
    manifest(&dir, &[("LOOM_LSP_NO_RENAME", "1")]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_rename");
    let ctx = context(&dir);
    let err = rt
        .block_on(tool.call(
            serde_json::json!({"path": "src/lib.rs", "symbol": "median", "new_name": "mean"}),
            &ctx,
        ))
        .expect_err("rename must fail without the capability");
    assert!(
        matches!(err, loom_kernel::tool::ToolError::Execution(ref m) if m.contains("rename")),
        "{err:?}"
    );
    // Nothing landed.
    let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert!(lib.contains("median"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn format_trims_trailing_whitespace() {
    let dir = workspace("format");
    manifest(&dir, &[]);
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_format");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"path": "src/third.rs"}), &ctx))
        .expect("format call");
    assert!(
        out.content.contains("formatted src/third.rs"),
        "{}",
        out.content
    );
    let third = std::fs::read_to_string(dir.join("src/third.rs")).unwrap();
    assert_eq!(third, "fn third() {}\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_reach_workspace_configuration() {
    let dir = workspace("settings");
    manifest_with(
        &dir,
        &[("LOOM_LSP_SETTINGS_PROBE", "1"), ("LOOM_LSP_NO_PULL", "1")],
        "settings = { rust_probe_value = \"probe-77\" }\n",
    );
    let rt = build_runtime();
    let attachment = rt.block_on(attach_lsp(&dir, ToolBudgets::default()));
    let tool = find_tool(&attachment.tools, "lsp_diagnostics");
    let ctx = context(&dir);
    let out = rt
        .block_on(tool.call(serde_json::json!({"path": "src/probe.rs"}), &ctx))
        .expect("diagnostics call");
    assert!(
        out.content.contains("settings seen: probe-77"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}
