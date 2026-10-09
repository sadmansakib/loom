//! Live DeepSeek API verification (research.md §4.1 UNVERIFIED list).
//!
//! These tests hit `https://api.deepseek.com` and spend real tokens
//! (bounded by small `max_tokens`). Every test is `#[ignore]`d, so the
//! default `cargo test` never touches the network:
//!
//! ```text
//! DEEPSEEK_API_KEY=sk-… cargo test -p loom-providers \
//!     --test deepseek_live -- --ignored --nocapture
//! ```
//!
//! Observations are recorded as sanitized facts in `docs/research.md`;
//! never commit API keys or recorded output containing secrets. Never
//! point these at the GLM Coding Plan endpoint.

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse, StopReason,
};
use loom_kernel::tool::ToolSpec;
use loom_kernel::transcript::{ToolCallId, ToolResult, ToolStatus};
use loom_providers::{DeepSeekConfig, DeepSeekProvider, ReasoningEffort, ThinkingMode};
use serde_json::json;

/// Live key from the environment; fail loudly when absent.
fn api_key() -> String {
    match std::env::var("DEEPSEEK_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => panic!("DEEPSEEK_API_KEY must be set for live tests"),
    }
}

#[expect(clippy::expect_used)]
fn provider(effort: ReasoningEffort) -> DeepSeekProvider {
    let config = DeepSeekConfig::new(api_key()).with_reasoning_effort(effort);
    DeepSeekProvider::new(config).expect("TLS init")
}

async fn collect(
    stream: impl futures::Stream<Item = Result<ModelEvent, ModelError>>,
) -> Vec<Result<ModelEvent, ModelError>> {
    let mut out = Vec::new();
    let mut stream = Box::pin(stream);
    while let Some(item) = stream.next().await {
        out.push(item);
    }
    out
}

/// The single authoritative `Completed` event of a successful request.
#[expect(clippy::unwrap_used)]
fn expect_completed(events: Vec<Result<ModelEvent, ModelError>>) -> ModelResponse {
    let completed = events
        .into_iter()
        .filter_map(|e| e.ok())
        .find_map(|e| match e {
            ModelEvent::Completed(response) => Some(response),
            _ => None,
        })
        .unwrap();
    println!("stop_reason:  {:?}", completed.message.stop_reason);
    println!(
        "reasoning:    {} bytes",
        completed
            .message
            .reasoning
            .as_ref()
            .map_or(0, |r| r.text.len())
    );
    println!("content:      {:?}", completed.message.content);
    println!("usage:        {:?}", completed.usage);
    completed
}

fn plain_request(prompt: &str) -> ModelRequest {
    ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![Message::User {
            text: prompt.into(),
        }],
        tools: vec![],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    }
}

fn list_files_tool() -> ToolSpec {
    ToolSpec {
        name: "list_files".into(),
        description: "List file names in the current directory.".into(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn plain_roundtrip_default_effort() {
    let provider = provider(ReasoningEffort::default());
    let request = plain_request("Reply with exactly one word: pong");
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    assert_eq!(completed.message.stop_reason, StopReason::EndTurn);
    assert!(!completed.message.content.is_empty());
    assert!(completed.usage.input_tokens > 0);
    assert!(completed.usage.output_tokens > 0);
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn effort_none_plain_roundtrip() {
    // Pins the none + thinking:enabled pairing (research.md §4.1):
    // research says the fields are independent; if the API rejects the
    // pair this fails with the 400 body in the error message.
    let provider = provider(ReasoningEffort::None);
    let request = plain_request("Reply with exactly one word: pong");
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    assert_eq!(completed.message.stop_reason, StopReason::EndTurn);
    assert!(!completed.message.content.is_empty());
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn effort_none_tools_two_turns() {
    // Pins two UNVERIFIED replay questions at once (research.md §4.1):
    // 1. a no-reasoning assistant turn replayed WITHOUT reasoning_content
    //    (current build_request_body omits the field) — 400 would fail
    //    here with the body in the error message;
    // 2. a tool-call-only assistant turn replayed with "content": "".
    let provider = provider(ReasoningEffort::None);
    let turn1 = ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![Message::User {
            text: "Call list_files exactly once. Do not say anything else.".into(),
        }],
        tools: vec![list_files_tool()],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&turn1)).await;
    let first = expect_completed(events);
    assert_eq!(first.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(first.message.tool_calls.len(), 1);
    // Live observation (research.md §4.1): `reasoning_effort: "none"`
    // does NOT suppress reasoning while thinking stays enabled — the
    // model emitted ~76 bytes of reasoning anyway. So the replay below
    // carries reasoning (the normal path), and this test pins the
    // tool-call-only `"content": ""` replay instead.

    // Replay: the stored assistant turn verbatim (reasoning included,
    // content: "" for the tool-call-only turn) + one tool result.
    let call = &first.message.tool_calls[0];
    let turn2 = ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![
            Message::User {
                text: "Call list_files exactly once. Do not say anything else.".into(),
            },
            Message::Assistant(first.message.clone()),
            Message::ToolResult(ToolResult::new(
                ToolCallId::try_from(call.id.as_str().to_string()).expect("id"),
                ToolStatus::Success,
                "Cargo.toml\nCargo.lock\ncrates",
                false,
            )),
            Message::User {
                text: "Now reply with exactly one word: done".into(),
            },
        ],
        tools: vec![list_files_tool()],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&turn2)).await;
    let second = expect_completed(events);
    assert_eq!(second.message.stop_reason, StopReason::EndTurn);
    assert!(!second.message.content.is_empty());

    // Turn 3: replay the no-reasoning assistant turn (finalize stored it
    // as `reasoning: None`, so build_request_body omits reasoning_content
    // for it). Pins the omit-vs-empty question for genuinely
    // no-reasoning turns; a 400 fails here with the body printed above.
    let turn3 = ModelRequest {
        model: "deepseek-flash".into(),
        messages: turn2
            .messages
            .into_iter()
            .chain([
                Message::Assistant(second.message.clone()),
                Message::User {
                    text: "Reply with exactly one word: done".into(),
                },
            ])
            .collect(),
        tools: vec![list_files_tool()],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&turn3)).await;
    let third = expect_completed(events);
    assert_eq!(third.message.stop_reason, StopReason::EndTurn);
    assert!(!third.message.content.is_empty());
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn thinking_disabled_plain_roundtrip() {
    // Pins the real off switch (research.md §4.1): `thinking: disabled`
    // is the separate non-thinking mode, and this request pairs it with
    // an explicit `reasoning_effort` to probe whether the two fields are
    // truly independent. If the API rejects the pair, this fails with
    // the 400 body in the error message.
    let config = DeepSeekConfig::new(api_key())
        .with_reasoning_effort(ReasoningEffort::High)
        .with_thinking(ThinkingMode::Disabled);
    let provider = DeepSeekProvider::new(config).expect("provider builds");
    let request = plain_request("Reply with exactly one word: pong");
    let events = collect(provider.stream(&request)).await;
    // Non-thinking mode: no reasoning may stream back, ever.
    for event in &events {
        assert!(
            !matches!(event, Ok(ModelEvent::ReasoningDelta(_))),
            "thinking disabled must not stream reasoning: {event:?}"
        );
    }
    let completed = expect_completed(events);
    assert_eq!(completed.message.stop_reason, StopReason::EndTurn);
    assert!(!completed.message.content.is_empty());
    assert!(completed.usage.output_tokens > 0);
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn effort_low_and_max_roundtrips() {
    // The two remaining effort values on deepseek-flash. Reasoning-delta
    // counts are printed but not asserted: whether the model reasons on
    // a trivial prompt is the model's choice, not the contract's.
    for effort in [ReasoningEffort::Low, ReasoningEffort::Max] {
        let provider = provider(effort);
        let request = plain_request("Reply with exactly one word: pong");
        let events = collect(provider.stream(&request)).await;
        let reasoning_deltas = events
            .iter()
            .filter(|e| matches!(e, Ok(ModelEvent::ReasoningDelta(_))))
            .count();
        println!("effort {effort:?}: {reasoning_deltas} reasoning deltas");
        let completed = expect_completed(events);
        assert_eq!(completed.message.stop_reason, StopReason::EndTurn);
        assert!(!completed.message.content.is_empty());
    }
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn parallel_calls_replay_roundtrip() {
    // Pins multi-tool-call replay: an assistant turn carrying TWO calls
    // (duplicated from a real one, distinct ids) plus two results, tools
    // still present. The capability table advertises parallel tool calls;
    // this pins the wire shape the API accepts back.
    let provider = provider(ReasoningEffort::None);
    let turn1 = ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![Message::User {
            text: "Call list_files exactly once. Do not say anything else.".into(),
        }],
        tools: vec![list_files_tool()],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&turn1)).await;
    let first = expect_completed(events);
    assert_eq!(first.message.stop_reason, StopReason::ToolCalls);

    let mut call_b = first.message.tool_calls[0].clone();
    call_b.id = ToolCallId::try_from(format!("{}-second", call_b.id.as_str())).expect("id");
    let id_b = call_b.id.clone();
    // The replayed assistant turn carries both calls.
    let mut two_calls = first.message.clone();
    two_calls.tool_calls.push(call_b);
    let messages: Vec<Message> = vec![
        Message::User {
            text: "Call list_files exactly once. Do not say anything else.".into(),
        },
        Message::Assistant(two_calls),
        Message::ToolResult(ToolResult::new(
            first.message.tool_calls[0].id.clone(),
            ToolStatus::Success,
            "Cargo.toml\nCargo.lock\ncrates",
            false,
        )),
        Message::ToolResult(ToolResult::new(id_b, ToolStatus::Success, "docs", false)),
        Message::User {
            text: "Now reply with exactly one word: done".into(),
        },
    ];

    let turn2 = ModelRequest {
        model: "deepseek-flash".into(),
        messages,
        tools: vec![list_files_tool()],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&turn2)).await;
    let second = expect_completed(events);
    assert_eq!(second.message.stop_reason, StopReason::EndTurn);
    assert!(!second.message.content.is_empty());
}

#[tokio::test]
#[ignore = "live API: costs tokens; requires DEEPSEEK_API_KEY"]
async fn invalid_model_error_body() {
    // Records the 4xx error-body shape (research.md §4.1 UNVERIFIED):
    // run with --nocapture and read the printed error message.
    let provider = provider(ReasoningEffort::default());
    let request = ModelRequest {
        model: "no-such-model".into(),
        messages: vec![Message::User { text: "hi".into() }],
        tools: vec![],
        max_tokens: Some(8),
        temperature: None,
        stable_prefix_messages: None,
    };
    let events = collect(provider.stream(&request)).await;
    let error = events
        .into_iter()
        .find_map(|e| e.err())
        .expect("invalid model must fail");
    println!("kind:    {:?}", error.kind);
    println!("message: {}", error.message);
    assert!(!error.kind.is_retryable());
}

#[tokio::test]
#[ignore = "live API: no cost; requires a bogus key (real key NOT used)"]
async fn bad_api_key_error_body() {
    let config = DeepSeekConfig::new("sk-invalid-key-for-error-shape-probe");
    let provider = DeepSeekProvider::new(config).expect("TLS init");
    let request = plain_request("hi");
    let events = collect(provider.stream(&request)).await;
    let error = events
        .into_iter()
        .find_map(|e| e.err())
        .expect("invalid key must fail");
    println!("kind:    {:?}", error.kind);
    println!("message: {}", error.message);
    assert!(!error.kind.is_retryable());
}
