//! Provider-neutral model abstractions.
//!
//! This module owns everything that describes a conversation with a language
//! model: the request shape ([`ModelRequest`], [`Message`]), the streaming
//! seam ([`ModelProvider`], [`ModelStream`], [`ModelEvent`]), the
//! authoritative result ([`ModelResponse`]), a model's descriptive
//! capabilities ([`ModelCapabilities`]), and error classification
//! ([`ModelError`], [`ModelErrorKind`]).
//!
//! Nothing here mentions a concrete provider: wire formats
//! (OpenAI-compatible, Anthropic, …) are translated by adapters in
//! `loom-providers` before values of these types exist.

use crate::tool::ToolSpec;
use crate::transcript::{AssistantMessage, ToolResult};
use serde::{Deserialize, Serialize};

/// Token accounting for one model request, or accumulated over a task.
///
/// Provider adapters fill this from the wire response; the agent loop sums
/// it per task. `cached_input_tokens` is `Option` because not every
/// provider reports cache hits.
///
/// Part of the event wire format (via `ModelCompleted`): adding a field
/// bumps `EVENT_SCHEMA_VERSION` in `crate::event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the request prompt.
    pub input_tokens: u64,
    /// Tokens the model generated.
    pub output_tokens: u64,
    /// Prompt tokens served from a provider-side cache, if reported.
    pub cached_input_tokens: Option<u64>,
}

impl std::ops::Add for Usage {
    type Output = Usage;

    /// Sums two usage records. Cache hits merge per the accumulation
    /// rules: `Some(a) + Some(b) = Some(a + b)`, and a single `Some` wins
    /// over `None`. Note this means a total built from a mix of
    /// providers that do and don't report cache hits shows a partial
    /// count; consumers comparing cache ratios must account for that.
    fn add(self, rhs: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            cached_input_tokens: match (self.cached_input_tokens, rhs.cached_input_tokens) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
        }
    }
}

/// Why the model stopped generating in a response that completed
/// successfully.
///
/// A stop reason describes *only* the model's own stopping condition. It is
/// deliberately distinct from failure modes that are not the model's
/// decision at all:
///
/// - cancellation is signalled by a `CancellationToken`, not a stop reason;
/// - protocol / transport failures are `ModelError`s (e.g. DeepSeek's
///   `finish_reason: "aborted"` is a transport error and
///   `"insufficient_system_resource"` an overload, not stop reasons).
///
/// Semantics (iteration 1):
///
/// | Variant | Loop behaviour |
/// |---|---|
/// | [`StopReason::EndTurn`] | terminal success |
/// | [`StopReason::ToolCalls`] | continue the loop, execute the calls |
/// | [`StopReason::MaxTokens`] | task failure (continuation recovery later) |
/// | [`StopReason::ContextLimit`] | task failure (compaction recovery later) |
/// | [`StopReason::ContentFilter`] | task failure |
///
/// Normalization from raw wire `finish_reason` values (e.g. `"stop"` emitted
/// alongside tool calls) is the **adapter's** duty; core only ever sees
/// structurally consistent values: `ToolCalls` without calls or `EndTurn`
/// with calls is a protocol error.
///
/// "No tool calls" is *not* synonymous with success — completion is decided
/// from this value and task state.
///
/// Part of the event wire format (via `ModelCompleted`): adding a variant
/// bumps `EVENT_SCHEMA_VERSION`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer; no further work was requested.
    EndTurn,
    /// The model requested tool executions before continuing.
    ToolCalls,
    /// Generation hit the output token limit.
    ///
    /// Partial tool-call arguments are discarded by the adapter and never
    /// executed when this is the stop reason.
    MaxTokens,
    /// The request exceeded the model's context window.
    ContextLimit,
    /// The model's output was blocked by a content filter.
    ///
    /// Persisted in the transcript (inspectable) but excluded from context
    /// compilation for later requests.
    ContentFilter,
}

/// One provider-neutral message in a [`ModelRequest`].
///
/// This is the *model-visible* projection of a conversation — built by the
/// context compiler from durable transcript [`Item`](crate::transcript::Item)s,
/// never persisted directly. Unlike an `Item`, a message can be a
/// [`Message::System`] prompt (which the transcript does not store), and the
/// projection may legitimately differ from the transcript (e.g. refused
/// `ContentFilter` turns are never replayed, §8.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Stable instruction prefix; cached by providers, so it should be
    /// deterministic for a given task.
    System {
        /// The system prompt text.
        text: String,
    },
    /// User input.
    User {
        /// The user's text, exactly as received.
        text: String,
    },
    /// An assistant turn, projected verbatim from the transcript (including
    /// stored reasoning when the compiler chooses to replay it).
    Assistant(AssistantMessage),
    /// The outcome of one tool call, projected verbatim from the
    /// transcript. Order within the request follows original tool-call
    /// order so transcript construction stays deterministic (§10.1).
    ToolResult(ToolResult),
}

impl Message {
    /// Returns the message's role name as used in the serde `role` tag
    /// (`"system"`, `"user"`, `"assistant"`, `"tool_result"`).
    #[must_use]
    pub fn role(&self) -> &'static str {
        match self {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "tool_result",
        }
    }
}

/// A single request to a model, produced by the context compiler.
///
/// All fields are provider-neutral; the adapter translates them into its
/// wire format. The compiler owns which transcript items and tools are
/// included (§8) — the agent loop never builds a `ModelRequest` itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Provider-specific model identifier (e.g. `"deepseek-flash"`). The
    /// provider resolves it to concrete [`ModelCapabilities`] (§6.4).
    pub model: String,
    /// The full projected conversation: system prompt first, then turns in
    /// transcript order.
    pub messages: Vec<Message>,
    /// Tool definitions exposed for this request. Deliberately explicit per
    /// request (§8.2): the compiler decides the set once per task so the
    /// cached prompt prefix stays stable.
    pub tools: Vec<ToolSpec>,
    /// Hard cap on generated tokens, if the caller sets one.
    pub max_tokens: Option<u32>,
    /// Sampling temperature, if the caller overrides the provider default.
    pub temperature: Option<f32>,
}

/// What a model is capable of — descriptive facts, not policy (§3).
///
/// Resolved **per model** via [`ModelProvider::capabilities`]; the core uses
/// these to avoid sending unsupported requests (e.g. tools to a model
/// without `tool_calls`) and to shape future context decisions. It never
/// encodes provider *behaviour* — no retry quirks, no wire details.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Maximum tokens the model accepts as request context.
    pub max_context_tokens: u32,
    /// Maximum tokens the model can generate in one response, if known.
    pub max_output_tokens: Option<u32>,
    /// Whether the model supports tool calls at all.
    pub tool_calls: bool,
    /// Whether the model may be asked for several tool calls in one turn.
    pub parallel_tool_calls: bool,
    /// Whether the model emits reasoning traces.
    ///
    /// A `bool` for now; grows into a mode enum only when the compiler must
    /// choose between reasoning modes (§6.4).
    pub reasoning: bool,
}

impl ModelCapabilities {
    /// Creates a capabilities value with conservative defaults: no known
    /// output limit and every optional feature off.
    ///
    /// `#[non_exhaustive]` means callers outside this crate cannot use a
    /// struct literal, so provider adapters build values through this
    /// constructor and the `with_*` methods.
    pub const fn new(max_context_tokens: u32) -> Self {
        Self {
            max_context_tokens,
            max_output_tokens: None,
            tool_calls: false,
            parallel_tool_calls: false,
            reasoning: false,
        }
    }

    /// Sets the maximum output tokens (builder style).
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = Some(max);
        self
    }

    /// Sets whether the model supports tool calls (builder style).
    #[must_use]
    pub const fn with_tool_calls(mut self, enabled: bool) -> Self {
        self.tool_calls = enabled;
        self
    }

    /// Sets whether the model may be asked for several tool calls in one
    /// turn (builder style).
    #[must_use]
    pub const fn with_parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = enabled;
        self
    }

    /// Sets whether the model emits reasoning traces (builder style).
    #[must_use]
    pub const fn with_reasoning(mut self, enabled: bool) -> Self {
        self.reasoning = enabled;
        self
    }
}

/// One event from a streaming model request.
///
/// Streamed deltas are **provisional** (§6.3): they exist for live display
/// only and are never committed to the transcript. The single
/// [`ModelEvent::Completed`] at the end of a successful stream is the only
/// authoritative output. A successful stream yields exactly one
/// `Completed` as its final item — anything after it, or end-of-stream
/// without it, is an error.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    /// A fragment of visible assistant text, for display only.
    TextDelta(String),
    /// A fragment of reasoning text, for display only.
    ReasoningDelta(String),
    /// The authoritative end of a successful request: complete content,
    /// fully-assembled tool calls, stop reason, and usage. Consumers never
    /// reconstruct this from deltas.
    Completed(ModelResponse),
}

/// The authoritative result of one successful model request (§6.2).
///
/// Carried as the final [`ModelEvent::Completed`]. The stop reason is
/// reachable as `message.stop_reason` — it is part of the assistant message
/// because the transcript persists it there (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// The complete assistant message, exactly as it will be offered to the
    /// transcript.
    pub message: AssistantMessage,
    /// Token accounting for this request, as reported by the provider.
    pub usage: Usage,
}

/// Stable classification of a model request failure (§6.2).
///
/// Retry decisions are made on this classification (plus provider metadata
/// like `retry_after`), never by matching error strings. Classification is
/// set by the provider adapter when the error is built.
///
/// Part of the event wire format (via `ModelRetry`): adding a variant
/// bumps `EVENT_SCHEMA_VERSION` in `crate::event`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    /// Authentication failed (bad or expired credentials).
    Auth,
    /// The request itself was malformed or unsupported. Never retried.
    InvalidRequest,
    /// The request exceeded the model's context window. Never retried
    /// (future: compaction recovery).
    ContextOverflow,
    /// Rate limited by the provider. Retryable, honouring `retry_after`.
    RateLimited,
    /// The provider reports overload. Retryable.
    Overloaded,
    /// The account's quota is exhausted. Never retried.
    QuotaExhausted,
    /// Network / connection failure, including liveness timeouts
    /// (§6.3). Retryable.
    Transport,
    /// The provider returned a server-side error. Retryable.
    Server,
    /// The provider's response violated the protocol contract (e.g.
    /// `finish_reason: "tool_calls"` with zero calls). Never retried.
    Protocol,
    /// The request was cancelled. Never retried — a cancelled request is
    /// not a failure to recover from.
    ///
    /// Built **only by the core**, after the task's cancellation token
    /// wins the `select!` against the stream (§6.6). Adapters never emit
    /// this kind: their cancellation path is the stream being dropped,
    /// and a dropped stream can no longer yield items.
    Cancelled,
}

impl ModelErrorKind {
    /// Whether the retry policy may retry a failure of this kind (§6.3):
    /// `rate_limited`, `overloaded`, `transport`, `server` are retryable;
    /// every other kind is terminal for the request.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ModelErrorKind::RateLimited
                | ModelErrorKind::Overloaded
                | ModelErrorKind::Transport
                | ModelErrorKind::Server
        )
    }
}

impl std::fmt::Display for ModelErrorKind {
    /// Renders the stable snake_case name, matching the serde form, so
    /// `ModelError`'s Display (e.g. `[rate_limited] …`) lines up with
    /// serialized values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ModelErrorKind::Auth => "auth",
            ModelErrorKind::InvalidRequest => "invalid_request",
            ModelErrorKind::ContextOverflow => "context_overflow",
            ModelErrorKind::RateLimited => "rate_limited",
            ModelErrorKind::Overloaded => "overloaded",
            ModelErrorKind::QuotaExhausted => "quota_exhausted",
            ModelErrorKind::Transport => "transport",
            ModelErrorKind::Server => "server",
            ModelErrorKind::Protocol => "protocol",
            ModelErrorKind::Cancelled => "cancelled",
        };
        write!(f, "{name}")
    }
}

/// A model request failure with its stable classification (§6.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{kind}] {message}")]
pub struct ModelError {
    /// Stable classification; the retry policy reads only this (plus
    /// `retry_after`).
    pub kind: ModelErrorKind,
    /// Human-readable detail from the provider, for logs and the CLI.
    pub message: String,
    /// Provider-supplied hint for when a retry may proceed, overriding the
    /// policy's computed backoff (§6.3).
    pub retry_after: Option<std::time::Duration>,
}

impl ModelError {
    /// Builds an error of the given kind with a detail message.
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Attaches a `retry_after` hint (builder style).
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: std::time::Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Convenience: whether the retry policy may retry this failure.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

/// A lazily-streaming model response (§2).
///
/// `BoxStream` is the async seam: providers return this from
/// [`ModelProvider::stream`] and callers poll it with ordinary stream
/// combinators. Nothing is sent until the first poll; dropping the stream
/// cancels the underlying request (§6.6) — that drop is the only
/// cancellation path for model requests.
pub type ModelStream<'a> = futures::stream::BoxStream<'a, Result<ModelEvent, ModelError>>;

/// The provider seam (§2): everything the agent loop knows about a model
/// backend.
///
/// Implementations live in `loom-providers` (e.g. the DeepSeek adapter); the
/// core depends only on this trait. Cancellation is by *dropping* the
/// returned stream — no token is passed in (§6.6), so the trait carries no
/// redundant cancellation path.
pub trait ModelProvider: Send + Sync {
    /// Short provider identifier (e.g. `"deepseek"`), used in tracing and
    /// config.
    fn name(&self) -> &str;

    /// Capabilities of one model served by this provider.
    /// `None` = model unknown to the provider; the agent then uses the
    /// configured fallback or fails before the first request (§6.4).
    fn capabilities(&self, model: &str) -> Option<ModelCapabilities>;

    /// Starts a request and returns its event stream.
    ///
    /// Lazy: nothing is sent until first poll. Dropping the stream cancels
    /// the request (§6.6). A successful stream ends with exactly one
    /// [`ModelEvent::Completed`] as its final item.
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSpec;
    use crate::transcript::{ToolCallId, ToolStatus};

    #[test]
    fn serde_uses_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolCalls).unwrap(),
            "\"tool_calls\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ContentFilter).unwrap(),
            "\"content_filter\""
        );
    }

    #[test]
    fn round_trips_every_variant() {
        let all = [
            StopReason::EndTurn,
            StopReason::ToolCalls,
            StopReason::MaxTokens,
            StopReason::ContextLimit,
            StopReason::ContentFilter,
        ];
        for reason in all {
            let json = serde_json::to_string(&reason).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, back);
        }
    }

    /// A stub provider proving the trait is object-safe and matches the
    /// architecture's signatures.
    struct StubProvider;

    impl ModelProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
            if model == "known" {
                Some(ModelCapabilities {
                    max_context_tokens: 200_000,
                    max_output_tokens: Some(8_192),
                    tool_calls: true,
                    parallel_tool_calls: true,
                    reasoning: true,
                })
            } else {
                None
            }
        }

        fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn provider_trait_is_object_safe() {
        let provider: Box<dyn ModelProvider> = Box::new(StubProvider);
        assert_eq!(provider.name(), "stub");
        assert!(provider.capabilities("known").is_some());
        // Unknown model -> None (agent fails before the first request).
        assert!(provider.capabilities("nope").is_none());
    }

    #[test]
    fn message_roles_and_round_trip() {
        let messages = vec![
            Message::System {
                text: "You are loom.".into(),
            },
            Message::User {
                text: "read Cargo.toml".into(),
            },
            Message::Assistant(AssistantMessage {
                content: "Reading it.".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            }),
            Message::ToolResult(ToolResult::new(
                ToolCallId::try_from("call_1".to_owned()).unwrap(),
                ToolStatus::Success,
                "[package]",
                false,
            )),
        ];
        let expected_roles = ["system", "user", "assistant", "tool_result"];
        for (msg, role) in messages.iter().zip(expected_roles) {
            assert_eq!(msg.role(), role);
        }

        let json = serde_json::to_string(&messages).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(messages, back);

        // The serde role tag matches the role() names.
        assert!(json.contains(r#""role":"system""#));
    }

    #[test]
    fn model_request_round_trips() {
        let request = ModelRequest {
            model: "deepseek-flash".into(),
            messages: vec![Message::User { text: "hi".into() }],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Read a file.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: Some(1_024),
            temperature: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: ModelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request, back);
    }

    #[test]
    fn retryability_table_matches_architecture() {
        // Retryable (§6.3): rate_limited, overloaded, transport, server.
        assert!(ModelErrorKind::RateLimited.is_retryable());
        assert!(ModelErrorKind::Overloaded.is_retryable());
        assert!(ModelErrorKind::Transport.is_retryable());
        assert!(ModelErrorKind::Server.is_retryable());

        // Never retried: everything else.
        assert!(!ModelErrorKind::Auth.is_retryable());
        assert!(!ModelErrorKind::InvalidRequest.is_retryable());
        assert!(!ModelErrorKind::ContextOverflow.is_retryable());
        assert!(!ModelErrorKind::QuotaExhausted.is_retryable());
        assert!(!ModelErrorKind::Protocol.is_retryable());
        assert!(!ModelErrorKind::Cancelled.is_retryable());
    }

    #[test]
    fn model_error_carries_kind_message_and_retry_after() {
        let err = ModelError::new(ModelErrorKind::RateLimited, "slow down")
            .with_retry_after(std::time::Duration::from_secs(7));
        assert!(err.is_retryable());
        assert_eq!(err.retry_after, Some(std::time::Duration::from_secs(7)));
        assert_eq!(err.to_string(), "[rate_limited] slow down");

        let fatal = ModelError::new(ModelErrorKind::Auth, "bad key");
        assert!(!fatal.is_retryable());
        assert!(fatal.retry_after.is_none());
    }

    #[test]
    fn display_always_matches_serde_name() {
        // The hand-written Display table and #[serde(rename_all)] must
        // agree for every kind. `_assert_listed` below makes this list
        // exhaustive by construction: adding a variant without extending
        // it fails to compile (the enum is #[non_exhaustive], which only
        // affects other crates).
        let all = [
            ModelErrorKind::Auth,
            ModelErrorKind::InvalidRequest,
            ModelErrorKind::ContextOverflow,
            ModelErrorKind::RateLimited,
            ModelErrorKind::Overloaded,
            ModelErrorKind::QuotaExhausted,
            ModelErrorKind::Transport,
            ModelErrorKind::Server,
            ModelErrorKind::Protocol,
            ModelErrorKind::Cancelled,
        ];
        for kind in all {
            let serde_name = serde_json::to_value(kind).unwrap();
            assert_eq!(
                serde_name.as_str().unwrap(),
                kind.to_string(),
                "Display and serde disagree for {kind:?}"
            );
        }
    }

    // Compile error here if a variant is added but not listed above.
    fn _assert_listed(kind: ModelErrorKind) {
        match kind {
            ModelErrorKind::Auth
            | ModelErrorKind::InvalidRequest
            | ModelErrorKind::ContextOverflow
            | ModelErrorKind::RateLimited
            | ModelErrorKind::Overloaded
            | ModelErrorKind::QuotaExhausted
            | ModelErrorKind::Transport
            | ModelErrorKind::Server
            | ModelErrorKind::Protocol
            | ModelErrorKind::Cancelled => {}
        }
    }

    #[test]
    fn completed_event_carries_authoritative_response() {
        let response = ModelResponse {
            message: AssistantMessage {
                content: "done".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: Some(2),
            },
        };
        // Stop reason is reachable through the message, where the
        // transcript persists it.
        assert_eq!(response.message.stop_reason, StopReason::EndTurn);

        let event = ModelEvent::Completed(response);
        let ModelEvent::Completed(back) = event else {
            panic!("must be Completed");
        };
        assert_eq!(back.usage.input_tokens, 10);
    }
}
