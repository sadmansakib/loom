//! User-facing `loom run` journeys through the CLI, engine, provider
//! adapter, SQLite store, tools, and trace writer. The HTTP boundary is
//! local and deterministic; no live credentials or model service needed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "journey-model";
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

fn sse(records: &[Value]) -> String {
    let mut body = String::new();
    for record in records {
        body.push_str("data: ");
        body.push_str(&record.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_response(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        sse(&[
            json!({"id":"journey","choices":[{"delta":{"role":"assistant"}}]}),
            json!({"id":"journey","choices":[{"delta":{"content":text}}]}),
            json!({"id":"journey","choices":[{"delta":{},"finish_reason":"stop"}]}),
        ])
        .into_bytes(),
        "text/event-stream",
    )
}

async fn server_with_catalog() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": MODEL,
                "context_window": 131_072,
                "supported_parameters": ["tools"]
            }]
        })))
        .mount(&server)
        .await;
    server
}

fn run_command(
    server: &MockServer,
    temp: &tempfile::TempDir,
    task: &str,
    extra: &[&str],
) -> Command {
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let db = temp.path().join("loom.db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_loom"));
    command
        .arg("run")
        .args([
            "--provider",
            "custom",
            "--model",
            MODEL,
            "--base-url",
            server.uri().as_str(),
            "--models-url",
            &format!("{}/v1/models", server.uri()),
            "--workspace",
        ])
        .arg(&workspace)
        .args(["--json"])
        .args(extra)
        .arg(task)
        .env("LOOM_DB", db)
        .env("CUSTOM_API_KEY", "integration-test-key")
        .env("LOOM_BARE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn bounded_output(command: &mut Command) -> std::process::Output {
    timeout(CHILD_TIMEOUT, command.output())
        .await
        .expect("loom subprocess exceeded the 30-second test deadline")
        .expect("spawn loom subprocess")
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("CLI stdout is UTF-8")
}

fn summary(output: &std::process::Output) -> Value {
    stdout_text(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["type"] == "summary")
        .unwrap_or_else(|| panic!("missing summary in stdout: {}", stdout_text(output)))
}

#[tokio::test]
async fn cli_run_executes_tool_exports_trace_and_resumes_persisted_session() {
    let server = server_with_catalog().await;
    let responses = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&responses);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_request: &Request| {
            match response_counter.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(200).set_body_raw(
                    sse(&[
                        json!({"id":"journey","choices":[{"delta":{"role":"assistant"}}]}),
                        json!({"id":"journey","choices":[{"delta":{"tool_calls":[{
                            "index":0,
                            "id":"call_write",
                            "type":"function",
                            "function":{"name":"write_file","arguments":r#"{"path":"journey.txt","content":"written by loom"}"#}
                        }]}}]}),
                        json!({"id":"journey","choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                    ])
                    .into_bytes(),
                    "text/event-stream",
                ),
                _ => text_response("The task is complete."),
            }
        })
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let trace_dir = temp.path().join("trace");
    let first = bounded_output(&mut run_command(
        &server,
        &temp,
        "create journey.txt with the requested text",
        &["--trace", trace_dir.to_str().unwrap()],
    ))
    .await;
    assert!(
        first.status.success(),
        "first run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_summary = summary(&first);
    let session_id = first_summary["session_id"].as_str().expect("session id");
    assert_eq!(first_summary["end_reason"], "completed");
    let workspace = temp.path().join("workspace");
    assert_eq!(
        std::fs::read_to_string(workspace.join("journey.txt")).unwrap(),
        "written by loom"
    );

    let task_trace = std::fs::read_dir(&trace_dir)
        .unwrap()
        .next()
        .expect("one task trace")
        .unwrap()
        .path();
    for name in ["events.ndjson", "transcript.json", "manifest.json"] {
        assert!(task_trace.join(name).is_file(), "missing trace file {name}");
    }
    let transcript: Value =
        serde_json::from_slice(&std::fs::read(task_trace.join("transcript.json")).unwrap())
            .unwrap();
    assert!(transcript.to_string().contains("written by loom"));

    let second = bounded_output(&mut run_command(
        &server,
        &temp,
        "summarize the completed change",
        &["--session", session_id],
    ))
    .await;
    assert!(
        second.status.success(),
        "resumed run failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(summary(&second)["session_id"], session_id);
    assert_eq!(summary(&second)["end_reason"], "completed");
    assert_eq!(responses.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cli_run_surfaces_provider_rejection_as_failure_without_panicking() {
    let server = server_with_catalog().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "invalid test request"}
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let output = bounded_output(&mut run_command(&server, &temp, "report status", &[])).await;
    assert!(
        !output.status.success(),
        "provider rejection must fail the task"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("panicked at"),
        "unexpected panic: {combined}"
    );
    assert!(
        combined.contains("invalid test request") || combined.contains("400"),
        "failure should retain provider context: {combined}"
    );
}
