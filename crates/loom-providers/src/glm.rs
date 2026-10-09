//! The GLM adapter: Z.ai's **metered** API on the OpenAI-compatible
//! wire, plus GLM error classification and a capability table.
//!
//! Wire behaviour follows the Z.ai developer documentation
//! (docs.z.ai/api-reference/llm/chat-completion, fetched 2026-10-09):
//! `POST /chat/completions` with streamed `reasoning_content` and
//! tool-call fragments, usage on the final chunk
//! (`prompt_tokens_details.cached_tokens`,
//! `completion_tokens_details.reasoning_tokens`), `data: [DONE]`
//! terminator, OpenAI-style error objects. Verified by wiremock, not
//! live: no funded key.
//!
//! The usage block sticks to the OpenAI-standard shapes, so this
//! adapter adds no [`dialect_usage`](crate::wire_stream::WireEndpoint::dialect_usage)
//! override — the generic wire covers it.
//!
//! **Never the Coding Plan.** The subscription endpoint
//! (`https://api.z.ai/api/coding/paas/v4`) serves endorsed tools only
//! and is off-limits by its terms; this adapter speaks the metered
//! endpoint ([`DEFAULT_BASE_URL`]) exclusively (roadmap, "Current
//! state").
//!
//! Capabilities come from a built-in table because the models endpoint
//! serves ids only. The table carries the rows Loom can verify from
//! Z.ai's published specs; unknown ids return `None` (loud
//! unknown-model failure, not a silently wrong row).
//!
//! GLM reasoning has two controls, and their support is per model:
//!
//! - `thinking.type` — `enabled`/`disabled`. The 5.3 family
//!   (`glm-5.3`, `glm-5.3-flash`, `glm-5.3-flashx`) accepts `enabled`
//!   **only**: sending `disabled` fails the request, so Loom maps its
//!   lowest effort onto `low` instead. `glm-5.2` accepts both, and
//!   disabling is how its cheapest mode is reached.
//! - `reasoning_effort` — `low`/`high`/`max` on the 5.3 family; the
//!   wider OpenAI-style set with server-side remapping on 5.2. Sent
//!   only for models that document it (5.2+).
//!
//! `clear_thinking: false` requests Preserved Thinking: Z.ai keeps
//! prior-turn `reasoning_content` in context (better cache hits in
//! agent loops) on the condition the client echoes it back verbatim —
//! which the shared wire already does for DeepSeek-compatible
//! endpoints. Pruning that drops old reasoning merely forfeits the
//! benefit for the affected turns; it does not fail the request.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};
use serde_json::Value;

use crate::deepseek::ReasoningEffort;
use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{WireOptions, build_request_body};
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

/// Default Z.ai metered API base URL.
///
/// The coding endpoint (`/api/coding/paas/v4`) is deliberately not the
/// default and must not be substituted: it belongs to the GLM Coding
/// Plan subscription, not the metered API.
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "glm";

/// Configuration for [`GlmProvider`].
#[derive(Debug, Clone)]
pub struct GlmConfig {
    /// API base URL; override for tests or proxies.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`], ADR-009).
    /// Reading the key from the environment is the caller's (CLI /
    /// account store) job, not the provider's.
    pub api_key: SecretString,
    /// Mapped onto `thinking.type` + `reasoning_effort` per model; see
    /// the module docs for the per-family rules.
    pub reasoning_effort: ReasoningEffort,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl GlmConfig {
    /// Creates a config with the default base URL, default timeouts,
    /// high reasoning effort, and the given key.
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

/// The GLM [`ModelProvider`] adapter.
///
/// `stream` uses `tokio::time::timeout`, so it must be polled inside a
/// tokio runtime with the time driver enabled; polling it outside one
/// panics.
#[derive(Debug, Clone)]
pub struct GlmProvider {
    config: Arc<GlmConfig>,
    client: reqwest::Client,
}

impl GlmProvider {
    /// Creates a provider with a connection-pooled HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized.
    pub fn new(config: GlmConfig) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }

    /// Classifies an HTTP status per the Z.ai error model
    /// (OpenAI-style error objects; HTTP-status-led).
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`].
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

/// Capability table from Z.ai's published model pages (fetched
/// 2026-10-09): the 5.3 family and 5.2 are 1M context / 131,072 max
/// output with tools and thinking. Parallel tool calls are not
/// documented, so calls serialize (the safe default: emitting calls a
/// model rejects would fail the turn; serializing never does). Earlier
/// generations (`glm-5.1` and below) predate the solid 1M window and
/// are left out until their limits are verified — unknown ids resolve
/// to `None`.
pub fn model_capabilities(model: &str) -> Option<ModelCapabilities> {
    let limits = match model {
        "glm-5.3" | "glm-5.3-flash" | "glm-5.3-flashx" | "glm-5.2" => {
            ModelCapabilities::new(1_048_576)
                .with_max_output_tokens(131_072)
                .with_tool_calls(true)
                .with_parallel_tool_calls(false)
                .with_reasoning(true)
        }
        _ => return None,
    };
    Some(limits)
}

/// Whether `model` accepts `thinking.type: "disabled"`.
///
/// The 5.3 family is forced-thinking: only `enabled` is valid, and the
/// migration notice says a `disabled` request fails outright.
fn thinking_is_forced(model: &str) -> bool {
    matches!(model, "glm-5.3" | "glm-5.3-flash" | "glm-5.3-flashx")
}

/// Resolves the `thinking` object and `reasoning_effort` string for
/// `model` at `effort`.
///
/// - Forced-thinking models: `enabled` + `clear_thinking: false`
///   always; effort `low`/`high`/`max`, with Loom's `none` clamped to
///   `low` (the closest available depth — the request would fail
///   otherwise).
/// - `glm-5.2`: `none` disables thinking outright (its cheapest mode);
///   other efforts send `enabled` plus the effort string for 5.2's
///   documented server-side remapping.
fn thinking_fields(model: &str, effort: ReasoningEffort) -> (Value, Option<&'static str>) {
    if thinking_is_forced(model) {
        let effort_str = match effort {
            ReasoningEffort::None | ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        };
        (
            serde_json::json!({"type": "enabled", "clear_thinking": false}),
            Some(effort_str),
        )
    } else {
        match effort {
            ReasoningEffort::None => (serde_json::json!({"type": "disabled"}), None),
            effort => (
                serde_json::json!({"type": "enabled", "clear_thinking": false}),
                Some(effort.as_str()),
            ),
        }
    }
}

impl WireEndpoint for GlmProvider {
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
        let (thinking, effort) = thinking_fields(&request.model, self.config.reasoning_effort);
        extra_body.insert("thinking".to_owned(), thinking);
        if let Some(effort) = effort {
            extra_body.insert("reasoning_effort".to_owned(), serde_json::json!(effort));
        }
        build_request_body(request, &WireOptions { extra_body })
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        Vec::new()
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// Z.ai documents no in-stream failure `finish_reason` values; none
    /// are mapped (an unknown reason is treated as why the model
    /// stopped, which is the safe reading).
    fn in_stream_failure(_reason: &str) -> Option<ModelErrorKind> {
        None
    }

    /// Classifies an HTTP status per the Z.ai error model. The mapping
    /// mirrors the OpenAI-compatible convention Z.ai documents error
    /// bodies for; statuses are wiremock-verified, not live-verified.
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        let kind = match status.as_u16() {
            400 | 404 | 422 => ModelErrorKind::InvalidRequest,
            401 | 403 => ModelErrorKind::Auth,
            402 | 429 => ModelErrorKind::RateLimited,
            408 => ModelErrorKind::Transport,
            503 => ModelErrorKind::Overloaded,
            _ if status.is_client_error() => ModelErrorKind::InvalidRequest,
            _ => ModelErrorKind::Server,
        };
        ModelError::new(kind, format!("HTTP {status}: {}", body_excerpt(body)))
    }
}

impl ModelProvider for GlmProvider {
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
        for model in ["glm-5.3", "glm-5.3-flash", "glm-5.3-flashx", "glm-5.2"] {
            let caps = model_capabilities(model).expect("known model");
            assert_eq!(caps.max_context_tokens, 1_048_576);
            assert_eq!(caps.max_output_tokens, Some(131_072));
            assert!(caps.tool_calls && caps.reasoning);
            // Undocumented: serialize rather than guess parallel support.
            assert!(!caps.parallel_tool_calls);
        }
    }

    #[test]
    fn capability_table_unknown_and_older_models_are_none() {
        assert!(model_capabilities("glm-5.1").is_none());
        assert!(model_capabilities("glm-4.7").is_none());
        assert!(model_capabilities("gpt-5").is_none());
        assert!(model_capabilities("").is_none());
    }

    #[test]
    fn forced_thinking_models_clamp_none_to_low() {
        for model in ["glm-5.3", "glm-5.3-flash", "glm-5.3-flashx"] {
            let (thinking, effort) = thinking_fields(model, ReasoningEffort::None);
            assert_eq!(thinking["type"], "enabled", "{model}");
            assert_eq!(thinking["clear_thinking"], false, "{model}");
            assert_eq!(effort, Some("low"), "{model}: disabled would fail");
        }
    }

    #[test]
    fn forced_thinking_models_pass_through_efforts() {
        let (_, low) = thinking_fields("glm-5.3", ReasoningEffort::Low);
        let (_, high) = thinking_fields("glm-5.3", ReasoningEffort::High);
        let (_, max) = thinking_fields("glm-5.3", ReasoningEffort::Max);
        assert_eq!((low, high, max), (Some("low"), Some("high"), Some("max")));
    }

    #[test]
    fn glm_52_none_disables_thinking() {
        let (thinking, effort) = thinking_fields("glm-5.2", ReasoningEffort::None);
        assert_eq!(thinking["type"], "disabled");
        assert!(thinking.get("clear_thinking").is_none());
        assert_eq!(effort, None);
    }

    #[test]
    fn glm_52_other_efforts_stay_enabled() {
        for (effort, wire) in [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Max, "max"),
        ] {
            let (thinking, sent) = thinking_fields("glm-5.2", effort);
            assert_eq!(thinking["type"], "enabled");
            assert_eq!(thinking["clear_thinking"], false);
            assert_eq!(sent, Some(wire));
        }
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (400, ModelErrorKind::InvalidRequest),
            (404, ModelErrorKind::InvalidRequest),
            (422, ModelErrorKind::InvalidRequest),
            (401, ModelErrorKind::Auth),
            (403, ModelErrorKind::Auth),
            (402, ModelErrorKind::RateLimited),
            (429, ModelErrorKind::RateLimited),
            (408, ModelErrorKind::Transport),
            (500, ModelErrorKind::Server),
            (503, ModelErrorKind::Overloaded),
        ];
        for (code, kind) in cases {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let error = GlmProvider::classify_status(status, "body");
            assert_eq!(error.kind, kind, "HTTP {code}");
            assert_eq!(error.is_retryable(), kind.is_retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn in_stream_failure_maps_nothing() {
        for reason in ["stop", "length", "error", "insufficient_system_resource"] {
            assert_eq!(
                <GlmProvider as WireEndpoint>::in_stream_failure(reason),
                None,
                "{reason}"
            );
        }
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let config = GlmConfig::new("zai-secret-value");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("zai-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
        let provider = GlmProvider::new(config).unwrap();
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("zai-secret-value"), "{rendered}");
    }

    #[test]
    fn default_base_url_is_the_metered_endpoint() {
        assert_eq!(GlmConfig::new("k").base_url, "https://api.z.ai/api/paas/v4");
    }
}
