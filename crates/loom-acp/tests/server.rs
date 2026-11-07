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
    ModelCapabilities, ModelEvent, ModelProvider, ModelRequest, ModelResponse, ModelStream,
    StopReason, Usage,
};
use loom_core::kernel::transcript::AssistantMessage;

/// A provider whose responses are fixed up front. `Hanging` never
/// completes a request — for the cancel test, the stream stays open
/// until the engine drops it.
#[derive(Clone)]
enum Script {
    Reply(String),
    Hanging,
}

#[derive(Clone)]
struct ScriptedProvider {
    scripts: Arc<Mutex<Vec<Script>>>,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts)),
        }
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

    fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
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

// The mapping units live in the library; the stdio smoke test lives
// in tests/stdio.rs. Keep this file wire-level only.
