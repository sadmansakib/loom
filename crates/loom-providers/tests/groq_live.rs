//! Live Groq API verification.
//!
//! These tests hit `https://api.groq.com/openai/v1` and are `#[ignore]`d,
//! so the default `cargo test` never touches the network:
//!
//! Either run `loom auth login groq` once, or export the key:
//!
//! ```text
//! GROQ_API_KEY=gsk_… cargo test -p loom-providers \
//!     --test groq_live -- --ignored --nocapture
//! ```
//!
//! Groq's free plan is unbilled but hard-capped at 8,000 tokens/min and
//! 200,000 tokens/day — tokens, not requests — so the prompts here are
//! deliberately tiny and the tests run sequentially. The free plan serves
//! only the three agentic deployments this adapter knows; a `404` here
//! usually means the plan changed, not that the adapter regressed. Never
//! commit keys or secret-bearing output.

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
};
use loom_kernel::tool::ToolSpec;
use loom_providers::{GroqConfig, GroqProvider, TimeoutConfig};
use serde_json::json;

mod live_key;

/// Live key from the loom account store or `GROQ_API_KEY`; fail loudly
/// when absent.
fn api_key() -> String {
    live_key::live_key("groq", "GROQ_API_KEY")
}

/// The model exercised when the caller does not pick one. `qwen/qwen3.8-27b`
/// is the only row in the adapter's table that issues parallel tool calls.
fn default_model() -> String {
    match std::env::var("GROQ_LIVE_MODEL") {
        Ok(model) if !model.is_empty() => model,
        _ => "qwen/qwen3.8-27b".to_owned(),
    }
}

#[expect(clippy::expect_used)]
fn provider() -> GroqProvider {
    GroqProvider::new(GroqConfig::new(api_key()).with_timeouts(TimeoutConfig::default()))
        .expect("provider construction")
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
    println!("tool_calls:   {}", completed.message.tool_calls.len());
    println!("content:      {:?}", completed.message.content);
    println!(
        "reasoning:    {:?}",
        completed
            .message
            .reasoning
            .as_ref()
            .map(|r| r.text.as_str())
            .unwrap_or_default()
    );
    println!("usage:        {:?}", completed.usage);
    println!("cost_usd:     {:?}", completed.usage.cost_usd);
    completed
}

fn plain_request(model: &str, prompt: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        messages: vec![Message::User {
            text: prompt.into(),
        }],
        tools: vec![],
        max_tokens: Some(512),
        temperature: None,
        stable_prefix_messages: None,
        context_editing: None,
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

fn read_file_tool() -> ToolSpec {
    ToolSpec {
        name: "read_file".into(),
        description: "Read one file and return its contents.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

#[tokio::test]
#[ignore = "live API: free plan; requires GROQ_API_KEY"]
async fn plain_roundtrip_reports_reasoning_and_usage() {
    let model = default_model();
    println!("model: {model}");
    let provider = provider();
    let request = plain_request(&model, "Reply with exactly one word: pong");
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    assert!(!completed.message.content.is_empty());
    assert!(completed.usage.input_tokens > 0);
    assert!(completed.usage.output_tokens > 0);
    // The free plan is unbilled, so a charge would be an upstream change
    // worth seeing rather than a silent budget hole.
    println!(
        "reasoning captured: {}",
        completed.message.reasoning.is_some()
    );
}

/// The load-bearing check for this adapter: a tool-call turn whose assistant
/// message is replayed verbatim. Groq 400s on the shared `reasoning_content`
/// key, so this only passes if both the request builder and the chunk
/// accumulator honor the dialect's `reasoning` key.
#[tokio::test]
#[ignore = "live API: free plan; requires GROQ_API_KEY"]
async fn tool_call_turn_replays_without_a_400() {
    let model = default_model();
    println!("model: {model}");
    let provider = provider();
    let mut request = plain_request(&model, "Call the list_files tool. Do not answer in prose.");
    request.tools = vec![list_files_tool()];
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    assert!(
        !completed.message.tool_calls.is_empty(),
        "expected at least one tool call"
    );
    for call in &completed.message.tool_calls {
        println!("call: {} {:?}", call.id, call.name);
        assert!(
            call.arguments.is_object(),
            "tool call arguments must be an object, got {:?}",
            call.arguments
        );
    }

    // Replay: feed the assistant turn back exactly as a second turn would.
    let mut messages = request.messages.clone();
    messages.push(Message::Assistant(completed.message.clone()));
    let mut followup = plain_request(&model, "");
    followup.messages = messages;
    followup.tools = vec![list_files_tool()];
    println!("replaying {} messages", followup.messages.len());
    let events = collect(provider.stream(&followup)).await;
    let replayed = expect_completed(events);
    assert!(replayed.usage.input_tokens > 0);
}

/// `qwen/qwen3.8-27b` is the one Groq row that parallelizes; the `gpt-oss`
/// rows serialize, so this asserts against the model's pinned capability
/// rather than against the wire.
#[tokio::test]
#[ignore = "live API: free plan; requires GROQ_API_KEY"]
async fn parallel_tool_calls_follow_the_pinned_capability() {
    let model = default_model();
    println!("model: {model}");
    let Some(capabilities) = loom_providers::groq_model_capabilities(&model) else {
        panic!("{model} should resolve capabilities; unknown ids are a bug here");
    };
    println!("capabilities: {capabilities:?}");
    let provider = provider();
    let mut request = plain_request(
        &model,
        "Call list_files and read_file. Do not answer in prose.",
    );
    request.tools = vec![list_files_tool(), read_file_tool()];
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    for call in &completed.message.tool_calls {
        println!("call: {} {:?}", call.id, call.name);
    }
    if capabilities.parallel_tool_calls {
        assert!(
            completed.message.tool_calls.len() >= 2,
            "{} claims parallel tool calls but issued {}",
            model,
            completed.message.tool_calls.len()
        );
    } else {
        assert!(
            completed.message.tool_calls.len() <= 1,
            "{} is pinned to serial tool calls but issued {}",
            model,
            completed.message.tool_calls.len()
        );
    }
}
