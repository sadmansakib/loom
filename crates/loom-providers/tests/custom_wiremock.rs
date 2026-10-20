//! Integration tests for the custom adapter against a real HTTP stack
//! via wiremock.
//!
//! The custom adapter exists to point at OpenAI-compatible endpoints
//! Loom has no adapter for, so these tests pin the contract that makes
//! "any gateway" work:
//!
//! * the catalog fetch against a full models URL (bearer auth, OpenAI
//!   envelope, tolerant field sniffing) and its failure modes;
//! * the happy-path chat round trip on the plain OpenAI wire — no
//!   dialect fields a gateway might reject;
//! * capability resolution from the fetched catalog, silent fields
//!   resolving to safe defaults;
//! * `usage.cost` echoed by gateway-style reports surfacing as
//!   provider-reported cost.
//!
//! Shared transport behaviour (SSE decoding, EOF tolerance, timeouts)
//! is covered by `deepseek_wiremock.rs`; this file stays on what is
//! particular to speaking an unspecified vendor's dialect.

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
};
use loom_providers::{
    CustomConfig, CustomProvider, TimeoutConfig, fetch_custom_catalog, parse_custom_catalog,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gateway-model";

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

/// The OpenAI-shaped catalog most gateways serve, with mixed field
/// spellings to exercise the sniffing.
const CATALOG_BODY: &str = r#"{
    "data": [
        {
            "id": "gateway-model",
            "context_window": 131072,
            "max_output_tokens": 16384,
            "supported_parameters": ["tools", "parallel_tool_calls"],
            "pricing": { "prompt": 0.000001, "completion": 0.000002 }
        },
        { "id": "bare-model" }
    ]
}"#;

async fn mount_catalog(server: &MockServer, body: String) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json"),
        )
        .mount(server)
        .await;
}

/// Builds a provider whose catalog came from `body`, pointed at the
/// mock server.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_with_catalog(base_url: &str, body: &str) -> CustomProvider {
    let catalog = parse_custom_catalog(body, &format!("{base_url}/models"), 1).unwrap();
    CustomProvider::new(
        CustomConfig::new(base_url, "test-key").with_timeouts(TimeoutConfig::default()),
        catalog,
    )
    .unwrap()
}

/// Drains a provider stream and returns every event it produced.
async fn drain(
    provider: &CustomProvider,
    request: &ModelRequest,
) -> Vec<Result<ModelEvent, ModelError>> {
    provider.stream(request).collect().await
}

fn expect_completed(events: &[Result<ModelEvent, ModelError>]) -> ModelResponse {
    match events.last() {
        Some(Ok(ModelEvent::Completed(response))) => response.clone(),
        other => panic!("expected a completed response, got {other:?}"),
    }
}

#[tokio::test]
async fn catalog_fetch_reads_the_models_url_with_bearer_auth() {
    let server = MockServer::start().await;
    let url = format!("{}/v1/models", server.uri());
    mount_catalog(&server, CATALOG_BODY.to_owned()).await;

    let catalog = fetch_custom_catalog(&url, "test-key", TimeoutConfig::default())
        .await
        .expect("catalog fetches");

    // The snapshot remembers where it came from (cache keying).
    assert_eq!(catalog.source, url);
    assert_eq!(catalog.len(), 2);

    let model = catalog.model(MODEL).expect("model");
    assert_eq!(model.context_length, Some(131_072));
    assert_eq!(model.max_completion_tokens, Some(16_384));
    assert!(model.tool_capable && model.parallel_tool_calls && !model.reasoning_capable);

    // The request carried the bearer credential.
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1);
    let auth = requests[0]
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("authorization header");
    assert_eq!(auth, "Bearer test-key");
}

#[tokio::test]
async fn catalog_fetch_maps_rejection_to_status_error() {
    let server = MockServer::start().await;
    let url = format!("{}/v1/models", server.uri());
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "no key"})))
        .mount(&server)
        .await;

    let error = fetch_custom_catalog(&url, "test-key", TimeoutConfig::default())
        .await
        .expect_err("must reject");
    assert!(
        error.to_string().contains("401"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn chat_round_trip_assembles_text_usage_and_reported_cost() {
    let server = MockServer::start().await;
    let provider = provider_with_catalog(&server.uri(), CATALOG_BODY);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                data_event(json!({"id":"1","choices":[{"delta":{"role":"assistant"}}]})),
                data_event(json!({"id":"1","choices":[{"delta":{"content":"Hel"}}]})),
                data_event(json!({"id":"1","choices":[{"delta":{"content":"lo"}}]})),
                data_event(json!({"id":"1","choices":[{"delta":{},"finish_reason":"stop"}]})),
                data_event(json!({"id":"1","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9,"cost":0.000011}})),
                done(),
            ])
            .into_bytes(),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let events = drain(&provider, &request_for(MODEL)).await;
    let response = expect_completed(&events);
    assert_eq!(response.message.content, "Hello");
    assert_eq!(
        response.message.stop_reason,
        loom_kernel::model::StopReason::EndTurn
    );
    assert_eq!(response.usage.input_tokens, 7);
    assert_eq!(response.usage.output_tokens, 2);
    // The gateway echoed an actual charge; it surfaces as
    // provider-reported cost.
    assert_eq!(response.usage.cost_usd, Some(0.000011));
}

#[tokio::test]
async fn chat_request_is_the_plain_openai_shape() {
    let server = MockServer::start().await;
    let provider = provider_with_catalog(&server.uri(), CATALOG_BODY);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                sse(&[
                    data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
                    done(),
                ])
                .into_bytes(),
                "text/event-stream",
            ),
        )
        .mount(&server)
        .await;

    let events = drain(&provider, &request_for(MODEL)).await;
    expect_completed(&events);

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    // No dialect fields: a gateway that only knows the OpenAI shape
    // must not see vendor-specific extras.
    assert!(body.get("reasoning").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("thinking").is_none());
}

#[tokio::test]
async fn error_status_bodies_classify_and_excerpt() {
    let server = MockServer::start().await;
    let provider = provider_with_catalog(&server.uri(), CATALOG_BODY);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "rate limit hit"}
        })))
        .mount(&server)
        .await;

    let events = drain(&provider, &request_for(MODEL)).await;
    match events.last() {
        Some(Err(error)) => {
            assert_eq!(error.kind, loom_kernel::model::ModelErrorKind::RateLimited);
            assert!(error.to_string().contains("429"), "{error}");
        }
        other => panic!("expected a rate-limited error, got {other:?}"),
    }
}

#[tokio::test]
async fn capabilities_resolve_from_the_bound_catalog() {
    let server = MockServer::start().await;
    let provider = provider_with_catalog(&server.uri(), CATALOG_BODY);

    let advertised = provider.capabilities(MODEL).expect("advertised caps");
    assert_eq!(advertised.max_context_tokens, 131_072);
    assert_eq!(advertised.max_output_tokens, Some(16_384));
    assert!(advertised.tool_calls && advertised.parallel_tool_calls);
    assert!(!advertised.reasoning);

    // A catalog entry with no metadata still resolves, on defaults.
    let bare = provider.capabilities("bare-model").expect("bare caps");
    assert_eq!(
        bare.max_context_tokens,
        loom_providers::DEFAULT_CONTEXT_TOKENS
    );
    assert_eq!(bare.max_output_tokens, None);
    assert!(bare.tool_calls);
    assert!(!bare.parallel_tool_calls);
    assert!(!bare.reasoning);

    // An id the gateway never listed fails loudly — typo protection.
    assert!(provider.capabilities("gateway-modle").is_none());
}
