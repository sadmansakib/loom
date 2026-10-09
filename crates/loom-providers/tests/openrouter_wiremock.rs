//! OpenRouter adapter over a mocked HTTP server.
//!
//! Covers what the adapter adds on top of the shared OpenAI-compatible
//! driver: attribution headers, the plain-OpenAI request body (no
//! provider-specific reasoning object), catalog-driven capabilities,
//! the OpenRouter error table (credits, rate limits, context overflow),
//! mid-stream error objects, provider-reported cost passthrough, and the
//! `finish_reason: "error"` retry path. The shared driver's own
//! behaviours (EOF tolerance, idle timeouts, stop normalization) are
//! covered against DeepSeek and are not duplicated here.

use std::time::Duration;

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest, StopReason,
};
use loom_kernel::tool::ToolSpec;
use loom_providers::{
    OpenRouterCatalog, OpenRouterConfig, OpenRouterProvider, TimeoutConfig, parse_catalog,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The catalog fixture every provider in this file is bound to: one
/// free tool-capable model and one paid non-reasoning model, so
/// free-versus-paid is a data difference, never a code path.
const CATALOG_JSON: &str = r#"{
    "data": [
        {
            "id": "vendor/free-model:free",
            "context_length": 200000,
            "top_provider": { "max_completion_tokens": 8192 },
            "pricing": { "prompt": "0", "completion": "0" },
            "supported_parameters": ["tools", "reasoning"]
        },
        {
            "id": "vendor/paid-model",
            "context_length": 1048576,
            "top_provider": { "max_completion_tokens": 65536 },
            "pricing": { "prompt": "0.000003", "completion": "0.000015" },
            "supported_parameters": ["tools"]
        }
    ]
}"#;

#[expect(clippy::expect_used, reason = "the fixture must parse")]
fn catalog() -> OpenRouterCatalog {
    parse_catalog(CATALOG_JSON, 1_700_000_000).expect("catalog fixture parses")
}

/// Provider pointed at `base_url`, attributed, bound to the fixture
/// catalog.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str) -> OpenRouterProvider {
    OpenRouterProvider::new(
        OpenRouterConfig::new("test-key")
            .with_base_url(base_url)
            .with_referer("https://loom.test")
            .with_title("Loom Tests")
            .with_timeouts(TimeoutConfig::default()),
        catalog(),
    )
    .unwrap()
}

/// A minimal user request for `model` with no tools.
fn plain_request(model: &str) -> ModelRequest {
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

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// A complete text response: one delta, then a finish with usage.
fn text_response(text: &str) -> String {
    sse(&[
        data_event(json!({"choices": [{"delta": {"content": text}}]})),
        data_event(json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 4}
        })),
        done(),
    ])
}

async fn collect(
    stream: loom_kernel::model::ModelStream<'_>,
) -> Vec<Result<ModelEvent, ModelError>> {
    stream.collect().await
}

fn expect_completed(
    events: Vec<Result<ModelEvent, ModelError>>,
) -> loom_kernel::model::ModelResponse {
    let Some(Ok(ModelEvent::Completed(response))) = events.last() else {
        panic!("expected a completed response, got {events:?}");
    };
    response.clone()
}

fn expect_error(events: &[Result<ModelEvent, ModelError>]) -> &ModelError {
    match events.last() {
        Some(Err(error)) => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn joined_text(events: &[Result<ModelEvent, ModelError>]) -> String {
    let mut text = String::new();
    for event in events {
        if let Ok(ModelEvent::TextDelta(delta)) = event {
            text.push_str(delta);
        }
    }
    text
}

#[tokio::test]
async fn streamed_text_completes_with_usage() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response("Hello from OpenRouter")).await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    assert_eq!(joined_text(&events), "Hello from OpenRouter");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 11);
    assert_eq!(response.usage.output_tokens, 4);
    // No cost reported by this fixture: absent is not zero.
    assert_eq!(response.usage.cost_usd, None);
}

#[tokio::test]
async fn reported_cost_passes_through_for_paid_models() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices": [{"delta": {"content": "paid"}}]})),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 1000,
                    "completion_tokens": 200,
                    "cost": 0.000018
                }
            })),
            done(),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/paid-model"))).await;

    let response = expect_completed(events);
    assert_eq!(response.usage.cost_usd, Some(0.000018));
}

#[tokio::test]
async fn attribution_headers_and_bearer_auth_are_sent() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response("ok")).await;

    let provider = provider_at(&server.uri());
    let request = plain_request("vendor/free-model:free");
    let _ = collect(provider.stream(&request)).await;

    let requests = server.received_requests().await.expect("captured request");
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    let header = |name: &str| {
        sent.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    assert_eq!(header("http-referer"), "https://loom.test");
    assert_eq!(header("x-title"), "Loom Tests");
    assert_eq!(header("authorization"), "Bearer test-key");
}

#[tokio::test]
async fn request_body_is_plain_openai() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response("ok")).await;

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(body["model"], "vendor/free-model:free");
    assert_eq!(body["stream"], true);
    // No provider-specific reasoning object on the OpenRouter wire.
    assert!(body.get("reasoning").is_none(), "{body}");
    assert!(body.get("thinking").is_none(), "{body}");
    // Tool fields are absent (not null) when the request carries no tools.
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
}

#[tokio::test]
async fn tools_are_advertised_when_present() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response("ok")).await;

    let tool = ToolSpec {
        name: "list_files".into(),
        description: "List files".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    };
    let mut request = plain_request("vendor/free-model:free");
    request.tools = vec![tool];

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&request)).await;

    let requests = server.received_requests().await.expect("captured request");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(body["tools"][0]["function"]["name"], "list_files");
    assert_eq!(body["tool_choice"], "auto");
}

#[tokio::test]
async fn credits_exhausted_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(402).set_body_json(json!({
            "error": {"message": "Insufficient credits", "code": 402}
        })))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/paid-model"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::QuotaExhausted);
    assert!(!error.is_retryable());
    assert!(error.message.contains("Insufficient credits"), "{error:?}");
}

#[tokio::test]
async fn rate_limit_is_retryable_and_honours_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "3")
                .set_body_json(json!({"error": {"message": "rate limited"}})),
        )
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::RateLimited);
    assert!(error.is_retryable());
    assert_eq!(error.retry_after, Some(Duration::from_secs(3)));
}

#[tokio::test]
async fn context_overflow_is_classified_not_generic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "This model's maximum context length is 200000 tokens"}
        })))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn auth_failure_is_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "no key"}})),
        )
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::Auth);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn mid_stream_error_object_is_classified() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices": [{"delta": {"content": "partial"}}]})),
            data_event(json!({
                "error": {"message": "upstream exploded", "code": 503}
            })),
            done(),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::Overloaded);
    assert!(error.is_retryable());
    assert!(error.message.contains("upstream exploded"), "{error:?}");
}

#[tokio::test]
async fn error_finish_reason_is_retryable() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices": [{"delta": {"content": "x"}}]})),
            data_event(json!({"choices": [{"delta": {}, "finish_reason": "error"}]})),
            done(),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("vendor/free-model:free"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, ModelErrorKind::Server);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn capabilities_come_from_the_bound_catalog() {
    let server = MockServer::start().await;
    let provider = provider_at(&server.uri());

    let free = provider
        .capabilities("vendor/free-model:free")
        .expect("free model in catalog");
    assert_eq!(free.max_context_tokens, 200_000);
    assert_eq!(free.max_output_tokens, Some(8192));
    assert!(free.tool_calls && free.reasoning);

    let paid = provider
        .capabilities("vendor/paid-model")
        .expect("paid model in catalog");
    assert_eq!(paid.max_context_tokens, 1_048_576);
    assert!(!paid.reasoning);

    // Unknown model: a loud None, never a guess.
    assert!(provider.capabilities("vendor/absent").is_none());
}

#[tokio::test]
async fn tool_calls_stream_through_with_synthesized_ids() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_abc",
                    "function": {"name": "list_files", "arguments": "{\"path\":"}
                }]}}]
            })),
            data_event(json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\".\"}"}
                }]}}]
            })),
            data_event(json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 3}
            })),
            done(),
        ]),
    )
    .await;

    let tool = ToolSpec {
        name: "list_files".into(),
        description: "List files".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    };
    let mut request = plain_request("vendor/free-model:free");
    request.tools = vec![tool];

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&request)).await;

    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
    let call = &response.message.tool_calls[0];
    assert_eq!(call.name, "list_files");
    assert_eq!(call.id.as_str(), "call_abc");
    let arguments: &Value = &call.arguments;
    assert_eq!(arguments["path"], ".");
}
