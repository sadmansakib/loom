//! The custom adapter: any OpenAI-compatible endpoint the user points
//! at, described entirely by two URLs and a key.
//!
//! Some teams run their own gateways (`LiteLLM`, vLLM, an internal
//! proxy, a vendor Loom has no adapter for). They all speak the
//! `OpenAI` chat wire, but their model catalogs differ in field names
//! and detail. This adapter is deliberately schema-tolerant: it
//! reads `GET /models` from any of the common shapes ({`data`: [...]},
//! {`models`: [...]}, or a bare array) and sniffs the well-known
//! spellings of the facts Loom needs. Whatever the catalog omits
//! falls back to safe defaults (see [`DEFAULT_CONTEXT_TOKENS`]); only
//! the model id itself is required, so a typo in `--model` still
//! fails loudly as an unknown model instead of guessing.
//!
//! Chat requests ride the plain `OpenAI` wire (no extra fields), so
//! anything that accepts the `OpenAI` chat-completions shape works
//! unchanged. Status classification follows the shared OpenAI-family
//! error model.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};
use serde_json::Value;

use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{WireOptions, build_request_body};
use crate::openrouter::names_context_overflow;
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

mod catalog;
pub use catalog::{CustomCatalog, CustomModel};
pub use catalog::{fetch_custom_catalog, fetch_custom_catalog_with, parse_custom_catalog};

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "custom";

/// Context window assumed when the catalog does not advertise one.
///
/// Most gateways omit context metadata entirely; Loom still needs a
/// window to prune history against. 128K is the conservative common
/// denominator of current frontier models and clears the engine's
/// 64K minimum, so catalogs that stay silent still work while wildly
/// wrong claims (a 2K window) fail loudly at build time.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 128 * 1024;

/// Configuration for [`CustomProvider`].
///
/// Unlike the hosted adapters there is no default base URL: the
/// endpoint address is the whole point of `custom`, so it is a
/// required constructor argument.
#[derive(Debug, Clone)]
pub struct CustomConfig {
    /// Chat API base URL; `chat/completions` is appended.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`]).
    pub api_key: SecretString,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
    /// Extra HTTP headers (attribution, correlation) sent verbatim on
    /// every chat request; empty by default. Names and values are
    /// validated where user input enters (the CLI's `--header`, `loom
    /// provider add`); a hand-edited registry entry that HTTP cannot
    /// carry is dropped with a warning, never a failed request.
    pub headers: Vec<(String, String)>,
}

impl CustomConfig {
    /// Creates a config for `base_url` with default timeouts.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let base_url = base_url.into();
        crate::http::warn_plain_http(&base_url);
        Self {
            base_url,
            api_key: SecretString::new(api_key),
            timeouts: TimeoutConfig::default(),
            headers: Vec::new(),
        }
    }

    /// Overrides the timeouts (builder style).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Sets extra HTTP headers (builder style), replacing any prior
    /// ones.
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }
}

/// The custom [`ModelProvider`] adapter.
///
/// Bound to one catalog snapshot and one base URL: both come from the
/// user, never from a built-in table. `stream` uses
/// `tokio::time::timeout`, so it must be polled inside a tokio
/// runtime with the time driver enabled.
#[derive(Debug, Clone)]
pub struct CustomProvider {
    config: Arc<CustomConfig>,
    client: reqwest::Client,
    catalog: Arc<CustomCatalog>,
}

impl CustomProvider {
    /// Creates a provider bound to `catalog` with a connection-pooled
    /// HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`reqwest::Error`] when the TLS backend cannot be
    /// initialized.
    pub fn new(config: CustomConfig, catalog: CustomCatalog) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
            catalog: Arc::new(catalog),
        })
    }

    /// The catalog snapshot this provider resolves capabilities from.
    #[must_use]
    pub fn catalog(&self) -> &CustomCatalog {
        &self.catalog
    }

    /// Classifies an HTTP status per the shared OpenAI-family error
    /// model.
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`]; the
    /// adapter trait is the single implementation.
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

impl WireEndpoint for CustomProvider {
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
        // Plain OpenAI wire plus one intent field: per-turn reasoning
        // effort (roadmap item 9), forwarded only when the catalog
        // marks the model reasoning-capable — a gateway that accepts
        // any body is not evidence it steers anything — and the
        // kernel asked for a level. It rides extra_body, never the
        // message list, so cache breakpoints are unaffected.
        let mut options = WireOptions::default();
        if let Some(effort) = request.reasoning_effort
            && self
                .catalog
                .capabilities(&request.model)
                .is_some_and(|capabilities| capabilities.reasoning)
        {
            options.extra_body.insert(
                String::from("reasoning_effort"),
                Value::from(effort.as_str()),
            );
        }
        build_request_body(request, &options)
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        // Pairs arrived validated from the CLI or `provider add`, but
        // the registry file is user-editable: drop (with a warning)
        // anything HTTP rejects rather than failing the request over
        // identity metadata.
        self.config
            .headers
            .iter()
            .filter_map(|(name, value)| {
                let Ok(parsed) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
                    tracing::warn!(name, "dropping a header whose name HTTP rejects");
                    return None;
                };
                if reqwest::header::HeaderValue::from_str(value).is_err() {
                    tracing::warn!(name, "dropping a header whose value HTTP rejects");
                    return None;
                }
                Some((parsed, value.clone()))
            })
            .collect()
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// OpenRouter-format gateways surface upstream failures as a
    /// `finish_reason` of `error`; custom endpoints that do not use
    /// that convention never send it, so mapping it costs nothing and
    /// keeps OpenRouter-shaped proxies retryable.
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind> {
        match reason {
            "error" => Some(ModelErrorKind::Server),
            _ => None,
        }
    }

    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        let mut kind = match status.as_u16() {
            400 | 404 | 422 => ModelErrorKind::InvalidRequest,
            401 | 403 => ModelErrorKind::Auth,
            402 => ModelErrorKind::QuotaExhausted,
            408 => ModelErrorKind::Transport,
            429 => ModelErrorKind::RateLimited,
            503 => ModelErrorKind::Overloaded,
            _ if status.is_client_error() => ModelErrorKind::InvalidRequest,
            _ => ModelErrorKind::Server,
        };
        if kind == ModelErrorKind::InvalidRequest && names_context_overflow(body) {
            kind = ModelErrorKind::ContextOverflow;
        }
        ModelError::new(kind, format!("HTTP {status}: {}", body_excerpt(body)))
    }

    /// Many gateways (`LiteLLM` and `OpenRouter`-format proxies alike)
    /// echo the actual USD charge back in `usage.cost`, and upstream
    /// DeepSeek-family models report cache hits as flat
    /// `prompt_cache_hit_tokens` instead of the OpenAI-standard
    /// nested `prompt_tokens_details.cached_tokens`. Read both so
    /// cache-hit visibility and cost reporting survive the hop
    /// through whatever gateway the user pointed at.
    fn dialect_usage(
        extra: &serde_json::Map<String, serde_json::Value>,
    ) -> crate::openai_wire::DialectUsage {
        #[derive(serde::Deserialize, Default)]
        struct ReportedUsage {
            #[serde(default)]
            cost: Option<f64>,
            #[serde(default)]
            prompt_cache_hit_tokens: Option<u64>,
        }
        let parsed =
            serde_json::from_value::<ReportedUsage>(serde_json::Value::Object(extra.clone()))
                .unwrap_or_default();
        crate::openai_wire::DialectUsage {
            cached_input_tokens: parsed.prompt_cache_hit_tokens,
            cost_usd: parsed.cost,
        }
    }
}

impl ModelProvider for CustomProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        self.catalog.capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        stream_endpoint(self, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_SHAPE: &str = r#"{
        "data": [
            {
                "id": "full-model",
                "context_length": 200000,
                "max_completion_tokens": 8192,
                "pricing": { "prompt": "0.000003", "completion": 0.000015 },
                "supported_parameters": ["tools", "parallel_tool_calls", "reasoning"]
            },
            { "id": "bare-model" }
        ]
    }"#;

    fn catalog(body: &str) -> CustomCatalog {
        parse_custom_catalog(body, "http://catalog.test/models", 1_700_000_000)
            .expect("fixture parses")
    }

    #[test]
    fn openai_envelope_parses_with_defaults() {
        let catalog = catalog(OPENAI_SHAPE);
        assert_eq!(catalog.source, "http://catalog.test/models");
        assert_eq!(catalog.fetched_at, 1_700_000_000);
        assert_eq!(catalog.len(), 2);

        let full = catalog.model("full-model").expect("full model");
        assert_eq!(full.context_length, Some(200_000));
        assert_eq!(full.max_completion_tokens, Some(8192));
        assert_eq!(full.prompt_price, Some(0.000_003));
        assert_eq!(full.completion_price, Some(0.000_015));
        assert!(full.tool_capable && full.parallel_tool_calls && full.reasoning_capable);
    }

    #[test]
    fn silent_fields_resolve_to_safe_defaults() {
        let catalog = catalog(OPENAI_SHAPE);
        let bare = catalog.model("bare-model").expect("bare model");
        assert_eq!(bare.context_length, None);
        assert_eq!(bare.max_completion_tokens, None);
        assert_eq!(bare.prompt_price, None);
        assert!(bare.tool_capable);
        assert!(!bare.parallel_tool_calls);
        assert!(!bare.reasoning_capable);

        // And the defaults flow through capability resolution.
        let caps = catalog.capabilities("bare-model").expect("caps");
        assert_eq!(caps.max_context_tokens, DEFAULT_CONTEXT_TOKENS);
        assert_eq!(caps.max_output_tokens, None);
        assert!(caps.tool_calls);
        assert!(!caps.parallel_tool_calls);
        assert!(!caps.reasoning);
    }

    #[test]
    fn unknown_model_is_not_capable() {
        let catalog = catalog(OPENAI_SHAPE);
        assert!(catalog.capabilities("nope").is_none());
        assert!(catalog.model("nope").is_none());
    }

    #[test]
    fn models_key_and_bare_array_envelopes_parse() {
        let models_key = catalog(r#"{"models": [{"id": "a", "context_window": 65536}]}"#);
        let entry = models_key.model("a").expect("entry");
        assert_eq!(entry.context_length, Some(65_536));

        let bare = catalog(r#"[{"id": "b", "max_model_len": "131072"}]"#);
        let entry = bare.model("b").expect("entry");
        assert_eq!(entry.context_length, Some(131_072));
    }

    #[test]
    fn field_spellings_are_sniffed() {
        let body = r#"{"data": [
            { "id": "a", "max_input_tokens": 32768, "max_output_tokens": 4096 },
            { "id": "b", "top_provider": { "max_completion_tokens": 512 },
              "supports_tools": false, "supports_parallel_tool_calls": true,
              "supports_reasoning": true },
            { "id": "c", "tool_support": false },
            { "id": "d", "supported_parameters": ["tool_calls"] },
            { "not": "an entry" },
            { "id": "e", "context_length": 8192, "max_tokens": 2048 }
        ]}"#;
        let catalog = catalog(body);
        assert_eq!(catalog.len(), 5); // the id-less entry is skipped

        let row_a = catalog.model("a").expect("row_a");
        assert_eq!(row_a.context_length, Some(32_768));
        assert_eq!(row_a.max_completion_tokens, Some(4096));

        let row_b = catalog.model("b").expect("row_b");
        assert_eq!(row_b.context_length, None);
        assert_eq!(row_b.max_completion_tokens, Some(512));
        assert!(!row_b.tool_capable && row_b.parallel_tool_calls && row_b.reasoning_capable);

        let row_c = catalog.model("c").expect("row_c");
        assert!(!row_c.tool_capable);

        let row_d = catalog.model("d").expect("row_d");
        assert!(row_d.tool_capable);
        assert!(!row_d.parallel_tool_calls && !row_d.reasoning_capable);

        // max_completion_tokens wins over max_tokens; both present.
        let row_e = catalog.model("e").expect("row_e");
        assert_eq!(row_e.max_completion_tokens, Some(2048));
    }

    #[test]
    fn negative_price_sentinels_are_unknown() {
        let catalog =
            catalog(r#"{"data": [{"id": "a", "pricing": {"prompt": -1, "completion": "-1"}}]}"#);
        let entry = catalog.model("a").expect("entry");
        assert_eq!(entry.prompt_price, None);
        assert_eq!(entry.completion_price, None);
    }

    #[test]
    fn unparseable_bodies_fail_loudly() {
        assert!(parse_custom_catalog("not json", "u", 1).is_err());
        assert!(parse_custom_catalog(r#"{"object": "list"}"#, "u", 1).is_err());
        assert!(parse_custom_catalog("42", "u", 1).is_err());
    }

    #[test]
    fn catalog_round_trips_through_cache_json() {
        let catalog = catalog(OPENAI_SHAPE);
        let json = serde_json::to_string(&catalog).expect("serializes");
        let back: CustomCatalog = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, catalog);
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (400, ModelErrorKind::InvalidRequest),
            (401, ModelErrorKind::Auth),
            (402, ModelErrorKind::QuotaExhausted),
            (403, ModelErrorKind::Auth),
            (404, ModelErrorKind::InvalidRequest),
            (408, ModelErrorKind::Transport),
            (418, ModelErrorKind::InvalidRequest),
            (422, ModelErrorKind::InvalidRequest),
            (429, ModelErrorKind::RateLimited),
            (500, ModelErrorKind::Server),
            (503, ModelErrorKind::Overloaded),
        ];
        for (code, kind) in cases {
            let status = reqwest::StatusCode::from_u16(code).expect("status");
            let error = CustomProvider::classify_status(status, "body");
            assert_eq!(error.kind, kind, "HTTP {code}");
            assert_eq!(error.is_retryable(), kind.is_retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn context_overflow_body_classifies_distinctly() {
        let status = reqwest::StatusCode::from_u16(400).expect("status");
        let error = CustomProvider::classify_status(
            status,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        );
        assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
        assert!(!error.is_retryable());
    }

    #[test]
    fn dialect_reads_reported_cost_and_cache_hits() {
        use crate::openai_wire::DialectUsage;
        let mut extra = serde_json::Map::new();
        extra.insert("cost".to_string(), serde_json::Value::from(0.000_123));
        extra.insert(
            "prompt_cache_hit_tokens".to_string(),
            serde_json::Value::from(4096u64),
        );
        assert_eq!(
            <CustomProvider as WireEndpoint>::dialect_usage(&extra),
            DialectUsage {
                cached_input_tokens: Some(4096),
                cost_usd: Some(0.000_123),
            }
        );
        // Gateways that echo neither key stay at the defaults; the
        // OpenAI-standard nested shape is read by the shared wire
        // fallback in `finalize`, not here.
        assert_eq!(
            <CustomProvider as WireEndpoint>::dialect_usage(&serde_json::Map::new()),
            DialectUsage::default()
        );
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let config = CustomConfig::new("http://gateway.test", "sk-custom-secret");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-custom-secret"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn endpoint_url_appends_chat_completions() {
        let config = CustomConfig::new("http://gateway.test/v1/", "k");
        let provider = CustomProvider::new(config, catalog(OPENAI_SHAPE)).expect("provider");
        assert_eq!(
            provider.endpoint_url(),
            "http://gateway.test/v1/chat/completions"
        );
    }

    #[test]
    fn provider_capabilities_track_the_bound_catalog() {
        let provider = CustomProvider::new(
            CustomConfig::new("http://gateway.test", "k"),
            catalog(OPENAI_SHAPE),
        )
        .expect("provider");
        assert!(provider.capabilities("full-model").is_some());
        assert!(provider.capabilities("bare-model").is_some());
        assert!(provider.capabilities("unknown").is_none());
        assert_eq!(provider.catalog().len(), 2);
    }
}
