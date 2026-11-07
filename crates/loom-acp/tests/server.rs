//! In-process ACP tests: the real server over an
//! [`agent_client_protocol::Channel`] pair, with a scripted provider
//! riding in through `EngineOverrides::provider` — no network, no
//! editor, no docker.
//!
//! The client side runs inside `connect_with`'s callback, where the
//! SDK hands out the connection handle; the server runs as a spawned
//! task on the other channel end. A notification handler on the
//! client captures every `session/update` in wire order.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::Channel;
use agent_client_protocol::role::acp::{Agent, Client};
use agent_client_protocol::v1::schema as acp;
use loom_acp::{AcpState, ServerOptions, serve};
use loom_core::kernel::model::{
    Message as ModelMessage, ModelCapabilities, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream, StopReason, Usage,
};
use loom_core::kernel::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId};
use serde_json::json;

/// A provider whose responses are fixed up front. `Hanging` never
/// completes a request — for the cancel test, the stream stays open
/// until the engine drops it. `ToolCall` answers with one assistant
/// tool call (the engine executes it and comes back for the next
/// script).
#[derive(Clone)]
enum Script {
    Reply(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    Hanging,
}

/// What the scripted provider saw in one model request: the user
/// texts (resume proof) and the advertised tool names (attach proof).
#[derive(Debug, Clone)]
struct CapturedRequest {
    user_texts: Vec<String>,
    tool_names: Vec<String>,
}

/// Shared across every provider clone (one per connection), so a test
/// can assert on the whole conversation history.
#[derive(Clone, Default)]
struct Capture {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl Capture {
    fn any_user_text(&self, needle: &str) -> bool {
        self.requests
            .lock()
            .map(|requests| {
                requests
                    .iter()
                    .any(|request| request.user_texts.iter().any(|t| t == needle))
            })
            .unwrap_or(false)
    }

    fn all_tools(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|requests| {
                requests
                    .iter()
                    .flat_map(|request| request.tool_names.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct ScriptedProvider {
    scripts: Arc<Mutex<Vec<Script>>>,
    capture: Capture,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts)),
            capture: Capture::default(),
        }
    }

    /// The shared request log for assertions.
    fn capture(&self) -> Capture {
        self.capture.clone()
    }
}

impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        // One known model, tool-capable, so the engine's pre-request
        // checks pass.
        (model == "test-model").then(|| {
            ModelCapabilities::new(200_000)
                .with_tool_calls(true)
                .with_parallel_tool_calls(false)
                .with_reasoning(false)
        })
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let user_texts = request
            .messages
            .iter()
            .filter_map(|message| match message {
                ModelMessage::User { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let tool_names = request
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let index = {
            let mut requests = match self.capture.requests.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            requests.push(CapturedRequest {
                user_texts,
                tool_names,
            });
            requests.len()
        };
        let script = self
            .scripts
            .lock()
            .map(|mut scripts| scripts.remove(0))
            .unwrap_or(Script::Reply(String::new()));
        match script {
            Script::Hanging => Box::pin(futures::stream::pending()),
            Script::Reply(text) => {
                // Real providers stream visible text as deltas before
                // the authoritative Completed (the kernel forwards
                // only deltas as TextDelta events — a Completed-only
                // stream would carry no displayable text at all).
                let response = ModelResponse {
                    message: AssistantMessage {
                        content: text.clone(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        stop_reason: StopReason::EndTurn,
                    },
                    usage: Usage::default(),
                };
                Box::pin(futures::stream::iter([
                    Ok(ModelEvent::TextDelta(text)),
                    Ok(ModelEvent::Completed(response)),
                ]))
            }
            Script::ToolCall { name, arguments } => {
                #[allow(clippy::expect_used)]
                let call = ToolCall {
                    id: ToolCallId::try_from(format!("call-{index}")).expect("valid call id"),
                    name,
                    arguments: ToolArguments::try_from(arguments).expect("object arguments"),
                };
                let response = ModelResponse {
                    message: AssistantMessage {
                        content: String::new(),
                        reasoning: None,
                        tool_calls: vec![call],
                        stop_reason: StopReason::ToolCalls,
                    },
                    usage: Usage::default(),
                };
                Box::pin(futures::stream::iter([Ok(ModelEvent::Completed(response))]))
            }
        }
    }
}

/// One captured client-side session notification.
#[derive(Debug, Clone)]
struct CapturedUpdate {
    session: acp::SessionId,
    update: acp::SessionUpdate,
}

/// A fresh workspace directory per test (kept alive for the test's
/// duration — the engine canonicalizes and reads manifests from it).
fn workspace() -> PathBuf {
    // Repo convention allows expect() in cfg(test) code; clippy's
    // in-tests exemption does not reach integration-test crates, so
    // this call site needs the explicit allow.
    #[allow(clippy::expect_used)]
    let dir = tempfile::tempdir().expect("workspace tempdir");
    dir.keep()
}

fn text_content(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.to_owned()))
}

async fn await_updates(
    updates: &Arc<Mutex<Vec<CapturedUpdate>>>,
    check: impl Fn(&[CapturedUpdate]) -> bool,
) {
    for _ in 0..400 {
        if updates.lock().map(|u| check(&u)).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let snapshot = updates.lock().map(|u| u.clone()).unwrap_or_default();
    panic!("updates never satisfied the check: {snapshot:#?}");
}

#[tokio::test]
async fn initialize_new_session_and_prompt_run_end_to_end() {
    let (client_end, agent_end) = Channel::duplex();
    let workspace = workspace();
    let updates: Arc<Mutex<Vec<CapturedUpdate>>> = Arc::new(Mutex::new(Vec::new()));

    let options = ServerOptions::new(
        "scripted".into(),
        "test-model".into(),
        std::env::temp_dir().join(format!("loom-acp-e2e-{}.db", std::process::id())),
    )
    .with_overrides_factory(Arc::new(move || {
        let provider = ScriptedProvider::new(vec![Script::Reply("Hello from loom.".into())]);
        loom_core::EngineOverrides::default().with_provider(Box::new(provider))
    }));
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });

    let capture = updates.clone();
    let wait_for = updates.clone();
    let result = Client
        .v1()
        .on_receive_notification(
            move |notification: acp::SessionNotification,
                  _cx: agent_client_protocol::v1::ConnectionTo<Agent>| {
                let captured = CapturedUpdate {
                    session: notification.session_id.clone(),
                    update: notification.update.clone(),
                };
                if let Ok(mut seen) = capture.lock() {
                    seen.push(captured);
                }
                async { Ok(()) }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_end, async move |cx| {
            let initialize = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            assert_eq!(
                initialize
                    .agent_info
                    .as_ref()
                    .map(|info| info.name.as_str()),
                Some("loom")
            );

            let session = cx
                .send_request(acp::NewSessionRequest::new(workspace))
                .block_task()
                .await
                .expect("new session");

            let response = cx
                .send_request(acp::PromptRequest::new(
                    session.session_id.clone(),
                    vec![text_content("say hello")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert!(matches!(response.stop_reason, acp::StopReason::EndTurn));

            let expected_session = session.session_id.clone();
            await_updates(&wait_for, |seen| {
                seen.iter().any(|captured| {
                    captured.session == expected_session
                        && match &captured.update {
                            acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                                acp::ContentBlock::Text(text) => text.text == "Hello from loom.",
                                _ => false,
                            },
                            _ => false,
                        }
                })
            })
            .await;
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
}

#[tokio::test]
async fn unknown_session_prompt_is_an_error() {
    let (client_end, agent_end) = Channel::duplex();
    let workspace = workspace();
    let _ = workspace;
    let options = ServerOptions::new(
        "scripted".into(),
        "test-model".into(),
        std::env::temp_dir().join(format!("loom-acp-unknown-{}.db", std::process::id())),
    )
    .with_overrides_factory(Arc::new(|| {
        let provider = ScriptedProvider::new(vec![Script::Reply(String::new())]);
        loom_core::EngineOverrides::default().with_provider(Box::new(provider))
    }));
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });

    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            let outcome = cx
                .send_request(acp::PromptRequest::new(
                    acp::SessionId::new("no-such-session"),
                    vec![text_content("hello?")],
                ))
                .block_task()
                .await;
            assert!(
                outcome.is_err(),
                "a prompt for an unknown session must be an error, got {outcome:?}"
            );
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
}

#[tokio::test]
async fn cancel_resolves_the_prompt_as_cancelled() {
    let (client_end, agent_end) = Channel::duplex();
    let workspace = workspace();
    let options = ServerOptions::new(
        "scripted".into(),
        "test-model".into(),
        std::env::temp_dir().join(format!("loom-acp-cancel-{}.db", std::process::id())),
    )
    .with_overrides_factory(Arc::new(|| {
        let provider = ScriptedProvider::new(vec![Script::Hanging]);
        loom_core::EngineOverrides::default().with_provider(Box::new(provider))
    }));
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });

    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            let session = cx
                .send_request(acp::NewSessionRequest::new(workspace))
                .block_task()
                .await
                .expect("new session");
            let prompt = cx.send_request(acp::PromptRequest::new(
                session.session_id.clone(),
                vec![text_content("stall forever")],
            ));
            // Let the prompt reach the hanging model request, then
            // cancel the session.
            tokio::time::sleep(Duration::from_millis(300)).await;
            cx.send_notification(acp::CancelNotification::new(session.session_id.clone()))
                .expect("cancel sent");
            let response = prompt.block_task().await.expect("prompt resolves");
            assert!(
                matches!(response.stop_reason, acp::StopReason::Cancelled),
                "cancel must resolve the prompt as cancelled, got {:?}",
                response.stop_reason
            );
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
}

// ------------------------------------------------- session/load (tail) ----

/// Builds one server options set over the given db with a clone of the
/// shared scripted provider (scripts and capture are Arc-shared).
fn load_options(db: &std::path::Path, provider: &ScriptedProvider) -> ServerOptions {
    let provider = provider.clone();
    ServerOptions::new("scripted".into(), "test-model".into(), db.to_path_buf())
        .with_overrides_factory(Arc::new(move || {
            loom_core::EngineOverrides::default().with_provider(Box::new(provider.clone()))
        }))
}

fn unique_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "loom-acp-{name}-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ));
    let _ = std::fs::remove_file(&path);
    path
}

#[tokio::test]
async fn session_load_resumes_a_persisted_session_across_connections() {
    let workspace = workspace();
    let db = unique_db("resume");
    let provider = ScriptedProvider::new(vec![
        Script::Reply("one".into()),
        Script::Reply("two".into()),
    ]);
    let capture = provider.capture();
    let shared_id: Arc<Mutex<Option<acp::SessionId>>> = Arc::new(Mutex::new(None));

    // Connection 1: create the session, run one turn, keep the id.
    let (client_end, agent_end) = Channel::duplex();
    let first_options = load_options(&db, &provider);
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(first_options)).await {
            eprintln!("test server ended: {error}");
        }
    });
    let first_workspace = workspace.clone();
    let set_id = shared_id.clone();
    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            let session = cx
                .send_request(acp::NewSessionRequest::new(first_workspace))
                .block_task()
                .await
                .expect("new session");
            let response = cx
                .send_request(acp::PromptRequest::new(
                    session.session_id.clone(),
                    vec![text_content("first turn")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert!(matches!(response.stop_reason, acp::StopReason::EndTurn));
            // The id doubles as the kernel session id: a valid UUID.
            assert!(uuid::Uuid::parse_str(&session.session_id.0).is_ok());
            if let Ok(mut slot) = set_id.lock() {
                *slot = Some(session.session_id.clone());
            }
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "first connection failed: {result:?}");

    let resumed_id = shared_id
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .expect("session id captured");

    // Connection 2: same db, same workspace — load and continue.
    let (client_end, agent_end) = Channel::duplex();
    let second_options = load_options(&db, &provider);
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(second_options)).await {
            eprintln!("test server ended: {error}");
        }
    });
    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            cx.send_request(acp::LoadSessionRequest::new(resumed_id.clone(), workspace))
                .block_task()
                .await
                .expect("session/load resolves");
            let response = cx
                .send_request(acp::PromptRequest::new(
                    resumed_id,
                    vec![text_content("second turn")],
                ))
                .block_task()
                .await
                .expect("prompt on the loaded session");
            assert!(matches!(response.stop_reason, acp::StopReason::EndTurn));
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "second connection failed: {result:?}");

    // The second connection's model request still carried the first
    // connection's persisted turn — that is resume, not a fresh start.
    assert!(
        capture.any_user_text("first turn"),
        "the loaded transcript rides the resumed request"
    );
    assert!(capture.any_user_text("second turn"));
}

#[tokio::test]
async fn session_load_of_an_unknown_id_is_an_error() {
    let workspace = workspace();
    let provider = ScriptedProvider::new(vec![]);
    let options = load_options(&unique_db("unknown-load"), &provider);
    let (client_end, agent_end) = Channel::duplex();
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });
    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            // A well-formed v7 id that was never persisted.
            let fresh = uuid::Uuid::now_v7().to_string();
            let outcome = cx
                .send_request(acp::LoadSessionRequest::new(
                    acp::SessionId::new(fresh),
                    workspace,
                ))
                .block_task()
                .await;
            assert!(
                outcome.is_err(),
                "loading an unknown session must be an error, got {outcome:?}"
            );
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
}

#[tokio::test]
async fn session_load_rejects_non_v7_ids() {
    let workspace = workspace();
    let provider = ScriptedProvider::new(vec![]);
    for bad in ["not-a-uuid", "00000000-0000-4000-8000-000000000000"] {
        let db = unique_db("non-v7-load");
        let options = load_options(&db, &provider);
        let (client_end, agent_end) = Channel::duplex();
        tokio::spawn(async move {
            if let Err(error) = serve(agent_end, AcpState::new(options)).await {
                eprintln!("test server ended: {error}");
            }
        });
        let workspace = workspace.clone();
        let result = Client
            .v1()
            .connect_with(client_end, async move |cx| {
                let _ = cx
                    .send_request(acp::InitializeRequest::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                    .block_task()
                    .await
                    .expect("initialize");
                let outcome = cx
                    .send_request(acp::LoadSessionRequest::new(
                        acp::SessionId::new(bad),
                        workspace,
                    ))
                    .block_task()
                    .await;
                assert!(
                    outcome.is_err(),
                    "a non-v7 session id must be rejected, got {outcome:?}"
                );
                Ok(())
            })
            .await;
        assert!(result.is_ok(), "client connection failed: {result:?}");
    }
}

// ------------------------------------------- forwarded mcp_servers (tail) ---

#[test]
fn mcp_mock_server() {
    // Test-binary re-entry: the forwarded-server spec points at this
    // executable with `--exact mcp_mock_server`; the child becomes a
    // one-tool MCP stdio server when the mode variable is set.
    if std::env::var("LOOM_MCP_MOCK_MODE").is_err() {
        return;
    }
    use std::io::{BufRead, Write};
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // libtest progress text has no trailing newline; start clean.
    let _ = writeln!(out);
    let _ = out.flush();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let message: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let reply = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "0.0.0"}
            }),
            "tools/list" => json!({"tools": [{
                "name": "greet",
                "description": "Greets a person by name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            }]}),
            "tools/call" => {
                let name = message
                    .pointer("/params/arguments/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("loom");
                json!({"content": [{"type": "text", "text": format!("Hello, {name}!")}]})
            }
            _ => continue,
        };
        let _ = writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": reply})
        );
        let _ = out.flush();
    }
}

fn mock_stdio_server() -> acp::McpServer {
    #[allow(clippy::expect_used)]
    let exe = std::env::current_exe().expect("test binary path");
    acp::McpServer::Stdio(
        acp::McpServerStdio::new("mock", exe)
            .args(vec![
                "--exact".into(),
                "mcp_mock_server".into(),
                "--test-threads=1".into(),
                "--nocapture".into(),
            ])
            .env(vec![acp::EnvVariable::new("LOOM_MCP_MOCK_MODE", "greet")]),
    )
}

#[tokio::test]
async fn forwarded_mcp_servers_attach_and_run_when_allowed() {
    let workspace = workspace();
    let updates: Arc<Mutex<Vec<CapturedUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![
        Script::ToolCall {
            name: "mcp_mock_greet".into(),
            arguments: json!({"name": "loom"}),
        },
        Script::Reply("done".into()),
    ]);
    let capture = provider.capture();
    let provider_for_factory = provider.clone();
    let options = ServerOptions::new(
        "scripted".into(),
        "test-model".into(),
        unique_db("mcp-allowed"),
    )
    .with_allow_client_mcp(true)
    .with_overrides_factory(Arc::new(move || {
        loom_core::EngineOverrides::default().with_provider(Box::new(provider_for_factory.clone()))
    }));

    let (client_end, agent_end) = Channel::duplex();
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });

    let capture_updates = updates.clone();
    let wait_for = updates.clone();
    let result = Client
        .v1()
        .on_receive_notification(
            move |notification: acp::SessionNotification,
                  _cx: agent_client_protocol::v1::ConnectionTo<Agent>| {
                let captured = CapturedUpdate {
                    session: notification.session_id.clone(),
                    update: notification.update.clone(),
                };
                if let Ok(mut seen) = capture_updates.lock() {
                    seen.push(captured);
                }
                async { Ok(()) }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            let mut new_session = acp::NewSessionRequest::new(workspace);
            new_session.mcp_servers = vec![mock_stdio_server()];
            let session = cx
                .send_request(new_session)
                .block_task()
                .await
                .expect("new session with a forwarded server");
            let response = cx
                .send_request(acp::PromptRequest::new(
                    session.session_id.clone(),
                    vec![text_content("greet loom")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert!(matches!(response.stop_reason, acp::StopReason::EndTurn));

            // The forwarded tool ran: the mock's answer reached the
            // editor as tool-call content.
            await_updates(&wait_for, |seen| {
                seen.iter().any(|captured| {
                    matches!(
                        &captured.update,
                        acp::SessionUpdate::ToolCallUpdate(update)
                            if update
                                .fields
                                .content
                                .as_ref()
                                .map(|blocks| {
                                    blocks.iter().any(|block| match block {
                                        acp::ToolCallContent::Content(content) => match &content.content {
                                            acp::ContentBlock::Text(text) => {
                                                text.text.contains("Hello, loom!")
                                            }
                                            _ => false,
                                        },
                                        _ => false,
                                    })
                                })
                                .unwrap_or(false)
                    )
                })
            })
            .await;
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
    // The engine advertised the forwarded tool to the model.
    assert!(
        capture
            .all_tools()
            .iter()
            .any(|name| name == "mcp_mock_greet"),
        "the forwarded server's tool is in the registry, got {:?}",
        capture.all_tools()
    );
}

#[tokio::test]
async fn forwarded_mcp_servers_are_ignored_when_the_flag_is_off() {
    let workspace = workspace();
    let provider = ScriptedProvider::new(vec![Script::Reply("ok".into())]);
    let capture = provider.capture();
    let options = load_options(&unique_db("mcp-off"), &provider);

    let (client_end, agent_end) = Channel::duplex();
    tokio::spawn(async move {
        if let Err(error) = serve(agent_end, AcpState::new(options)).await {
            eprintln!("test server ended: {error}");
        }
    });
    let result = Client
        .v1()
        .connect_with(client_end, async move |cx| {
            let _ = cx
                .send_request(acp::InitializeRequest::new(
                    agent_client_protocol::schema::ProtocolVersion::V1,
                ))
                .block_task()
                .await
                .expect("initialize");
            let mut new_session = acp::NewSessionRequest::new(workspace);
            new_session.mcp_servers = vec![mock_stdio_server()];
            let session = cx
                .send_request(new_session)
                .block_task()
                .await
                .expect("new session");
            let response = cx
                .send_request(acp::PromptRequest::new(
                    session.session_id.clone(),
                    vec![text_content("hello")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert!(matches!(response.stop_reason, acp::StopReason::EndTurn));
            // No spawned server, no mcp_* tools advertised.
            assert!(
                capture
                    .all_tools()
                    .iter()
                    .all(|name| !name.starts_with("mcp_")),
                "no MCP tools without the operator grant, got {:?}",
                capture.all_tools()
            );
            Ok(())
        })
        .await;
    assert!(result.is_ok(), "client connection failed: {result:?}");
}
