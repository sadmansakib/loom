//! The NVIDIA adapter: NIM hosted inference on the OpenAI-compatible
//! wire, plus NIM error classification and a capability table.
//!
//! Wire behaviour follows the NIM LLM API reference
//! (docs.api.nvidia.com/nim, fetched 2026-10-09): `POST
//! /v1/chat/completions` at [`DEFAULT_BASE_URL`] with Bearer
//! `NVIDIA_API_KEY`, streamed `reasoning_content` and tool-call
//! fragments, usage on the final chunk, `data: [DONE]` terminator,
//! OpenAI-style error objects. Live-verified 2026-10-09 against
//! `z-ai/glm-5.3` (real streaming session, tool call, usage, cache
//! writes); the rest stays wiremock-pinned because the hosted catalog
//! moves too fast to pin against a live listing.
//!
//! **Why this adapter exists.** The hosted free-trial tier carries the
//! exact open models the eval backlog wants (roadmap, item 1): GLM-5.3
//! at 1M context, no credit card. Free-tier rate limits apply per
//! model, so N ≥ 5 sweeps must pace themselves, but the funding
//! blocker for measured claims is gone. The DeepSeek V4 deployments
//! that shared the catalog have since hit end-of-life (HTTP 410 on
//! live verification, 2026-08/2026-09) and are gone from the table.
//!
//! Model ids follow the catalog's `publisher/model` scheme (`z-ai/…`,
//! `deepseek-ai/…`); the live `/v1/models` listing is authoritative and
//! a mismatch surfaces as a loud unknown-model failure — a one-line
//! table fix, not a silent wrong row.
//!
//! Reasoning effort is a single top-level `reasoning_effort` field
//! (no `thinking` object here — that is Z.ai's first-party dialect):
//!
//! - GLM-5.3 family: thinking is always on; `low`/`high`/`max` with a
//!   `max` default, so Loom's `none` clamps to `low` (the closest
//!   available depth; there is no off switch to map onto).
//! - Any non-forced deployment (the DeepSeek V4 rows, before their
//!   end-of-life, were the case study): `none`/`low`/`high`/`max`,
//!   with `none` disabling thinking — all four of Loom's efforts pass
//!   through unchanged.
//!
//! NIM's chat template defaults `clear_thinking` to `false` (preserved
//! thinking): prior-turn `reasoning_content` stays in context, which
//! suits agent loops — and the shared wire already echoes it back
//! verbatim, so the default is used as-is.
//!
//! Output caps carry Z.ai's documented 131,072. Parallel tool calls
//! are not documented, so calls serialize (the safe default).

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

/// Default NIM hosted-inference base URL.
pub const DEFAULT_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "nvidia";

/// Configuration for [`NvidiaProvider`].
#[derive(Debug, Clone)]
pub struct NvidiaConfig {
    /// API base URL; override for tests or proxies.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`], ADR-009).
    /// Reading the key from the environment is the caller's (CLI /
    /// account store) job, not the provider's.
    pub api_key: SecretString,
    /// Mapped onto the top-level `reasoning_effort` field per model;
    /// see the module docs for the per-family rules.
    pub reasoning_effort: ReasoningEffort,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl NvidiaConfig {
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

/// The NVIDIA NIM [`ModelProvider`] adapter.
///
/// `stream` uses `tokio::time::timeout`, so it must be polled inside a
/// tokio runtime with the time driver enabled; polling it outside one
/// panics.
#[derive(Debug, Clone)]
pub struct NvidiaProvider {
    config: Arc<NvidiaConfig>,
    client: reqwest::Client,
}

impl NvidiaProvider {
    /// Creates a provider with a connection-pooled HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized.
    pub fn new(config: NvidiaConfig) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }

    /// Classifies an HTTP status per the NIM error model.
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`].
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

/// Capability table from the NIM model pages (fetched 2026-10-09,
/// live-checked the same day; the Nemotron row was added 2026-10-16
/// after a live HTTP 200 check): every pinned row is 1M context with
/// tools and reasoning, output capped at Z.ai's documented 131,072
/// (for Nemotron the cap is a conservative ceiling — NIM accepted a
/// 200k `max_tokens` request). Parallel tool calls are not
/// documented, so calls serialize. The DeepSeek V4 deployments that
/// once shared the catalog returned HTTP 410 (end-of-life
/// 2026-08/2026-09) on live verification and are deliberately
/// absent. Unknown ids resolve to `None` (loud unknown-model
/// failure, not a silently wrong row).
pub fn model_capabilities(model: &str) -> Option<ModelCapabilities> {
    let limits = match model {
        "z-ai/glm-5.3" | "z-ai/glm-5.3-flash" => ModelCapabilities::new(1_048_576)
            .with_max_output_tokens(131_072)
            .with_tool_calls(true)
            .with_parallel_tool_calls(false)
            .with_reasoning(true),
        // Nemotron 3 Super (120B total / 12B active): 1M context per
        // the NIM model page, reasoning on by default, tool calling
        // trained and live-verified. Measured ~2.4 s per call on NIM,
        // which is what makes it usable as a bulk eval model.
        "nvidia/nemotron-3-super-120b-a12b" => ModelCapabilities::new(1_048_576)
            .with_max_output_tokens(131_072)
            .with_tool_calls(true)
            .with_parallel_tool_calls(false)
            .with_reasoning(true),
        _ => return None,
    };
    Some(limits)
}

/// Whether `model` is a forced-thinking NIM deployment.
///
/// GLM-5.3 on NIM has no off switch: thinking is always enabled and
/// only the budget (`low`/`high`/`max`) moves.
fn thinking_is_forced(model: &str) -> bool {
    matches!(model, "z-ai/glm-5.3" | "z-ai/glm-5.3-flash")
}

/// Resolves the `reasoning_effort` string for `model` at `effort`.
///
/// Forced-thinking GLM models clamp `none` onto `low` — the request
/// would otherwise ask for a mode the deployment cannot serve. DeepSeek
/// V4 deployments accept all four documented values, including `none`
/// (their non-think mode), so every effort passes through unchanged.
fn effort_for(model: &str, effort: ReasoningEffort) -> &'static str {
    if thinking_is_forced(model) {
        match effort {
            ReasoningEffort::None | ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    } else {
        effort.as_str()
    }
}

impl WireEndpoint for NvidiaProvider {
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
            serde_json::json!(effort_for(&request.model, self.config.reasoning_effort)),
        );
        build_request_body(request, &WireOptions { extra_body })
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        Vec::new()
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// NIM documents no in-stream failure `finish_reason` values; none
    /// are mapped (an unknown reason is treated as why the model
    /// stopped, which is the safe reading).
    fn in_stream_failure(_reason: &str) -> Option<ModelErrorKind> {
        None
    }

    /// Classifies an HTTP status per the NIM error model (OpenAI-style
    /// error objects; HTTP-status-led). Statuses are wiremock-verified,
    /// not live-verified.
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

impl ModelProvider for NvidiaProvider {
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
        for model in [
            "z-ai/glm-5.3",
            "z-ai/glm-5.3-flash",
            "nvidia/nemotron-3-super-120b-a12b",
        ] {
            let caps = model_capabilities(model).expect("known model");
            assert_eq!(caps.max_context_tokens, 1_048_576, "{model}");
            assert!(caps.tool_calls && caps.reasoning, "{model}");
            // Undocumented: serialize rather than guess parallel support.
            assert!(!caps.parallel_tool_calls, "{model}");
            assert_eq!(caps.max_output_tokens, Some(131_072), "{model}");
        }
    }

    #[test]
    fn capability_table_unknown_models_are_none() {
        assert!(model_capabilities("glm-5.3").is_none(), "unprefixed");
        assert!(
            model_capabilities("moonshotai/kimi-k3").is_none(),
            "unverified row"
        );
        assert!(model_capabilities("gpt-5").is_none());
        assert!(model_capabilities("").is_none());
        // Live-verified 2026-10-09: the DeepSeek V4 deployments are
        // end-of-life on NIM (HTTP 410) and must not resolve.
        for model in [
            "deepseek-ai/deepseek-v4-pro",
            "deepseek-ai/deepseek-v4-pro-0813",
            "deepseek-ai/deepseek-v4-flash",
        ] {
            assert!(
                model_capabilities(model).is_none(),
                "end-of-life deployment {model}"
            );
        }
    }

    #[test]
    fn forced_thinking_models_clamp_none_to_low() {
        for model in ["z-ai/glm-5.3", "z-ai/glm-5.3-flash"] {
            assert_eq!(effort_for(model, ReasoningEffort::None), "low", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::Low), "low", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::High), "high", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::Max), "max", "{model}");
        }
    }

    #[test]
    fn non_forced_deployments_pass_every_effort_through() {
        // The DeepSeek V4 ids are end-of-life on NIM but stand in for
        // any future non-forced deployment: `effort_for`'s else branch
        // passes every effort through, `none` included. Nemotron 3
        // Super is a live non-forced row, so it is checked too.
        for model in [
            "deepseek-ai/deepseek-v4-pro",
            "deepseek-ai/deepseek-v4-pro-0813",
            "deepseek-ai/deepseek-v4-flash",
            "nvidia/nemotron-3-super-120b-a12b",
        ] {
            assert_eq!(effort_for(model, ReasoningEffort::None), "none", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::Low), "low", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::High), "high", "{model}");
            assert_eq!(effort_for(model, ReasoningEffort::Max), "max", "{model}");
        }
    }

    #[test]
    fn default_base_url_is_the_hosted_nim_endpoint() {
        assert_eq!(DEFAULT_BASE_URL, "https://integrate.api.nvidia.com/v1");
        let config = NvidiaConfig::new("nim-secret-value");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("nim-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn client_error_statuses_classify_per_the_openai_table() {
        let invalid = NvidiaProvider::classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error": {"message": "boom"}}"#,
        );
        assert_eq!(invalid.kind, ModelErrorKind::InvalidRequest);
        assert!(invalid.message.contains("boom"));

        let auth = NvidiaProvider::classify_status(reqwest::StatusCode::UNAUTHORIZED, "no key");
        assert_eq!(auth.kind, ModelErrorKind::Auth);
        assert!(!auth.is_retryable());

        let rate =
            NvidiaProvider::classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert_eq!(rate.kind, ModelErrorKind::RateLimited);
        assert!(rate.is_retryable());

        let overloaded =
            NvidiaProvider::classify_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, "overloaded");
        assert_eq!(overloaded.kind, ModelErrorKind::Overloaded);
        assert!(overloaded.is_retryable());
    }
}
