//! Integration tests for the DeepSeek adapter against a real HTTP stack:
//! wiremock for full request/response exchanges, a raw TCP server where
//! wiremock cannot help (mid-body stalls, disconnect detection).
//!
//! Covers the plan's 1C list: streamed text/reasoning/tool-call merging,
//! usage on the last content chunk, the HTTP-status error table, the
//! in-stream failure mapping, every architecture §5.1 normalization row
//! over the wire, keep-alive comments, request-body shape (thinking,
//! explicit reasoning effort, tool_choice only with tools, reasoning
//! replay when tools are sent), and the liveness/cancellation contract
//! (drop = request cancelled, stalls = retryable Transport).

use std::time::Duration;

use futures::StreamExt;
use loom_core::model::{
    Message, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest, StopReason,
};
use loom_core::tool::ToolSpec;
use loom_core::transcript::{
    AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use loom_providers::{DeepSeekConfig, DeepSeekProvider, TimeoutConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Provider pointed at `base_url` with the given liveness timeouts.
#[expect(clippy::unwrap_used, reason = "test-only TLS init")]
fn provider_at(base_url: &str, timeouts: TimeoutConfig) -> DeepSeekProvider {
    DeepSeekProvider::new(
        DeepSeekConfig::new("test-key")
            .with_base_url(base_url)
            .with_timeouts(timeouts),
    )
    .unwrap()
}

/// A minimal user request with no tools.
fn plain_request() -> ModelRequest {
    ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![Message::User { text: "hi".into() }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
    }
}

/// Serializes one chunk payload as a `data:` event body.
fn data_event(payload: Value) -> String {
    payload.to_string()
}

/// Joins `data:` events into an SSE body.
fn sse(events: &[String]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(event);
        body.push_str("\n\n");
    }
    body
}

/// The `[DONE]` terminator event.
fn done() -> String {
    "data: [DONE]\n\n".into()
}

/// Mounts a 200 response streaming the given SSE body.
async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            // set_body_raw, not insert_header + set_body_string: the
            // latter's text/plain would override the header, which the
            // content-type gate rejects.
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Drains a provider stream to a Vec.
async fn collect(stream: loom_core::model::ModelStream<'_>) -> Vec<Result<ModelEvent, ModelError>> {
    stream.collect().await
}

/// The stream must have ended with `Completed`; returns the response.
fn expect_completed(
    events: Vec<Result<ModelEvent, ModelError>>,
) -> loom_core::model::ModelResponse {
    match events.into_iter().last() {
        Some(Ok(ModelEvent::Completed(response))) => response,
        other => panic!("expected stream to end with Completed, got {other:?}"),
    }
}

/// The stream must have ended with an error; returns it.
fn expect_error(events: Vec<Result<ModelEvent, ModelError>>) -> ModelError {
    match events.into_iter().last() {
        Some(Err(error)) => error,
        other => panic!("expected stream to end with an error, got {other:?}"),
    }
}

/// Concatenates every `TextDelta` payload.
fn joined_text(events: &[Result<ModelEvent, ModelError>]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelEvent::TextDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn streams_text_deltas_then_completed_end_turn() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"Hel"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"lo"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}],
                                "usage":{"prompt_tokens":10,"completion_tokens":5,
                                         "prompt_cache_hit_tokens":4}})),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hello");
    let response = expect_completed(events);
    assert_eq!(response.message.content, "Hello");
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    assert_eq!(response.message.tool_calls.len(), 0);
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
    assert_eq!(response.usage.cached_input_tokens, Some(4));
}

#[tokio::test]
async fn streams_reasoning_deltas_and_keeps_reasoning_text() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"reasoning_content":"Think"}}]})),
            data_event(json!({"choices":[{"delta":{"reasoning_content":"ing…"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"answer"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}],
                                "usage":{"prompt_tokens":1,"completion_tokens":1}})),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelEvent::ReasoningDelta(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "Thinking…");
    let response = expect_completed(events);
    let trace = response.message.reasoning.expect("reasoning present");
    assert_eq!(trace.text, "Thinking…");
    assert!(trace.provider_data.is_none());
    assert_eq!(response.message.content, "answer");
}

#[tokio::test]
async fn merges_tool_call_fragments_into_completed_response() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            // First fragment carries id + name and the start of arguments.
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"read_file","arguments":"{\"pa"}}]}}]})),
            // Continuation: arguments only.
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"th\":\"x.rs\"}"}}]}}]})),
            // Second call with no id: the adapter must synthesize one.
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"function":{"name":"list_dir","arguments":"{}"}}]}}]})),
            data_event(
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],
                                "usage":{"prompt_tokens":100,"completion_tokens":20}}),
            ),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    // Tool fragments are never streamed: the only event is Completed.
    assert_eq!(events.len(), 1, "no tool deltas may leak: {events:?}");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    let calls = &response.message.tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id.as_str(), "call_a");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x.rs"}"#);
    assert!(
        calls[1].id.as_str().starts_with("deepseek-call-"),
        "got {}",
        calls[1].id.as_str()
    );
    assert_eq!(calls[1].name, "list_dir");
    // Usage without prompt_cache_hit_tokens: cache miss.
    assert_eq!(response.usage.cached_input_tokens, None);
}

#[tokio::test]
async fn trailing_junk_after_done_is_ignored() {
    // Some proxies append bytes after [DONE] within the same body read;
    // the response is final the moment [DONE] is seen — trailing
    // non-JSON must not turn a completed response into a Protocol
    // error, and trailing JSON must not mutate it.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"Hi"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]) + &done()
            + "data: not-json\n\ndata: {\"evil\":true}\n\n",
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hi");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn stop_with_calls_normalizes_to_tool_calls() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"read_file","arguments":"{}"}}]}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
}

#[tokio::test]
async fn done_without_finish_reason_normalizes_structurally() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{"content":"done talking"}}]}),
        )]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn stream_ending_without_done_is_retryable_transport() {
    let server = MockServer::start().await;
    // A complete-looking exchange whose [DONE] never arrives: wiremock
    // closes the connection after the body.
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"partial"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(
        joined_text(&events),
        "partial",
        "deltas observed before the close"
    );
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn length_with_partial_args_discards_calls() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"read_file","arguments":"{\"path\": \"x.rs"}}]}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"length"}]})),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
    assert!(
        response.message.tool_calls.is_empty(),
        "partially-generated calls must never be executed"
    );
}

#[tokio::test]
async fn unparseable_args_is_protocol_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"read_file","arguments":"not json"}}]}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Protocol);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn in_stream_insufficient_system_resource_is_overloaded() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"half"}}]})),
            data_event(
                json!({"choices":[{"delta":{},"finish_reason":"insufficient_system_resource"}]}),
            ),
        ]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Overloaded);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn in_stream_aborted_is_transport() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"aborted"}]}),
        )]) + &done(),
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn keep_alive_comments_are_ignored() {
    let server = MockServer::start().await;
    let body = ": keep-alive\n\n".to_string()
        + &sse(&[
            data_event(json!({"choices":[{"delta":{"content":"Hel"}}]})),
            data_event(json!({"choices":[{"delta":{"content":"lo"}}]})),
        ])
        + ": keep-alive\n\n"
        + &done()
        + ": keep-alive\n\n";
    mount_sse(&server, body).await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hello");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn http_error_status_classification() {
    let cases = [
        (400, ModelErrorKind::InvalidRequest, false),
        (401, ModelErrorKind::Auth, false),
        (402, ModelErrorKind::QuotaExhausted, false),
        (408, ModelErrorKind::Transport, true),
        (422, ModelErrorKind::InvalidRequest, false),
        (429, ModelErrorKind::RateLimited, true),
        (500, ModelErrorKind::Server, true),
        (503, ModelErrorKind::Overloaded, true),
    ];
    for (status, kind, retryable) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string(r#"{"error":{"message":"boom"}}"#),
            )
            .mount(&server)
            .await;

        let events =
            collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
                .await;
        let error = expect_error(events);
        assert_eq!(error.kind, kind, "HTTP {status}");
        assert_eq!(error.is_retryable(), retryable, "HTTP {status}");
        assert!(
            error.message.contains("boom"),
            "error body excerpt missing for HTTP {status}: {}",
            error.message
        );
    }
}

#[tokio::test]
async fn non_sse_200_body_is_protocol_error() {
    // Captive portals / proxies answer 200 with an HTML page; without
    // the content-type check that surfaces as retryable Transport
    // ("stream ended without [DONE]") and the retry loop re-sends a
    // request that can never succeed.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<html>sign in</html>".as_bytes().to_vec(), "text/html"),
        )
        .mount(&server)
        .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Protocol);
    assert!(!error.is_retryable());
    assert!(
        error.message.contains("text/html"),
        "content type should be in the message: {}",
        error.message
    );
}

#[tokio::test]
async fn uppercase_content_type_is_accepted() {
    // MIME types are case-insensitive (RFC 9110 §8.3); a proxy may
    // rewrite the header case. The stream must still decode.
    let server = MockServer::start().await;
    let body = sse(&[
        data_event(json!({"choices":[{"delta":{"content":"Hi"}}]})),
        data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
    ]) + &done();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "Text/Event-Stream"),
        )
        .mount(&server)
        .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hi");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn done_without_blank_line_at_eof_completes() {
    // Gateways may close right after `data: [DONE]` without the
    // closing blank line. The decoder never dispatches that event
    // (spec: incomplete at EOF), so the narrow EOF tolerance peeks it:
    // a complete response must complete, not burn the retry budget.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"Hi"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]) + "data: [DONE]\n", // no closing blank line
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hi");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn done_without_newline_at_eof_completes() {
    // Even narrower: a gateway that closes right after the bytes
    // `data: [DONE]` — no newline at all. `pending_event` cannot see
    // it (no complete line), so the raw trailing bytes are matched:
    // complete response completes instead of retrying to the limit.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[
            data_event(json!({"choices":[{"delta":{"content":"Hi"}}]})),
            data_event(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
        ]) + "data: [DONE]", // no newline at all
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    assert_eq!(joined_text(&events), "Hi");
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn pending_event_with_unterminated_done_still_fails_retryably() {
    // A gateway that skips blank lines holds back every data event in
    // the decoder; the unterminated `data: [DONE]` tolerance must NOT
    // fire over an undispatched event — that chunk (content, finish
    // reason, usage) was never accumulated, so accepting it would
    // return an empty response as success. Retryable Transport error
    // instead.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[]) // no blank-line-separated events at all
            + "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\
               data: [DONE]", // single newline, then EOF
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn event_truncated_at_eof_still_fails_retryably() {
    // The tolerance is narrow: a stream cut mid-event (here, inside the
    // final content delta) leaves pending bytes that are NOT the
    // terminator — still a retryable Transport error, never a silent
    // partial response.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(json!({"choices":[{"delta":{"content":"Hi"}}]}))])
            + "data: {\"choices\":[{\"delta\":{\"content\":\"wor", // cut mid-event
    )
    .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn retry_after_header_is_honoured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "7")
                .set_body_string(r#"{"error":{"message":"slow down"}}"#),
        )
        .mount(&server)
        .await;

    let events =
        collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request()))
            .await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::RateLimited);
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
}

#[tokio::test]
async fn request_body_shape_and_reasoning_replay() {
    // Part 1: plain request — thinking flag, explicit effort, usage
    // opt-in, no tool fields.
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;
    collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&plain_request())).await;
    let requests = server.received_requests().await.expect("captured request");
    let plain_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(plain_body["model"], "deepseek-flash");
    assert_eq!(plain_body["stream"], true);
    assert_eq!(plain_body["stream_options"]["include_usage"], true);
    assert_eq!(plain_body["thinking"], json!({"type": "enabled"}));
    assert_eq!(plain_body["reasoning_effort"], "high");
    assert!(plain_body.get("tools").is_none());
    assert!(plain_body.get("tool_choice").is_none());
    assert!(plain_body.get("max_tokens").is_none());

    // Part 2: tools present — tool_choice appears, every prior
    // assistant reasoning_content is replayed (plain and tool turns
    // alike: the DeepSeek contract makes omitting one an HTTP 400 when
    // tools are sent).
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
        messages: vec![
            Message::System { text: "sys".into() },
            Message::Assistant(AssistantMessage {
                content: "plan first".into(),
                reasoning: Some(ReasoningTrace {
                    text: "plan".into(),
                    provider_data: None,
                }),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
            }),
            Message::Assistant(AssistantMessage {
                content: "reading".into(),
                reasoning: Some(ReasoningTrace {
                    text: "step".into(),
                    provider_data: None,
                }),
                tool_calls: vec![call],
                stop_reason: StopReason::ToolCalls,
            }),
            Message::ToolResult(ToolResult {
                call_id: ToolCallId::try_from("call_1".to_owned()).unwrap(),
                status: ToolStatus::Success,
                content: "file body".into(),
                truncated: false,
            }),
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "reads a file".into(),
            parameters: json!({"type": "object"}),
        }],
        max_tokens: Some(4096),
        ..plain_request()
    };
    collect(provider_at(&server.uri(), TimeoutConfig::default()).stream(&request)).await;
    let requests = server.received_requests().await.expect("captured request");
    let tool_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(tool_body["tool_choice"], "auto");
    assert_eq!(tool_body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(tool_body["max_tokens"], 4096);
    assert_eq!(tool_body["messages"][1]["reasoning_content"], "plan");
    assert_eq!(tool_body["messages"][2]["reasoning_content"], "step");
    assert_eq!(tool_body["messages"][2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(tool_body["messages"][3]["role"], "tool");
}

#[tokio::test]
async fn header_stall_is_retryable_transport() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Accept the connection but never send headers.
        let mut buf = [0u8; 64];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let timeouts = TimeoutConfig {
        connect: Duration::from_secs(5),
        header: Duration::from_millis(100),
        idle: Duration::from_secs(5),
    };
    let events =
        collect(provider_at(&format!("http://{addr}"), timeouts).stream(&plain_request())).await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
    assert!(
        error.message.contains("headers"),
        "should name the header phase: {}",
        error.message
    );
}

#[tokio::test]
async fn idle_stall_is_retryable_transport() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                  data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
            )
            .await
            .unwrap();
        // Deliver one event, then stall with the connection open.
        let mut buf = [0u8; 64];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let timeouts = TimeoutConfig {
        connect: Duration::from_secs(5),
        header: Duration::from_secs(5),
        idle: Duration::from_millis(100),
    };
    let events =
        collect(provider_at(&format!("http://{addr}"), timeouts).stream(&plain_request())).await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
    assert!(
        error.message.contains("idle"),
        "should name the idle phase: {}",
        error.message
    );
}

#[tokio::test]
async fn trailing_slash_base_url_still_hits_the_endpoint() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse(&[data_event(
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        )]) + &done(),
    )
    .await;

    let provider = provider_at(&(server.uri() + "/"), TimeoutConfig::default());
    let events = collect(provider.stream(&plain_request())).await;
    let response = expect_completed(events);
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn malformed_base_url_is_invalid_request() {
    // A builder failure (no scheme) is a configuration mistake and must
    // not be retried.
    let events =
        collect(provider_at("not-a-url", TimeoutConfig::default()).stream(&plain_request())).await;
    let error = expect_error(events);
    assert_eq!(error.kind, ModelErrorKind::InvalidRequest);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn stalled_error_body_keeps_status_kind_500_is_retryable_server() {
    stalled_error_body_asserts_kind(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2048\r\n\r\n",
        ModelErrorKind::Server,
        true,
    )
    .await;
}

#[tokio::test]
async fn stalled_error_body_keeps_status_kind_401_is_not_retryable() {
    // A stalled body must not downgrade a permanent failure into a
    // retryable one: retrying a bad key forever serves nobody.
    stalled_error_body_asserts_kind(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2048\r\n\r\n",
        ModelErrorKind::Auth,
        false,
    )
    .await;
}

/// Error headers, then a body that never arrives. Whatever the status
/// says, the classification must follow it, with a note that no
/// excerpt could be read.
#[expect(clippy::unwrap_used)]
async fn stalled_error_body_asserts_kind(
    headers: &'static str,
    kind: ModelErrorKind,
    retryable: bool,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(headers.as_bytes()).await.unwrap();
        let mut buf = [0u8; 64];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let timeouts = TimeoutConfig {
        connect: Duration::from_secs(5),
        header: Duration::from_secs(5),
        idle: Duration::from_millis(100),
    };
    let events =
        collect(provider_at(&format!("http://{addr}"), timeouts).stream(&plain_request())).await;
    let error = expect_error(events);
    assert_eq!(error.kind, kind);
    assert_eq!(error.is_retryable(), retryable);
    assert!(
        error.message.contains("error body unread"),
        "should note the stalled body: {}",
        error.message
    );
}

#[tokio::test]
async fn dropping_the_stream_cancels_the_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                  data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
            )
            .await
            .unwrap();
        // Hold the stream open until the client goes away.
        let mut buf = [0u8; 64];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = closed_tx.send(());
    });

    let provider = provider_at(&format!("http://{addr}"), TimeoutConfig::default());
    let request = plain_request();
    let mut stream = provider.stream(&request);
    let first = stream.next().await;
    assert!(
        matches!(&first, Some(Ok(ModelEvent::TextDelta(text))) if text == "par"),
        "expected one delta before dropping, got {first:?}"
    );
    // Cancellation is drop: the request must be aborted.
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("server should observe the disconnect within 5s")
        .unwrap();
}
