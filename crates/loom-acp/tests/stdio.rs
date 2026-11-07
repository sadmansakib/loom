//! Stdio smoke test: the real `loom-acp` binary over real pipes.
//!
//! Asserts the property the ACP spec mandates and editors depend on:
//! every byte the process writes to stdout is a valid JSON-RPC
//! message (one per line), and a clean stdin close shuts the process
//! down. No credentials are involved — no `session/new` is sent, so
//! the engine never opens.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn stdout_carries_only_jsonrpc_and_shutdown_is_clean() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-acp"))
        .arg("--provider")
        .arg("deepseek")
        .arg("--model")
        .arg("deepseek-chat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn loom-acp");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let initialize =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
    stdin
        .write_all(initialize.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .expect("send initialize");

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).expect("read a stdout line");
    assert!(bytes > 0, "the agent must answer initialize on stdout");
    let reply: serde_json::Value = serde_json::from_str(line.trim())
        .expect("stdout carries only JSON-RPC: every line must parse");
    assert_eq!(reply.get("id").and_then(|id| id.as_i64()), Some(1));
    assert!(
        reply.get("result").is_some() || reply.get("error").is_some(),
        "a well-formed response or error, got {reply}"
    );

    drop(stdin);
    let status = child.wait().expect("process exits after stdin closes");
    assert!(status.success(), "clean shutdown, got {status}");
}
