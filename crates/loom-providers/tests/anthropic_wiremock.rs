//! Anthropic adapter over a mocked HTTP server.
//!
//! The wire translation itself is unit-tested in `anthropic_wire`; these
//! tests exercise the streaming machine end to end: headers, request
//! shape (including the stable-prefix cache breakpoint), SSE assembly,
//! usage accounting, and error classification.

use futures::StreamExt;
use loom_kernel::model::{
    ContextEditingHint, Message, ModelEvent, ModelProvider, ModelRequest, ModelStream, StopReason,
};
use loom_kernel::transcript::{ToolArguments, ToolCall, ToolCallId};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use loom_providers::{AnthropicConfig, AnthropicProvider};

#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str) -> AnthropicProvider {
    AnthropicProvider::new(
        AnthropicConfig::new(String::from("test-key")).with_base_url(base_url.to_owned()),
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
        context_editing: None,
    }
}

fn sse(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body
}

/// A complete text-only Anthropic stream with cached input accounting.
fn text_response() -> String {
    sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 40}}
        }),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello from Anthropic"}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
        serde_json::json!({"type": "message_stop"}),
    ])
}

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

async fn collect(
    stream: ModelStream<'_>,
) -> Vec<Result<ModelEvent, loom_kernel::model::ModelError>> {
    stream.collect().await
}

fn expect_completed(
    events: Vec<Result<ModelEvent, loom_kernel::model::ModelError>>,
) -> loom_kernel::model::ModelResponse {
    let Some(Ok(ModelEvent::Completed(response))) = events.last() else {
        panic!("expected a completed response, got {events:?}");
    };
    response.clone()
}

fn expect_error(
    events: &[Result<ModelEvent, loom_kernel::model::ModelError>],
) -> &loom_kernel::model::ModelError {
    match events.last() {
        Some(Err(error)) => error,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn joined_text(events: &[Result<ModelEvent, loom_kernel::model::ModelError>]) -> String {
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
    mount_sse(&server, text_response()).await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    assert_eq!(joined_text(&events), "Hello from Anthropic");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    // 100 billed input tokens, 40 of them cache reads: the cache share
    // is reported separately, the input count excludes it.
    assert_eq!(response.usage.input_tokens, 60);
    assert_eq!(response.usage.cached_input_tokens, Some(40));
    assert_eq!(response.usage.output_tokens, 4);
    assert_eq!(response.usage.cost_usd, None);
}

#[tokio::test]
async fn auth_headers_and_the_wire_shape_are_sent() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response()).await;

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&plain_request("claude-haiku-4-5"))).await;

    let requests = server.received_requests().await.expect("captured request");
    let request = &requests[0];
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(header("x-api-key"), "test-key");
    assert_eq!(header("anthropic-version"), "2023-06-01");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    // Anthropic requires max_tokens; the adapter fills a default.
    assert_eq!(body["max_tokens"], 8_192);
    assert_eq!(body["model"], "claude-haiku-4-5");
    assert_eq!(body["messages"][0]["role"], "user");
    // No system message in this request: the param stays absent.
    assert!(body.get("system").is_none());
}

#[tokio::test]
async fn a_stable_prefix_places_a_cache_breakpoint() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response()).await;

    let mut request = plain_request("claude-sonnet-4-5");
    request.messages = vec![
        Message::System {
            text: "be brief".into(),
        },
        Message::User { text: "hi".into() },
    ];
    // The whole projection is stable: the breakpoint lands on the
    // system block, the last message the hint covers.
    request.stable_prefix_messages = Some(2);

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&request)).await;

    let requests = server.received_requests().await.expect("captured request");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
    // Uncovered nothing: the system block stays a plain string; the
    // breakpoint marks the LAST message the hint covers — the user
    // turn — so the cache runs to the end of the stable region.
    assert_eq!(body["system"], "be brief");
    assert_eq!(
        body["messages"][0]["content"],
        serde_json::json!([{
            "type": "text",
            "text": "hi",
            "cache_control": {"type": "ephemeral"}
        }])
    );
}

#[tokio::test]
async fn in_turn_system_messages_survive_the_round_trip() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response()).await;

    let mut request = plain_request("claude-opus-5-5");
    request.messages = vec![
        Message::System {
            text: "global".into(),
        },
        Message::User {
            text: "task".into(),
        },
        // Per-turn steering form (Fable 5.1+): must stay in the
        // messages array, not fold into the top-level `system`.
        Message::System {
            text: "output_config: effort high".into(),
        },
    ];
    request.stable_prefix_messages = Some(2);

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&request)).await;

    let requests = server.received_requests().await.expect("captured request");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["system"], serde_json::json!("global"));
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["messages"][1]["role"], serde_json::json!("system"));
    assert_eq!(
        body["messages"][1]["content"][0]["text"],
        "output_config: effort high"
    );
    // The breakpoint marks the stable user turn, not the steering
    // message after it.
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        serde_json::json!({"type": "ephemeral"})
    );
    assert!(
        body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
}

#[tokio::test]
async fn a_55_thinking_stream_accounts_thinking_and_cache_writes() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            serde_json::json!({
                "type": "message_start",
                "message": {"usage": {
                    "input_tokens": 100,
                    "cache_read_input_tokens": 40,
                    "cache_creation_input_tokens": 10
                }}
            }),
            serde_json::json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
            serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "pondering"}
            }),
            serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig-1"}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({
                "type": "content_block_start", "index": 1,
                "content_block": {"type": "text", "text": ""}
            }),
            serde_json::json!({
                "type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": "Done"}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 1}),
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 50, "output_tokens_details": {"thinking_tokens": 30}}
            }),
            serde_json::json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-opus-5-5"))).await;

    let response = expect_completed(events);
    assert_eq!(response.message.content, "Done");
    assert_eq!(
        response.message.reasoning.as_ref().map(|r| r.text.as_str()),
        Some("pondering")
    );
    assert_eq!(response.usage.input_tokens, 60);
    assert_eq!(response.usage.cached_input_tokens, Some(40));
    assert_eq!(response.usage.cache_creation_input_tokens, Some(10));
    assert_eq!(response.usage.thinking_tokens, Some(30));
    assert_eq!(response.usage.output_tokens, 50);
}

#[tokio::test]
async fn tool_call_streams_assemble_into_tool_calls() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            serde_json::json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 12}}
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file"}
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"sr"}
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "c/lib.rs\"}"}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 9}
            }),
            serde_json::json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    let expected = vec![ToolCall {
        id: ToolCallId::try_from("toolu_1".to_owned()).expect("valid id"),
        name: "read_file".to_owned(),
        arguments: ToolArguments::try_from(serde_json::json!({"path": "src/lib.rs"}))
            .expect("object arguments"),
    }];
    assert_eq!(response.message.tool_calls, expected);
    // The AssistantMessage equality in the kernel compares the full
    // struct; spot-check the remaining fields for sanity.
    assert_eq!(response.message.content, "");
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(response.usage.output_tokens, 9);
}

#[tokio::test]
async fn rate_limited_responses_are_classified() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "Number of requests"}
        })))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, loom_kernel::model::ModelErrorKind::RateLimited);
    assert!(error.message.contains("Number of requests"));
}

#[tokio::test]
async fn overloaded_responses_map_to_overloaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_json(serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        })))
        .mount(&server)
        .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    let error = expect_error(&events);
    assert_eq!(error.kind, loom_kernel::model::ModelErrorKind::Overloaded);
}

#[tokio::test]
async fn a_truncated_stream_reports_length_without_calls() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            serde_json::json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 5}}
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file"}
            }),
            // The JSON argument cuts mid-string: it must be discarded,
            // not parsed.
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"sr"}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "max_tokens"},
                "usage": {"output_tokens": 8}
            }),
            serde_json::json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let provider = provider_at(&server.uri());
    let events = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
    assert!(response.message.tool_calls.is_empty());
    assert_eq!(response.usage.output_tokens, 8);
}

#[tokio::test]
async fn context_editing_hint_maps_to_clear_tool_uses_and_the_beta_header() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response()).await;

    let mut request = plain_request("claude-sonnet-4-5");
    request.context_editing = Some(ContextEditingHint {
        trigger_tokens: 30_000,
        keep_tool_uses: 3,
        clear_at_least_tokens: Some(5_000),
        exclude_tools: vec![String::from("scout")],
    });

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&request)).await;

    let requests = server.received_requests().await.expect("captured request");
    let request = &requests[0];
    let beta = request
        .headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(beta, "context-management-2025-06-27");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    // Every knob explicit: replaying these bytes reconstructs the
    // behavior without unstated provider defaults.
    assert_eq!(
        body["context_management"],
        serde_json::json!({
            "edits": [{
                "type": "clear_tool_uses_20250919",
                "trigger": {"type": "input_tokens", "value": 30000},
                "keep": {"type": "tool_uses", "value": 3},
                "clear_at_least": {"type": "input_tokens", "value": 5000},
                "exclude_tools": ["scout"]
            }]
        })
    );
}

#[tokio::test]
async fn no_context_editing_sends_no_context_management_and_no_beta_header() {
    let server = MockServer::start().await;
    mount_sse(&server, text_response()).await;

    let provider = provider_at(&server.uri());
    let _ = collect(provider.stream(&plain_request("claude-sonnet-4-5"))).await;

    let requests = server.received_requests().await.expect("captured request");
    let request = &requests[0];
    let beta = request
        .headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(beta, "", "default-off must not send the beta header");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
    assert!(
        body.get("context_management").is_none(),
        "default-off must not send context_management"
    );
}

#[tokio::test]
async fn capabilities_come_from_the_static_table() {
    let provider = provider_at("http://unused.local");
    let sonnet = provider
        .capabilities("claude-sonnet-4-5")
        .expect("known model");
    assert!(sonnet.tool_calls);
    assert!(sonnet.reasoning);
    assert_eq!(sonnet.max_context_tokens, 200_000);
    // Unknown models fail loud rather than guessing.
    assert!(provider.capabilities("claude-unknown").is_none());
}
