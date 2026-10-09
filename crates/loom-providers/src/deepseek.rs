//! The DeepSeek adapter: an OpenAI-compatible wire client plus DeepSeek
//! error classification, in-stream failure mapping, and a capability
//! table.
//!
//! Wire behaviour verified against the DeepSeek API reference
//! (api-docs.deepseek.com, fetched 2026-10-03): `POST /chat/completions`
//! with streamed `reasoning_content` and tool-call fragments, usage on
//! the final content chunk, `data: [DONE]` terminator, `: keep-alive`
//! comments while queued, HTTP-status-only errors.
//!
//! Capabilities come from a built-in table because the API serves none:
//! `GET /models` lists ids and owners only, so the numbers (context and
//! output limits, tool support) live here, mirroring the published
//! "Models & Pricing" page. Unknown ids return `None`; keeping the
//! table correct for new releases is a code change, deliberately —
//! there is no user-facing capability configuration to drift out of
//! sync with the adapter's wire behaviour.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};
use serde_json::Value;

use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{WireOptions, build_request_body};
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

/// Default DeepSeek API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "deepseek";

/// Reasoning effort the adapter sends explicitly on every request.
///
/// Always sent (never left to endpoint defaults) so provider-side
/// default changes cannot silently alter task behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Lowest effort. Does **not** disable reasoning — the model may
    /// still reason on tool-planning turns (verified live); use
    /// [`ThinkingMode::Disabled`] to turn reasoning off.
    None,
    /// Low effort.
    Low,
    /// High effort (the API's documented default).
    #[default]
    High,
    /// Maximum effort.
    Max,
}

impl ReasoningEffort {
    /// The wire string for this effort.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }
}

/// Error returned when a string is not a valid [`ReasoningEffort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseReasoningEffortError;

impl std::fmt::Display for ParseReasoningEffortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expected one of `none`, `low`, `high`, `max`")
    }
}

impl std::error::Error for ParseReasoningEffortError {}

impl std::str::FromStr for ReasoningEffort {
    type Err = ParseReasoningEffortError;

    /// Parses the wire spelling exactly (lowercase, matching
    /// [`as_str`](ReasoningEffort::as_str)) — the CLI passes user input
    /// through this, so invalid values fail loudly instead of mapping
    /// to a default.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err(ParseReasoningEffortError),
        }
    }
}

/// The independent `thinking` request field (live verification).
///
/// `Disabled` selects the separate non-thinking mode — the actual
/// reasoning off switch (8 KiB default `max_tokens`, no
/// `reasoning_content` in responses). This is distinct from
/// [`ReasoningEffort::None`], which the live API accepts without
/// complaint but which does **not** suppress reasoning while thinking
/// stays enabled (verified live).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// `{"type": "enabled"}` — thinking mode, the API default.
    #[default]
    Enabled,
    /// `{"type": "disabled"}` — the non-thinking mode.
    Disabled,
}

impl ThinkingMode {
    /// Wire string for the `thinking.type` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

/// Configuration for [`DeepSeekProvider`].
#[derive(Debug, Clone)]
pub struct DeepSeekConfig {
    /// API base URL; override for tests or proxies.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`], ADR-009).
    /// Reading `DEEPSEEK_API_KEY` from the environment is the caller's
    /// (CLI/account store) job, not the provider's.
    pub api_key: SecretString,
    /// Sent explicitly on every request; see [`ReasoningEffort`].
    pub reasoning_effort: ReasoningEffort,
    /// The independent thinking on/off switch; see [`ThinkingMode`].
    pub thinking: ThinkingMode,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl DeepSeekConfig {
    /// Creates a config with the default base URL, default timeouts,
    /// high reasoning effort, and the given key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: SecretString::new(api_key),
            reasoning_effort: ReasoningEffort::default(),
            thinking: ThinkingMode::default(),
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

    /// Overrides the thinking on/off switch (builder style).
    #[must_use]
    pub fn with_thinking(mut self, mode: ThinkingMode) -> Self {
        self.thinking = mode;
        self
    }

    /// Overrides the timeouts (builder style).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// The DeepSeek [`ModelProvider`] adapter.
///
/// `stream` uses `tokio::time::timeout`, so it must be polled inside a
/// tokio runtime with the time driver enabled (`#[tokio::main]` and
/// `#[tokio::test]` both qualify); polling it outside one panics.
#[derive(Debug, Clone)]
pub struct DeepSeekProvider {
    config: Arc<DeepSeekConfig>,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    /// Creates a provider with a connection-pooled HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized (e.g.
    /// missing system roots); every other aspect of construction is
    /// infallible configuration.
    pub fn new(config: DeepSeekConfig) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }

    /// Classifies DeepSeek's in-stream failure `finish_reason` values —
    /// conditions that are transport/overload failures, not why the
    /// model stopped generating.
    ///
    /// Test-only alias for [`WireEndpoint::in_stream_failure`]; the
    /// adapter trait is the single implementation.
    #[cfg(test)]
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind> {
        <Self as WireEndpoint>::in_stream_failure(reason)
    }

    /// Classifies an HTTP status per the DeepSeek error model
    /// (HTTP-status only; no business codes).
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`].
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

/// Capability table mirroring the published DeepSeek "Models & Pricing"
/// page (fetched 2026-10-03): both current models are 1M context /
/// 384K (393,216) max output with tools and thinking. Verified live
/// (live verification): the API enumerates exactly these two ids — the
/// legacy `deepseek-v4-flash` is rejected with a 400, so it maps to
/// `None` (loud unknown-model failure, not a silently wrong row).
pub fn model_capabilities(model: &str) -> Option<ModelCapabilities> {
    let limits = match model {
        "deepseek-flash" | "deepseek-v4-pro" => ModelCapabilities::new(1_048_576)
            .with_max_output_tokens(393_216)
            .with_tool_calls(true)
            .with_parallel_tool_calls(true)
            .with_reasoning(true),
        _ => return None,
    };
    Some(limits)
}

impl WireEndpoint for DeepSeekProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn client(&self) -> &reqwest::Client {
        &self.client
    }

    fn endpoint_url(&self) -> String {
        // Tolerate a trailing slash: `https://host/` + path would
        // otherwise produce `//chat/completions`, which some proxies 404.
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
            "thinking".to_owned(),
            serde_json::json!({"type": self.config.thinking.as_str()}),
        );
        extra_body.insert(
            "reasoning_effort".to_owned(),
            serde_json::json!(self.config.reasoning_effort.as_str()),
        );
        build_request_body(request, &WireOptions { extra_body })
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        Vec::new()
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// Classifies DeepSeek's in-stream failure `finish_reason` values —
    /// conditions that are transport/overload failures, not why the
    /// model stopped generating.
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind> {
        match reason {
            "insufficient_system_resource" => Some(ModelErrorKind::Overloaded),
            "aborted" => Some(ModelErrorKind::Transport),
            _ => None,
        }
    }

    /// Classifies an HTTP status per the DeepSeek error model
    /// (HTTP-status only; no business codes).
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        let kind = match status.as_u16() {
            400 | 422 => ModelErrorKind::InvalidRequest,
            401 => ModelErrorKind::Auth,
            402 => ModelErrorKind::QuotaExhausted,
            408 => ModelErrorKind::Transport,
            429 => ModelErrorKind::RateLimited,
            503 => ModelErrorKind::Overloaded,
            _ if status.is_client_error() => ModelErrorKind::InvalidRequest,
            _ => ModelErrorKind::Server,
        };
        // Body text is included best-effort; the shape is verified live
        // (live-verified: OpenAI-style error object) and needs no
        // parsing — the excerpt carries error.message for logs/CLI.
        ModelError::new(kind, format!("HTTP {status}: {}", body_excerpt(body)))
    }
}

impl ModelProvider for DeepSeekProvider {
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
        for model in ["deepseek-flash", "deepseek-v4-pro"] {
            let caps = model_capabilities(model).expect("known model");
            assert_eq!(caps.max_context_tokens, 1_048_576);
            assert_eq!(caps.max_output_tokens, Some(393_216));
            assert!(caps.tool_calls && caps.parallel_tool_calls && caps.reasoning);
        }
        // Legacy id rejected by the live API (verified 2026-10-03):
        // unknown models resolve to None, never a silently wrong row.
        assert!(model_capabilities("deepseek-v4-flash").is_none());
    }

    #[test]
    fn capability_table_unknown_model_is_none() {
        assert!(model_capabilities("gpt-5").is_none());
        assert!(model_capabilities("").is_none());
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (400, ModelErrorKind::InvalidRequest),
            (422, ModelErrorKind::InvalidRequest),
            (401, ModelErrorKind::Auth),
            (402, ModelErrorKind::QuotaExhausted),
            (429, ModelErrorKind::RateLimited),
            (500, ModelErrorKind::Server),
            (503, ModelErrorKind::Overloaded),
            (404, ModelErrorKind::InvalidRequest),
            (502, ModelErrorKind::Server),
        ];
        for (code, kind) in cases {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let error = DeepSeekProvider::classify_status(status, "body");
            assert_eq!(error.kind, kind, "HTTP {code}");
            assert_eq!(error.is_retryable(), kind.is_retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn in_stream_failure_mapping() {
        assert_eq!(
            DeepSeekProvider::in_stream_failure("insufficient_system_resource"),
            Some(ModelErrorKind::Overloaded)
        );
        assert_eq!(
            DeepSeekProvider::in_stream_failure("aborted"),
            Some(ModelErrorKind::Transport)
        );
        assert_eq!(DeepSeekProvider::in_stream_failure("stop"), None);
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let config = DeepSeekConfig::new("sk-live-secret-value");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-live-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
        let provider = DeepSeekProvider::new(config).unwrap();
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("sk-live-secret-value"), "{rendered}");
    }

    #[test]
    fn reasoning_effort_from_str_round_trips() {
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let parsed: ReasoningEffort = effort.as_str().parse().unwrap();
            assert_eq!(parsed, effort);
        }
        for bad in ["medium", "HIGH", "", " high"] {
            assert!(bad.parse::<ReasoningEffort>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reasoning_effort_wire_strings() {
        assert_eq!(ReasoningEffort::None.as_str(), "none");
        assert_eq!(ReasoningEffort::Low.as_str(), "low");
        assert_eq!(ReasoningEffort::High.as_str(), "high");
        assert_eq!(ReasoningEffort::Max.as_str(), "max");
    }

    #[test]
    fn thinking_mode_wire_strings() {
        assert_eq!(ThinkingMode::default(), ThinkingMode::Enabled);
        assert_eq!(ThinkingMode::Enabled.as_str(), "enabled");
        assert_eq!(ThinkingMode::Disabled.as_str(), "disabled");
    }
}
