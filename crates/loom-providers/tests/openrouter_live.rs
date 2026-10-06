//! Live OpenRouter API verification.
//!
//! These tests hit `https://openrouter.ai/api/v1` and use the live model
//! catalog. They are `#[ignore]`d, so the default `cargo test` never
//! touches the network:
//!
//! ```text
//! OPENROUTER_API_KEY=sk-or-… cargo test -p loom-providers \
//!     --test openrouter_live -- --ignored --nocapture
//! ```
//!
//! The streaming tests deliberately select a **free** tool-capable model
//! from the live catalog, so a run spends no credit — it does consume the
//! account-wide free-request quota (20/min, plus a daily cap). `:free`
//! catalog variants churn; the tests skip loudly (rather than fail) when
//! the catalog advertises no suitable model. Never commit keys or
//! secret-bearing output.

use futures::StreamExt;
use loom_kernel::model::{
    Message, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
};
use loom_kernel::tool::ToolSpec;
use loom_providers::{
    OPENROUTER_DEFAULT_BASE_URL, OpenRouterCatalog, OpenRouterConfig, OpenRouterProvider,
    TimeoutConfig, fetch_catalog,
};
use serde_json::json;

/// Live key from the environment; fail loudly when absent.
fn api_key() -> String {
    match std::env::var("OPENROUTER_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => panic!("OPENROUTER_API_KEY must be set for live tests"),
    }
}

/// The live catalog, fetched with production defaults.
#[expect(clippy::expect_used)]
async fn catalog() -> OpenRouterCatalog {
    fetch_catalog(
        OPENROUTER_DEFAULT_BASE_URL,
        &api_key(),
        TimeoutConfig::default(),
    )
    .await
    .expect("catalog fetch")
}

/// The first free, tool-capable model with a usable context window.
///
/// Returns `None` when the live catalog advertises none (a legitimate
/// state — free variants churn), so tests can skip instead of failing.
fn free_tool_model(catalog: &OpenRouterCatalog) -> Option<&str> {
    catalog
        .models
        .iter()
        .find(|model| {
            model.is_free() == Some(true)
                && model.tool_capable
                && model
                    .context_length
                    .is_some_and(|tokens| tokens >= 64 * 1024)
        })
        .map(|model| model.id.as_str())
}

#[expect(clippy::unwrap_used)]
fn provider(catalog: OpenRouterCatalog) -> OpenRouterProvider {
    OpenRouterProvider::new(OpenRouterConfig::new(api_key()), catalog).unwrap()
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
#[ignore = "live API: free; requires OPENROUTER_API_KEY"]
async fn catalog_reports_free_and_paid_models() {
    let catalog = catalog().await;
    println!(
        "catalog: {} models, fetched_at {}",
        catalog.len(),
        catalog.fetched_at
    );
    assert!(!catalog.is_empty(), "live catalog should not be empty");
    let free = catalog
        .models
        .iter()
        .filter(|model| model.is_free() == Some(true))
        .count();
    let paid = catalog
        .models
        .iter()
        .filter(|model| model.is_free() == Some(false))
        .count();
    println!("free models: {free}, paid models: {paid}");
    assert!(free > 0, "expected at least one free model");

    // Capabilities resolve for advertised models; unknown ids do not.
    let sample = &catalog.models[0];
    println!("sample: {sample:?}");
    if let Some(capabilities) = catalog.capabilities(&sample.id) {
        println!("sample capabilities: {capabilities:?}");
    }
    assert!(catalog.capabilities("definitely/not-a-model").is_none());
}

#[tokio::test]
#[ignore = "live API: free; requires OPENROUTER_API_KEY"]
async fn free_model_plain_roundtrip() {
    let catalog = catalog().await;
    let Some(model) = free_tool_model(&catalog).map(str::to_owned) else {
        println!("skipped: the live catalog advertises no free tool-capable model");
        return;
    };
    println!("model: {model}");
    let provider = provider(catalog);
    let request = plain_request(&model, "Reply with exactly one word: pong");
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    assert!(!completed.message.content.is_empty());
    assert!(completed.usage.input_tokens > 0);
    assert!(completed.usage.output_tokens > 0);
    // Free usage reports zero or nothing at all — never a charge.
    if let Some(cost) = completed.usage.cost_usd {
        assert_eq!(cost, 0.0, "a free model must not report a charge");
    }
}

#[tokio::test]
#[ignore = "live API: free; requires OPENROUTER_API_KEY"]
async fn free_model_accepts_tools() {
    let catalog = catalog().await;
    let Some(model) = free_tool_model(&catalog).map(str::to_owned) else {
        println!("skipped: the live catalog advertises no free tool-capable model");
        return;
    };
    println!("model: {model}");
    let provider = provider(catalog);
    let mut request = plain_request(&model, "Call the list_files tool. Do not answer in prose.");
    request.tools = vec![list_files_tool()];
    let events = collect(provider.stream(&request)).await;
    let completed = expect_completed(events);
    // Free upstreams are sloppy: either a tool call or a prose answer is
    // acceptable, but a tool call must carry a usable argument object.
    for call in &completed.message.tool_calls {
        let arguments: &serde_json::Value = &call.arguments;
        assert!(
            arguments.is_object(),
            "tool call arguments must be an object, got {arguments:?}"
        );
    }
}
