//! Integration tests for the NVIDIA adapter (NIM hosted inference)
//! against a real HTTP stack via wiremock.
//!
//! NVIDIA rides the same OpenAI-compatible wire as DeepSeek and GLM
//! (`openai_wire` + `wire_stream`), so the shared transport surface —
//! SSE decoding, EOF tolerance, error-body excerpts, cancellation — is
//! covered by `deepseek_wiremock.rs`. This file pins the behaviors
//! that are NIM-specific (docs.api.nvidia.com, fetched 2026-10-09):
//!
//! * the request-body shape: `publisher/model` ids, a top-level
//!   `reasoning_effort`, and no `thinking` object (that dialect belongs
//!   to Z.ai's first-party API);
//! * the GLM-5.3 deployments' forced thinking — an absent effort
//!   clamps to `low`;
//! * the DeepSeek V4 deployments' non-think mode — `none` passes
//!   through as `none`;
//! * usage accounting through the OpenAI-style details objects;
//! * tool-call fragment assembly with nvidia-prefixed synthesized ids;
//! * bearer authentication against `/v1/chat/completions`.

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest, StopReason,
};
use loom_kernel::transcript::ToolStatus;
use loom_providers::{NvidiaConfig, NvidiaProvider, ReasoningEffort, TimeoutConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Provider pointed at `base_url` with default (high) effort.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str) -> NvidiaProvider {
    NvidiaProvider::new(
        NvidiaConfig::new("test-key")
            .with_base_url(base_url)
            .with_timeouts(TimeoutConfig::default()),
    )
    .unwrap()
}

/// ...with an explicit reasoning-effort override.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_with_effort(base_url: &str, effort: ReasoningEffort) -> NvidiaProvider {
    NvidiaProvider::new(
        NvidiaConfig::new("test-key")
            .with_base_url(base_url)
            .with_reasoning_effort(effort)
            .with_timeouts(TimeoutConfig::default()),
    )
    .unwrap()
}

/// A minimal user request for a given model.
fn request_for(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        messages: vec![Message::User { text: "hi".into() }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
    }
}

fn data_event(payload: Value) -> String {
    payload.to_string()
}

fn sse(events: &[String]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(event);
        body.push_str("\n\n");
    }
    body
}

fn done() -> String {
    "data: [DONE]\n\n".into()
}

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Drains a provider stream to its terminal event.
async fn run_to_completion(provider: &NvidiaProvider, request: &ModelRequest) {
    let events: Vec<Result<ModelEvent, ModelError>> = provider.stream(request).collect().await;
    match events.into_iter().last() {
        Some(Ok(ModelEvent::Completed(_))) => {}
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn nvidia_body_carries_publisher_ids_and_top_level_effort() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;
    let request = request_for("z-ai/glm-5.3");
    run_to_completion(&provider_at(&server.uri()), &request).await;

    let requests = server.received_requests().await.expect("captured request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
    assert!(
        requests[0].url.path().ends_with("/chat/completions"),
        "got {}",
        requests[0].url
    );
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .expect("auth header")
            .to_str()
            .unwrap(),
        "Bearer test-key"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "z-ai/glm-5.3");
    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning_effort"], "high");
    assert!(
        body.get("thinking").is_none(),
        "the thinking object is Z.ai's first-party dialect, not NIM's"
    );
}

#[tokio::test]
async fn nvidia_glm_deployments_clamp_none_to_low() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;
    let request = request_for("z-ai/glm-5.3-flash");
    run_to_completion(
        &provider_with_effort(&server.uri(), ReasoningEffort::None),
        &request,
    )
    .await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["reasoning_effort"], "low",
        "forced-thinking deployment"
    );
}

#[tokio::test]
async fn nvidia_deepseek_deployments_pass_none_through() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;
    let request = request_for("deepseek-ai/deepseek-v4-pro");
    run_to_completion(
        &provider_with_effort(&server.uri(), ReasoningEffort::None),
        &request,
    )
    .await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["reasoning_effort"], "none", "non-think is a real mode");
}

#[tokio::test]
async fn nvidia_deepseek_deployments_map_max() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;
    let request = request_for("deepseek-ai/deepseek-v4-flash");
    run_to_completion(
        &provider_with_effort(&server.uri(), ReasoningEffort::Max),
        &request,
    )
    .await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["reasoning_effort"], "max");
}

#[tokio::test]
async fn reasoning_and_usage_details_account_cache_and_thinking() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"reasoning_content":"Pon"}}]})),
            data_event(json!({"choices":[{"delta":{"reasoning_content":"der"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"answer"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":100,"completion_tokens":50,
                         "prompt_tokens_details":{"cached_tokens":40},
                         "completion_tokens_details":{"reasoning_tokens":30}}})),
        ]) + &done(),
    )
    .await;

    let provider = provider_at(&server.uri());
    let request = request_for("z-ai/glm-5.3");
    let events: Vec<Result<ModelEvent, ModelError>> = provider.stream(&request).collect().await;
    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelEvent::ReasoningDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "Ponder");
    match events.into_iter().last() {
        Some(Ok(ModelEvent::Completed(response))) => {
            assert_eq!(response.message.content, "answer");
            assert_eq!(
                response.message.reasoning.expect("reasoning").text,
                "Ponder"
            );
            assert_eq!(response.usage.input_tokens, 100);
            assert_eq!(response.usage.output_tokens, 50);
            assert_eq!(response.usage.cached_input_tokens, Some(40));
            assert_eq!(response.usage.thinking_tokens, Some(30));
            assert_eq!(response.usage.cache_creation_input_tokens, None);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_call_fragments_assemble_with_nvidia_prefixed_ids() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"read_file","arguments":"{\"pa"}}]}}]})),
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"th\":\"x.rs\"}"}}]}}]})),
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"type":"function",
                 "function":{"name":"list_dir","arguments":"{\"pa"}}]}}]})),
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"th\":\".\"}"}}]}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
        ]) + &done(),
    )
    .await;

    let provider = provider_at(&server.uri());
    let request = request_for("deepseek-ai/deepseek-v4-pro");
    let events: Vec<Result<ModelEvent, ModelError>> = provider.stream(&request).collect().await;
    match events.into_iter().last() {
        Some(Ok(ModelEvent::Completed(response))) => {
            assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
            let calls = &response.message.tool_calls;
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].id.as_str(), "call_a");
            assert_eq!(calls[0].name, "read_file");
            assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x.rs"}"#);
            assert!(
                calls[1].id.as_str().starts_with("nvidia-call-"),
                "got {}",
                calls[1].id.as_str()
            );
            assert_eq!(calls[1].name, "list_dir");
            assert_eq!(calls[1].arguments.to_string(), r#"{"path":"."}"#);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_results_round_trip_through_the_wire_body() {
    // NIM defaults `clear_thinking` to false (preserved thinking), so
    // assistant `reasoning_content` must be replayed verbatim (same
    // DeepSeek contract shape); a following tool result must land as
    // a `tool` role message.
    use loom_kernel::model::Message as M;
    use loom_kernel::transcript::{
        AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId,
    };

    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;

    let call = ToolCall {
        id: ToolCallId::try_from("call_1".to_owned()).unwrap(),
        name: "read_file".into(),
        arguments: ToolArguments::try_from(json!({"path": "x.rs"})).unwrap(),
    };
    let request = ModelRequest {
        model: "deepseek-ai/deepseek-v4-flash".into(),
        messages: vec![
            M::System { text: "sys".into() },
            M::Assistant(AssistantMessage {
                content: "reading".into(),
                reasoning: Some(ReasoningTrace {
                    text: "step".into(),
                    provider_data: None,
                }),
                tool_calls: vec![call],
                stop_reason: StopReason::ToolCalls,
            }),
            M::ToolResult(loom_kernel::transcript::ToolResult::new(
                ToolCallId::try_from("call_1".to_owned()).unwrap(),
                ToolStatus::Success,
                "file body",
                false,
            )),
        ],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
    };
    let provider = provider_at(&server.uri());
    let _events: Vec<Result<ModelEvent, ModelError>> = provider.stream(&request).collect().await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[1]["reasoning_content"], "step");
    assert_eq!(messages[2]["role"], "tool");
}

#[tokio::test]
async fn http_error_status_classification_matches_the_openai_table() {
    for (status, kind, retryable) in [
        (401u16, ModelErrorKind::Auth, false),
        (429, ModelErrorKind::RateLimited, true),
        (503, ModelErrorKind::Overloaded, true),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string(r#"{"error": {"message": "boom"}}"#),
            )
            .mount(&server)
            .await;

        let provider = provider_at(&server.uri());
        let request = request_for("z-ai/glm-5.3");
        let events: Vec<Result<ModelEvent, ModelError>> = provider.stream(&request).collect().await;
        match events.into_iter().next() {
            Some(Err(error)) => {
                assert_eq!(error.kind, kind, "HTTP {status}");
                assert_eq!(error.is_retryable(), retryable, "HTTP {status}");
                assert!(error.message.contains("boom"), "body excerpt rides along");
            }
            other => panic!("HTTP {status}: expected error, got {other:?}"),
        }
    }
}
