//! Integration tests for the Groq adapter against a real HTTP stack via
//! wiremock.
//!
//! Groq rides the same OpenAI-compatible wire as DeepSeek, GLM, and NVIDIA
//! (`openai_wire` + `wire_stream`), so the shared transport surface — SSE
//! decoding, EOF tolerance, error-body excerpts, cancellation — is covered
//! by `deepseek_wiremock.rs`. This file pins the behaviours that are
//! Groq-specific (console.groq.com docs and live probes, 2026-10-18):
//!
//! * the plain `reasoning` dialect, on both the request body and the
//!   stream read: replaying an assistant turn with `reasoning_content`
//!   is a live-confirmed HTTP 400;
//! * `reasoning_effort` clamping onto the closed enum every pinned row
//!   accepts (gpt-oss rejects `none` and `max` with HTTP 400);
//! * tool-call assembly with provider-issued ids kept verbatim and
//!   idless fragments synthesized under the provider's own prefix;
//! * `x_groq.error` as the mid-stream failure channel — Groq documents no
//!   in-stream `finish_reason` for failures;
//! * the Groq-specific statuses 498 (flex tier at capacity, retryable)
//!   and 499 (caller cancelled, not retryable);
//! * capability resolution from the adapter's own table, which is the
//!   only authority here (Groq publishes no capability document).

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest, StopReason,
};
use loom_providers::{GroqConfig, GroqProvider, ReasoningEffort, TimeoutConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "openai/gpt-oss-120b";

/// Provider pointed at `base_url` with default (high) effort.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str) -> GroqProvider {
    GroqProvider::new(
        GroqConfig::new("test-key")
            .with_base_url(base_url)
            .with_timeouts(TimeoutConfig::default()),
    )
    .unwrap()
}

/// ...with an explicit reasoning-effort override.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_with_effort(base_url: &str, effort: ReasoningEffort) -> GroqProvider {
    GroqProvider::new(
        GroqConfig::new("test-key")
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
    "[DONE]".into()
}

/// A stream that ends immediately, for tests that only inspect the
/// request the adapter sent.
fn empty_stream() -> String {
    sse(&[
        data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        done(),
    ])
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

async fn mount_status(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// Drains a provider stream and returns every event it produced.
async fn drain(
    provider: &GroqProvider,
    request: &ModelRequest,
) -> Vec<Result<ModelEvent, ModelError>> {
    provider.stream(request).collect().await
}

fn expect_completed(
    events: &[Result<ModelEvent, ModelError>],
) -> loom_kernel::model::ModelResponse {
    match events.last() {
        Some(Ok(ModelEvent::Completed(response))) => response.clone(),
        other => panic!("expected a completed response, got {other:?}"),
    }
}

fn expect_error(events: &[Result<ModelEvent, ModelError>]) -> ModelError {
    match events.last() {
        Some(Err(error)) => error.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn joined_reasoning(events: &[Result<ModelEvent, ModelError>]) -> String {
    let mut text = String::new();
    for event in events {
        if let Ok(ModelEvent::ReasoningDelta(delta)) = event {
            text.push_str(delta);
        }
    }
    text
}

#[tokio::test]
async fn groq_body_sends_top_level_effort_and_bearer_auth() {
    let server = MockServer::start().await;
    mount_sse(&server, empty_stream()).await;
    drain(&provider_at(&server.uri()), &request_for(MODEL)).await;

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
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning_effort"], "high");
    assert!(
        body.get("thinking").is_none(),
        "the thinking object is Z.ai's first-party dialect, not Groq's"
    );
}

#[tokio::test]
async fn groq_clamps_effort_onto_the_enum_gpt_oss_accepts() {
    // Live: gpt-oss-120b answers HTTP 400 ("`reasoning_effort` must be
    // one of `low`, `medium`, or `high`") for `none` and `max`, so the
    // adapter clamps onto the accepted set instead of forwarding a
    // request the model would reject.
    for (effort, expected) in [
        (ReasoningEffort::None, "low"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::Max, "high"),
    ] {
        let server = MockServer::start().await;
        mount_sse(&server, empty_stream()).await;
        drain(
            &provider_with_effort(&server.uri(), effort),
            &request_for(MODEL),
        )
        .await;

        let requests = server.received_requests().await.expect("captured request");
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["reasoning_effort"], expected, "{effort:?}");
    }
}

#[tokio::test]
async fn groq_streams_reasoning_from_the_plain_delta_field() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"reasoning":"weighing "}}]})),
            data_event(json!({"choices":[{"delta":{"reasoning":"the tradeoff"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"done"}}]})),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 40, "completion_tokens": 9}
            })),
            done(),
        ]),
    )
    .await;

    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    assert_eq!(joined_reasoning(&events), "weighing the tradeoff");
    let response = expect_completed(&events);
    let trace = response
        .message
        .reasoning
        .expect("the reasoning trace survives into the response");
    assert_eq!(trace.text, "weighing the tradeoff");
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 40);
    assert_eq!(response.usage.output_tokens, 9);
}

#[tokio::test]
async fn groq_prefers_the_plain_field_and_tolerates_the_shared_one() {
    // Read side is tolerant, write side is not: Groq emits `reasoning`, but
    // a gateway in front of it may still emit the shared
    // `reasoning_content`. Reading it costs nothing — the trace replays
    // under the plain key either way — so the plain field wins when both
    // are present and the shared one is a fallback, never a merge.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{
                "reasoning": "groq's own ",
                "reasoning_content": "the wrong dialect "
            }}]})),
            data_event(json!({"choices":[{"delta":{"reasoning_content":"only the shared one"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"hi"}}]})),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1}
            })),
            done(),
        ]),
    )
    .await;

    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    let response = expect_completed(&events);
    let trace = response
        .message
        .reasoning
        .expect("the tolerated fragment still lands in the trace");
    assert_eq!(
        trace.text, "groq's own only the shared one",
        "the plain field wins per chunk; the shared one is a fallback"
    );
}

#[tokio::test]
async fn groq_tool_call_turn_keeps_the_provider_id() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[{
                "index": 0,
                "id": "fc_9840b1fd-6a7c-4f21-9c3e-2b8d5a1e77aa",
                "function": {"name": "read_file", "arguments": "{\"path\":"}
            }]}}]})),
            data_event(json!({"choices":[{"delta":{"tool_calls":[{
                "index": 0,
                "function": {"arguments": "\"src/main.rs\"}"}
            }]}}]})),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 60, "completion_tokens": 20}
            })),
            done(),
        ]),
    )
    .await;

    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    let response = expect_completed(&events);
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
    let call = &response.message.tool_calls[0];
    assert_eq!(call.name, "read_file");
    assert_eq!(call.id.as_str(), "fc_9840b1fd-6a7c-4f21-9c3e-2b8d5a1e77aa");
    assert_eq!(call.arguments.to_string(), r#"{"path":"src/main.rs"}"#);
}

#[tokio::test]
async fn groq_synthesizes_ids_for_idless_tool_fragments() {
    // qwen emits short opaque ids and can omit the id entirely on a
    // continuation fragment; the provider's own prefix keeps synthesized
    // ids in its namespace.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[{
                "index": 0,
                "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}
            }]}}]})),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 8}
            })),
            done(),
        ]),
    )
    .await;

    let events = drain(
        &provider_at(&server.uri()),
        &request_for("qwen/qwen3.8-27b"),
    )
    .await;
    let response = expect_completed(&events);
    let call = &response.message.tool_calls[0];
    assert!(
        call.id.as_str().starts_with("groq-call-"),
        "synthesized under the provider prefix, got {}",
        call.id
    );
}

#[tokio::test]
async fn groq_envelope_error_is_the_mid_stream_failure_channel() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"partial"}}]})),
            data_event(json!({"x_groq": {"error": {
                "message": "The model is overloaded",
                "type": "server_error"
            }}})),
            done(),
        ]),
    )
    .await;

    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::Server);
    assert!(
        error.message.contains("overloaded"),
        "the envelope body reaches the message, got {}",
        error.message
    );
}

#[tokio::test]
async fn groq_flex_capacity_status_is_a_retryable_overload() {
    // 498 is Groq's own "flex tier at capacity"; 499 is the caller
    // cancelling. Neither is a generic client error.
    let server = MockServer::start().await;
    mount_status(
        &server,
        498,
        json!({"error": {"message": "flex at capacity"}}),
    )
    .await;
    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    assert_eq!(expect_error(&events).kind, ModelErrorKind::Overloaded);

    let server = MockServer::start().await;
    mount_status(&server, 499, json!({"error": {"message": "cancelled"}})).await;
    let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
    assert_eq!(expect_error(&events).kind, ModelErrorKind::Cancelled);
}

#[tokio::test]
async fn groq_status_table_maps_the_documented_codes() {
    for (status, kind) in [
        (400u16, ModelErrorKind::InvalidRequest),
        (401, ModelErrorKind::Auth),
        (429, ModelErrorKind::RateLimited),
        (503, ModelErrorKind::Overloaded),
        (500, ModelErrorKind::Server),
    ] {
        let server = MockServer::start().await;
        mount_status(
            &server,
            status,
            json!({"error": {"message": "boom", "type": "x"}}),
        )
        .await;
        let events = drain(&provider_at(&server.uri()), &request_for(MODEL)).await;
        assert_eq!(expect_error(&events).kind, kind, "HTTP {status}");
    }
}

#[tokio::test]
async fn groq_capabilities_come_from_the_adapter_table() {
    let provider = provider_at("http://localhost:1");
    let caps = provider.capabilities(MODEL).expect("pinned row");
    assert_eq!(caps.max_context_tokens, 131_072);
    assert_eq!(caps.max_output_tokens, Some(65_536));
    assert!(caps.tool_calls && caps.reasoning);
    // Live-verified: gpt-oss emits one tool call per turn.
    assert!(!caps.parallel_tool_calls);

    let qwen = provider
        .capabilities("qwen/qwen3.8-27b")
        .expect("pinned row");
    assert!(qwen.parallel_tool_calls);

    for unknown in [
        "meta-llama/llama-3.3-70b-versatile",
        "whisper-large-v3",
        "meta-llama/llama-prompt-guard-2-86m",
        "gpt-oss-120b",
    ] {
        assert!(
            provider.capabilities(unknown).is_none(),
            "unknown id resolves to no row: {unknown}"
        );
    }
}
