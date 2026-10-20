//! The Groq adapter: Groq-hosted inference on the OpenAI-compatible
//! wire, plus Groq error classification and a capability table.
//!
//! Wire behaviour follows the Groq API reference (console.groq.com/docs,
//! fetched 2026-10-18): `POST /openai/v1/chat/completions` at
//! [`DEFAULT_BASE_URL`] with Bearer `GROQ_API_KEY`, `data: [DONE]`
//! terminator, usage on the final chunk, nested
//! `{"error": {"message", "type"}}` objects, `retry-after` on 429 only,
//! and `x-ratelimit-*` headers on every chat completion
//! (`limit-requests` = RPD, `limit-tokens` = TPM). Every wire claim in
//! this module was live-verified 2026-10-18 against the free plan with
//! a real key, not read off the docs alone.
//!
//! **Why this adapter exists.** The eval backlog's blocker is funded
//! variance, not missing models (roadmap, item 1). Groq's free plan
//! (30 RPM / 1K RPD / 8K TPM / 200K TPD) carries the open coding and
//! reasoning models the backlog wants, at $0, with no credit card — but
//! **8K tokens/minute is the binding constraint**, not the request
//! count. A single agent turn on a large context can exhaust it, so
//! sweeps must pace on tokens, not just `LOOM_EVAL_RPM`.
//!
//! Two wire deviations need adapter code, both live-verified:
//!
//! 1. **The reasoning field is `reasoning`, not `reasoning_content`.**
//!    Groq streams the trace as `delta.reasoning` *and* rejects the
//!    standard key on a replayed assistant message with HTTP 400
//!    (`'messages.1' : property 'reasoning_content' is unsupported`),
//!    so the shared replay cannot merely omit the field — it has to
//!    write it under Groq's name. See
//!    [`crate::openai_wire::REASONING_FIELD_PLAIN`].
//! 2. **Mid-stream failures ride in the `x_groq` envelope**, not the
//!    OpenAI-standard top-level `error` key. Read by the shared chunk
//!    parser via [`crate::openai_wire::ChatChunk::x_groq`].
//!
//! Two more, documented because they change measurements rather than
//! requests: `temperature: 0` is silently rewritten to `1e-8` (so a
//! greedy intent never actually decodes greedily — one reason free-tier
//! turn counts scatter), and `delta.channel` appears on gpt-oss chunks
//! (ignored as an unknown field).
//!
//! Reasoning effort is a single top-level `reasoning_effort` field.
//! The docs list eight values, but the models disagree: gpt-oss accepts
//! exactly `low`/`medium`/`high` and answers HTTP 400 for `none`,
//! `max`, `minimal`, and `default` (live-verified). Every pinned row is
//! therefore clamped to that closed set — gpt-oss has no off switch, and
//! one uniform contract beats a per-model table that would have to be
//! re-verified on every catalog move.
//!
//! Parallel tool calls are **not** supported by the gpt-oss rows
//! (live-verified: one call per turn even when the request offered two),
//! and are supported by the Qwen row (live-verified: two calls in one
//! turn). The Llama rows that used to carry Groq's catalog
//! (`llama-3.1-8b-instant`, `llama-3.3-70b-versatile`) were shut down on
//! 2026-08-16 for free and developer tiers and are deliberately absent.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};

use serde_json::Value;

use crate::deepseek::ReasoningEffort;
use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{REASONING_FIELD_PLAIN, WireOptions, build_request_body};
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

/// Default Groq API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "groq";

/// Configuration for [`GroqProvider`].
#[derive(Debug, Clone)]
pub struct GroqConfig {
    /// API base URL; override for tests or proxies.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`], ADR-009).
    /// Reading the key from the environment is the caller's (CLI /
    /// account store) job, not the provider's.
    pub api_key: SecretString,
    /// Mapped onto the top-level `reasoning_effort` field per model;
    /// see the module docs for the clamp.
    pub reasoning_effort: ReasoningEffort,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl GroqConfig {
    /// Creates a config with the default base URL, default timeouts,
    /// default reasoning effort, and the given key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: SecretString::new(api_key),
            reasoning_effort: ReasoningEffort::default(),
            timeouts: TimeoutConfig::default(),
        }
    }

    /// Overrides the base URL (builder style).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Overrides the reasoning effort (builder style).
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Overrides the timeouts (builder style).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// The Groq [`ModelProvider`] adapter.
///
/// `stream` uses `tokio::time::timeout`, so it must be polled inside a
/// tokio runtime with the time driver enabled; polling it outside one
/// panics.
#[derive(Debug, Clone)]
pub struct GroqProvider {
    config: Arc<GroqConfig>,
    client: reqwest::Client,
}

impl GroqProvider {
    /// Creates a provider with a connection-pooled HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized.
    pub fn new(config: GroqConfig) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }

    /// Classifies an HTTP status per the Groq error model.
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`].
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

/// Capability table from the Groq model pages (fetched 2026-10-18 and
/// live-checked the same day against `GET /openai/v1/models` plus a real
/// tool-calling turn per row). Every pinned row has tools and
/// reasoning; the gpt-oss rows serialize tool calls, the Qwen row does
/// not. Unknown ids resolve to `None` (loud unknown-model failure, not
/// a silently wrong row).
///
/// Deliberately absent from `/openai/v1/models` on the free plan:
/// `meta-llama/llama-3.1-8b-instant` and
/// `meta-llama/llama-3.3-70b-versatile` (shut down 2026-08-16 for free
/// and developer tiers). Also absent by design: the moderation
/// (`*-safeguard-*`, `llama-prompt-guard-*`), audio (`whisper-*`), and
/// single-language rows (`allam-2-7b`, `canopylabs/orpheus-*`) — none
/// speak the chat-completions tool protocol this adapter needs, and a
/// row that resolves would promise capabilities the model lacks.
pub fn model_capabilities(model: &str) -> Option<ModelCapabilities> {
    let limits = match model {
        // gpt-oss: reasoning models with tools, but one call per turn
        // (live-verified against both sizes).
        "openai/gpt-oss-120b" | "openai/gpt-oss-20b" => ModelCapabilities::new(131_072)
            .with_max_output_tokens(65_536)
            .with_tool_calls(true)
            .with_parallel_tool_calls(false)
            .with_reasoning(true),
        // Qwen 3.8 27B: tools and parallel tool calls both live-verified
        // (two calls emitted in one turn).
        "qwen/qwen3.8-27b" => ModelCapabilities::new(131_072)
            .with_max_output_tokens(16_384)
            .with_tool_calls(true)
            .with_parallel_tool_calls(true)
            .with_reasoning(true),
        _ => return None,
    };
    Some(limits)
}

/// Resolves the `reasoning_effort` string for `effort`.
///
/// Clamps onto the closed set every pinned Groq row accepts. Live:
/// gpt-oss-120b answers HTTP 400
/// (``reasoning_effort` must be one of `low`, `medium`, or `high` ``)
/// for `none` and `max`, so Loom's four efforts map onto three wire
/// values — `none` onto `low`, the nearest available depth, mirroring
/// the forced-thinking clamp the NIM adapter applies to GLM.
fn effort_for(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Low => "low",
        ReasoningEffort::High | ReasoningEffort::Max => "high",
    }
}

impl WireEndpoint for GroqProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn endpoint_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn api_key(&self) -> &str {
        self.config.api_key.expose()
    }

    fn request_body(&self, request: &ModelRequest) -> Value {
        let mut extra_body = serde_json::Map::new();
        extra_body.insert(
            "reasoning_effort".to_owned(),
            serde_json::json!(effort_for(self.config.reasoning_effort)),
        );
        build_request_body(
            request,
            &WireOptions {
                extra_body,
                reasoning_field: Some(REASONING_FIELD_PLAIN),
            },
        )
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        Vec::new()
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// Groq documents no in-stream failure `finish_reason` values (the
    /// stream reasons are `stop`, `length`, `tool_calls`,
    /// `function_call`); none are mapped, so an unknown reason reads as
    /// why the model stopped. A real mid-stream failure arrives as
    /// `x_groq.error` instead.
    fn in_stream_failure(_reason: &str) -> Option<ModelErrorKind> {
        None
    }

    /// Groq's reasoning dialect (`reasoning`, not `reasoning_content`),
    /// on both the request and the stream read.
    fn reasoning_field(&self) -> Option<&'static str> {
        Some(REASONING_FIELD_PLAIN)
    }

    /// Classifies an HTTP status per the Groq error model: OpenAI-style
    /// nested error objects, HTTP-status-led, plus the two Groq-specific
    /// codes — 498 (flex tier at capacity, retryable) and 499 (caller
    /// cancelled, not retryable).
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        let kind = match status.as_u16() {
            400 | 404 | 422 => ModelErrorKind::InvalidRequest,
            401 | 403 => ModelErrorKind::Auth,
            402 | 429 => ModelErrorKind::RateLimited,
            408 => ModelErrorKind::Transport,
            498 => ModelErrorKind::Overloaded,
            499 => ModelErrorKind::Cancelled,
            503 => ModelErrorKind::Overloaded,
            _ if status.is_client_error() => ModelErrorKind::InvalidRequest,
            _ => ModelErrorKind::Server,
        };
        ModelError::new(kind, format!("HTTP {status}: {}", body_excerpt(body)))
    }
}

impl ModelProvider for GroqProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        model_capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        stream_endpoint(self, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_table_current_models() {
        for model in ["openai/gpt-oss-120b", "openai/gpt-oss-20b"] {
            let caps = model_capabilities(model).expect("known model");
            assert_eq!(caps.max_context_tokens, 131_072, "{model}");
            assert_eq!(caps.max_output_tokens, Some(65_536), "{model}");
            assert!(caps.tool_calls && caps.reasoning, "{model}");
            // Live-verified: gpt-oss emits one call per turn.
            assert!(!caps.parallel_tool_calls, "{model}");
        }
        let qwen = model_capabilities("qwen/qwen3.8-27b").expect("known model");
        assert_eq!(qwen.max_context_tokens, 131_072);
        assert_eq!(qwen.max_output_tokens, Some(16_384));
        assert!(qwen.parallel_tool_calls, "live-verified parallel call");
    }

    #[test]
    fn capability_table_unknown_models_are_none() {
        // Live-verified 2026-10-18: shut down for free/developer tiers.
        for model in [
            "llama-3.3-70b-versatile",
            "meta-llama/llama-3.3-70b-versatile",
            "llama-3.1-8b-instant",
            "meta-llama/llama-3.1-8b-instant",
        ] {
            assert!(
                model_capabilities(model).is_none(),
                "decommissioned row {model}"
            );
        }
        // Present in the catalog but not chat/tool-capable rows.
        for model in [
            "openai/gpt-oss-safeguard-20b",
            "meta-llama/llama-prompt-guard-2-22m",
            "whisper-large-v3",
            "allam-2-7b",
            "canopylabs/orpheus-v1-english",
            "gpt-oss-120b",
            "",
        ] {
            assert!(model_capabilities(model).is_none(), "non-agent row {model}");
        }
    }

    #[test]
    fn efforts_clamp_onto_the_closed_groq_set() {
        // gpt-oss rejects `none` and `max` with HTTP 400 (live).
        assert_eq!(effort_for(ReasoningEffort::None), "low");
        assert_eq!(effort_for(ReasoningEffort::Low), "low");
        assert_eq!(effort_for(ReasoningEffort::High), "high");
        assert_eq!(effort_for(ReasoningEffort::Max), "high");
    }

    #[test]
    fn default_base_url_is_the_hosted_groq_endpoint() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.groq.com/openai/v1");
        let config = GroqConfig::new("groq-secret-value");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("groq-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn request_body_uses_the_plain_reasoning_field() {
        let provider = GroqProvider::new(GroqConfig::new("groq-secret-value")).expect("client");
        let request = ModelRequest {
            model: "openai/gpt-oss-120b".to_owned(),
            messages: vec![loom_kernel::model::Message::Assistant(
                loom_kernel::transcript::AssistantMessage {
                    content: "ok".to_owned(),
                    reasoning: Some(loom_kernel::transcript::ReasoningTrace {
                        text: "because".to_owned(),
                        provider_data: None,
                    }),
                    tool_calls: Vec::new(),
                    stop_reason: loom_kernel::model::StopReason::EndTurn,
                },
            )],
            tools: Vec::new(),
            max_tokens: Some(64),
            temperature: None,
            stable_prefix_messages: None,
        };
        let body = provider.request_body(&request);
        assert_eq!(body["reasoning_effort"], "high", "default effort");
        let assistant = &body["messages"][0];
        assert_eq!(
            assistant["reasoning"], "because",
            "replay must use Groq's field name"
        );
        assert!(
            assistant.get("reasoning_content").is_none(),
            "Groq rejects reasoning_content on an assistant message"
        );
    }

    #[test]
    fn status_codes_classify_per_the_groq_table() {
        let invalid = GroqProvider::classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error": {"message": "`reasoning_effort` must be one of `low`, `medium`, or `high`"}}"#,
        );
        assert_eq!(invalid.kind, ModelErrorKind::InvalidRequest);
        assert!(invalid.message.contains("reasoning_effort"));

        let auth = GroqProvider::classify_status(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error": {"message": "bad key"}}"#,
        );
        assert_eq!(auth.kind, ModelErrorKind::Auth);
        assert!(!auth.is_retryable());

        let rate = GroqProvider::classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "rate limit reached",
        );
        assert_eq!(rate.kind, ModelErrorKind::RateLimited);
        assert!(rate.is_retryable());

        // Groq-specific: flex tier at capacity is retryable overload.
        let flex = GroqProvider::classify_status(
            reqwest::StatusCode::from_u16(498).expect("498"),
            "flex capacity",
        );
        assert_eq!(flex.kind, ModelErrorKind::Overloaded);
        assert!(flex.is_retryable());

        // Groq-specific: caller cancelled is terminal.
        let cancelled = GroqProvider::classify_status(
            reqwest::StatusCode::from_u16(499).expect("499"),
            "cancelled",
        );
        assert_eq!(cancelled.kind, ModelErrorKind::Cancelled);
        assert!(!cancelled.is_retryable());
    }
}
