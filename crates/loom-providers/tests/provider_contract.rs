//! Cross-crate contract tests for `loom-core`'s provider seams.
//!
//! These tests exist in `loom-providers` — not `loom-core` — on purpose:
//! they prove that a *different crate* can implement `ModelProvider` and
//! build the values it needs. `#[non_exhaustive]` types that only
//! `loom-core`'s own tests construct would hide construction gaps until
//! the real DeepSeek adapter hits them.

use loom_core::model::{
    Message, ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider,
    ModelRequest, ModelResponse, ModelStream, StopReason, Usage,
};
use loom_core::tool::ToolSpec;
use loom_core::transcript::AssistantMessage;

struct TestProvider;

impl ModelProvider for TestProvider {
    fn name(&self) -> &str {
        "test"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        match model {
            // Exercises the constructor + builders from outside the crate.
            "capable" => Some(
                ModelCapabilities::new(200_000)
                    .with_max_output_tokens(8_192)
                    .with_tool_calls(true)
                    .with_parallel_tool_calls(true)
                    .with_reasoning(true),
            ),
            _ => None,
        }
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let response = ModelResponse {
            message: AssistantMessage {
                content: format!("echo: {} message(s)", request.messages.len()),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage::default(),
        };
        Box::pin(futures::stream::iter([Ok(ModelEvent::Completed(response))]))
    }
}

#[test]
fn capabilities_buildable_outside_core() {
    let provider = TestProvider;
    let caps = provider.capabilities("capable").expect("known model");
    assert_eq!(caps.max_context_tokens, 200_000);
    assert_eq!(caps.max_output_tokens, Some(8_192));
    assert!(caps.tool_calls && caps.parallel_tool_calls && caps.reasoning);

    // Conservative defaults when nothing is opted in.
    let bare = ModelCapabilities::new(1_000);
    assert_eq!(bare.max_output_tokens, None);
    assert!(!bare.tool_calls);
    assert!(!bare.parallel_tool_calls);
    assert!(!bare.reasoning);

    assert!(provider.capabilities("unknown").is_none());
}

#[test]
fn request_buildable_and_streamable_outside_core() {
    let provider = TestProvider;
    let request = ModelRequest {
        model: "capable".into(),
        messages: vec![
            Message::System {
                text: "test".into(),
            },
            Message::User {
                text: "hello".into(),
            },
        ],
        tools: vec![ToolSpec {
            name: "noop".into(),
            description: "does nothing".into(),
            parameters: serde_json::json!({"type": "object"}),
        }],
        max_tokens: None,
        temperature: None,
    };

    let stream = provider.stream(&request);
    let events: Vec<_> = futures::executor::block_on_stream(stream)
        .collect::<Result<Vec<_>, ModelError>>()
        .expect("stream must succeed");

    // Exactly one Completed as the final item (§6.2).
    assert!(matches!(events.as_slice(), [ModelEvent::Completed(_)]));
}

#[test]
fn model_error_usable_outside_core() {
    let err = ModelError::new(ModelErrorKind::Overloaded, "busy");
    assert!(err.is_retryable());
    assert_eq!(err.to_string(), "[overloaded] busy");
}
