//! The OpenRouter adapter: an OpenAI-compatible client for
//! openrouter.ai, with capabilities resolved from the provider's model
//! catalog.
//!
//! OpenRouter fronts many upstream models — free and paid alike — behind
//! one OpenAI-shaped endpoint. Loom treats those the same way: nothing in
//! this adapter branches on whether a model is free. Pricing arrives as
//! catalog data (zero for free models, non-zero for paid ones) and is
//! reported back through usage cost; capability limits come from the same
//! catalog, so a paid model needs no code change to work.
//!
//! The catalog is fetched by the *frontend* (see [`fetch_catalog`]) and
//! handed to the engine as data, so the engine stays synchronous and
//! offline-capable: it never performs discovery itself. `GET /models` is
//! the source of truth for context limits, output limits, tool support,
//! and prices (verified against the OpenRouter API reference,
//! openrouter.ai/docs, fetched 2026-10-06).
//!
//! Free-tier specifics live outside this adapter: the day/rate quotas are
//! account-wide, not per model, and surface as ordinary 429 responses
//! that the loop already retries.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{WireOptions, build_request_body};
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

/// Default OpenRouter API base URL.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The provider name this adapter answers to in configuration.
pub const PROVIDER_NAME: &str = "openrouter";

/// Configuration for [`OpenRouterProvider`].
#[derive(Debug, Clone)]
pub struct OpenRouterConfig {
    /// API base URL; override for tests, proxies, or self-hosted
    /// gateways.
    pub base_url: String,
    /// Bearer token, redacted in `Debug` ([`SecretString`]). Reading
    /// `OPENROUTER_API_KEY` from the environment is the caller's
    /// (CLI/account store) job, not the provider's.
    pub api_key: SecretString,
    /// Optional `HTTP-Referer` attribution header.
    pub referer: Option<String>,
    /// Optional `X-Title` attribution header.
    pub title: Option<String>,
    /// Liveness timeouts; see [`TimeoutConfig`].
    pub timeouts: TimeoutConfig,
}

impl OpenRouterConfig {
    /// Creates a config with the default base URL, default timeouts, and
    /// no attribution headers.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: SecretString::new(api_key),
            referer: None,
            title: None,
            timeouts: TimeoutConfig::default(),
        }
    }

    /// Overrides the base URL (builder style).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the `HTTP-Referer` attribution header (builder style).
    #[must_use]
    pub fn with_referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    /// Sets the `X-Title` attribution header (builder style).
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Overrides the timeouts (builder style).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// One catalog entry, reduced to the facts Loom uses.
///
/// Prices are USD per token as advertised by the catalog; free models
/// advertise `0` and paid models their actual rate. `None` means the
/// provider omitted the field or advertised the unknown sentinel, never
/// "free".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterModel {
    /// Model id as sent on the wire (e.g. `vendor/model:free`).
    pub id: String,
    /// Input token limit; `None` when the provider omits it.
    pub context_length: Option<u32>,
    /// Output token limit; `None` when the provider omits it.
    pub max_completion_tokens: Option<u32>,
    /// Whether the model accepts tool calls.
    pub tool_capable: bool,
    /// Whether the model accepts parallel tool calls in one turn, from
    /// `supported_parameters`. `false` when unlisted: serializing calls
    /// is always safe, while emitting parallel calls a model rejects
    /// would fail the turn.
    #[serde(default)]
    pub parallel_tool_calls: bool,
    /// Whether the model supports explicit reasoning controls.
    pub reasoning_capable: bool,
    /// USD per input token, when priced.
    pub prompt_price: Option<f64>,
    /// USD per output token, when priced.
    pub completion_price: Option<f64>,
}

impl OpenRouterModel {
    /// `Some(true)` for a free model, `Some(false)` for a priced one,
    /// `None` when the catalog did not price it (unknown, not free).
    #[must_use]
    pub fn is_free(&self) -> Option<bool> {
        match (self.prompt_price, self.completion_price) {
            (Some(prompt), Some(completion)) => Some(prompt == 0.0 && completion == 0.0),
            _ => None,
        }
    }
}

/// A model catalog snapshot from `GET /models`.
///
/// Serializable so a frontend can cache it; `fetched_at` drives cache
/// freshness. The engine receives the parsed snapshot as data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterCatalog {
    /// Unix seconds at which the snapshot was fetched.
    pub fetched_at: u64,
    /// Every model the provider advertised, in wire order.
    pub models: Vec<OpenRouterModel>,
}

impl OpenRouterCatalog {
    /// The catalog entry for `id`, if present.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&OpenRouterModel> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Capabilities for `id`, or `None` when the model is unknown or the
    /// catalog omits its context window (loud unknown-model failure, not
    /// a silently wrong guess).
    #[must_use]
    pub fn capabilities(&self, id: &str) -> Option<ModelCapabilities> {
        let model = self.model(id)?;
        let context = model.context_length?;
        let mut capabilities = ModelCapabilities::new(context)
            .with_tool_calls(model.tool_capable)
            .with_parallel_tool_calls(model.parallel_tool_calls)
            .with_reasoning(model.reasoning_capable);
        if let Some(max_output) = model.max_completion_tokens {
            capabilities = capabilities.with_max_output_tokens(max_output);
        }
        Some(capabilities)
    }

    /// Number of catalog entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Whether the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// A provider-supplied model catalog, handed to the engine as data.
///
/// One variant per catalog-driven adapter: the engine's provider factory
/// matches on the variant and never knows a provider's catalog shape, so
/// a second catalog-driven provider extends this enum instead of growing
/// engine configuration. Adapters that serve capabilities from a built-in
/// table never see it.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderCatalog {
    /// A `GET /models` snapshot (see [`fetch_catalog`]).
    OpenRouter(OpenRouterCatalog),
}

/// Errors from [`fetch_catalog`].
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The request could not be sent or the body could not be read.
    #[error("catalog request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The provider answered with a non-success status.
    #[error("catalog request rejected: HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Bounded body excerpt.
        body: String,
    },
    /// The response did not parse as a catalog.
    #[error("catalog response did not parse: {0}")]
    Decode(String),
    /// The provider did not answer within the configured window.
    #[error("catalog request timed out")]
    Timeout,
}

/// Fetches `GET {base_url}/models` and reduces it to a catalog,
/// timestamped now.
///
/// Builds its own connection-pooled client so a frontend needs no HTTP
/// client of its own; the header timeout bounds time-to-headers and the
/// idle timeout bounds the body read.
pub async fn fetch_catalog(
    base_url: &str,
    api_key: &str,
    timeouts: TimeoutConfig,
) -> Result<OpenRouterCatalog, CatalogError> {
    let client = build_client(&timeouts)?;
    fetch_catalog_with(&client, base_url, api_key, timeouts).await
}

/// Cap on the catalog response body. The live catalog is a few hundred
/// kilobytes; a cap keeps a misbehaving endpoint from ballooning memory —
/// the SSE path caps its reads the same way.
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

/// [`fetch_catalog`] for callers that already own a client.
pub async fn fetch_catalog_with(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    timeouts: TimeoutConfig,
) -> Result<OpenRouterCatalog, CatalogError> {
    use futures::StreamExt;

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let send = client.get(url).bearer_auth(api_key).send();
    let response = match tokio::time::timeout(timeouts.header, send).await {
        Err(_) => return Err(CatalogError::Timeout),
        Ok(Err(error)) => return Err(CatalogError::Request(error)),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    if !status.is_success() {
        let body = crate::http::read_error_body(response, timeouts.idle).await;
        return Err(CatalogError::Status {
            status: status.as_u16(),
            body: body_excerpt(&body),
        });
    }
    // Bounded, validated read: a catalog larger than the cap or not
    // valid UTF-8 is a protocol failure, not a reason to buffer
    // unbounded bytes the way `Response::text` would.
    let read = async {
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(CatalogError::Request)?;
            if buf.len() + chunk.len() > MAX_CATALOG_BYTES {
                return Err(CatalogError::Decode(format!(
                    "catalog body exceeded the {MAX_CATALOG_BYTES}-byte cap"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        String::from_utf8(buf)
            .map_err(|_| CatalogError::Decode("catalog body is not valid UTF-8".to_owned()))
    };
    let body = match tokio::time::timeout(timeouts.idle, read).await {
        Err(_) => return Err(CatalogError::Timeout),
        Ok(result) => result?,
    };
    parse_catalog(&body, now_epoch_secs()).map_err(|error| CatalogError::Decode(error.to_string()))
}

/// Parses a `/models` response body into a catalog stamped with
/// `fetched_at` (exposed for offline re-reads of a cached snapshot).
pub fn parse_catalog(body: &str, fetched_at: u64) -> Result<OpenRouterCatalog, serde_json::Error> {
    let raw: ModelsResponse = serde_json::from_str(body)?;
    Ok(OpenRouterCatalog {
        fetched_at,
        models: raw.data.into_iter().map(reduce_model).collect(),
    })
}

/// Current Unix time in seconds; `0` if the clock is before the epoch.
pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The raw `/models` envelope.
#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<RawModel>,
}

/// One raw catalog entry: only the fields Loom uses are decoded.
#[derive(Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    top_provider: Option<RawTopProvider>,
    #[serde(default)]
    pricing: Option<RawPricing>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct RawPricing {
    #[serde(default)]
    prompt: Option<Value>,
    #[serde(default)]
    completion: Option<Value>,
}

/// Reduces a raw entry to the facts Loom uses.
fn reduce_model(raw: RawModel) -> OpenRouterModel {
    let parameters = raw.supported_parameters.unwrap_or_default();
    let supports = |name: &str| parameters.iter().any(|entry| entry == name);
    OpenRouterModel {
        id: raw.id,
        context_length: raw.context_length,
        max_completion_tokens: raw
            .top_provider
            .and_then(|top_provider| top_provider.max_completion_tokens),
        tool_capable: supports("tools"),
        parallel_tool_calls: supports("parallel_tool_calls"),
        reasoning_capable: supports("reasoning"),
        prompt_price: raw
            .pricing
            .as_ref()
            .and_then(|pricing| price(pricing.prompt.as_ref())),
        completion_price: raw
            .pricing
            .as_ref()
            .and_then(|pricing| price(pricing.completion.as_ref())),
    }
}

/// Parses a price that the catalog may encode as a JSON number or a
/// string; negative advertised sentinels mean unknown.
fn price(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    (parsed >= 0.0).then_some(parsed)
}

/// The OpenRouter [`ModelProvider`] adapter.
///
/// Bound to one catalog snapshot: capabilities resolve from it, so a
/// model change requires a new catalog (the frontend refreshes and the
/// engine opens a new provider). `stream` uses `tokio::time::timeout`,
/// so it must be polled inside a tokio runtime with the time driver
/// enabled; polling it outside one panics.
#[derive(Debug, Clone)]
pub struct OpenRouterProvider {
    config: Arc<OpenRouterConfig>,
    client: reqwest::Client,
    catalog: Arc<OpenRouterCatalog>,
}

impl OpenRouterProvider {
    /// Creates a provider bound to `catalog` with a connection-pooled
    /// HTTP client.
    ///
    /// Fails only when the TLS backend cannot be initialized.
    pub fn new(
        config: OpenRouterConfig,
        catalog: OpenRouterCatalog,
    ) -> Result<Self, reqwest::Error> {
        let client = build_client(&config.timeouts)?;
        Ok(Self {
            config: Arc::new(config),
            client,
            catalog: Arc::new(catalog),
        })
    }

    /// The catalog snapshot this provider resolves capabilities from.
    #[must_use]
    pub fn catalog(&self) -> &OpenRouterCatalog {
        &self.catalog
    }

    /// Classifies an HTTP status per the OpenRouter error model.
    ///
    /// Test-only alias for [`WireEndpoint::classify_status`]; the
    /// adapter trait is the single implementation.
    #[cfg(test)]
    fn classify_status(status: reqwest::StatusCode, body: &str) -> ModelError {
        <Self as WireEndpoint>::classify_status(status, body)
    }
}

/// Whether an error body describes a context-window overflow.
fn names_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("maximum context")
        || lower.contains("context window")
}

impl WireEndpoint for OpenRouterProvider {
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
        // No extra fields: OpenRouter's own `reasoning` object is not
        // sent, so the adapter speaks the plain OpenAI wire and stays
        // correct for every upstream model, reasoning or not.
        build_request_body(request, &WireOptions::default())
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        let mut headers = Vec::new();
        if let Some(referer) = &self.config.referer {
            headers.push((
                reqwest::header::HeaderName::from_static("http-referer"),
                referer.clone(),
            ));
        }
        if let Some(title) = &self.config.title {
            headers.push((
                reqwest::header::HeaderName::from_static("x-title"),
                title.clone(),
            ));
        }
        headers
    }

    fn timeouts(&self) -> TimeoutConfig {
        self.config.timeouts
    }

    /// OpenRouter streams upstream failures as a `finish_reason` of
    /// `error`; retry, since another upstream can serve the request.
    fn in_stream_failure(reason: &str) -> Option<ModelErrorKind> {
        match reason {
            "error" => Some(ModelErrorKind::Server),
            _ => None,
        }
    }

    /// Classifies an HTTP status per the OpenRouter error model.
    ///
    /// 402 signals exhausted credits and blocks even free models (a
    /// permanent condition the loop must not retry); 429 is the shared
    /// rate/`free_model_daily_requests` limit and is retryable. A 4xx
    /// whose body names a context-length overflow is classified as
    /// [`ModelErrorKind::ContextOverflow`] so the loop can distinguish it
    /// from a malformed request.
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
}

impl ModelProvider for OpenRouterProvider {
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

    const CATALOG_JSON: &str = r#"{
        "data": [
            {
                "id": "vendor/free-model:free",
                "context_length": 200000,
                "top_provider": { "max_completion_tokens": 8192 },
                "pricing": { "prompt": "0", "completion": "0" },
                "supported_parameters": ["tools", "parallel_tool_calls", "reasoning", "temperature"]
            },
            {
                "id": "vendor/paid-model",
                "context_length": 1048576,
                "top_provider": { "max_completion_tokens": 65536 },
                "pricing": { "prompt": "0.000003", "completion": "0.000015" },
                "supported_parameters": ["tools"]
            },
            {
                "id": "vendor/unpriced-model",
                "context_length": 32768,
                "pricing": { "prompt": "-1", "completion": "-1" }
            }
        ]
    }"#;

    fn catalog() -> OpenRouterCatalog {
        parse_catalog(CATALOG_JSON, 1_700_000_000).expect("fixture parses")
    }

    #[test]
    fn catalog_parses_free_and_paid_models_alike() {
        let catalog = catalog();
        assert_eq!(catalog.fetched_at, 1_700_000_000);
        assert_eq!(catalog.len(), 3);

        let free = catalog.model("vendor/free-model:free").expect("free model");
        assert_eq!(free.context_length, Some(200_000));
        assert_eq!(free.max_completion_tokens, Some(8192));
        assert_eq!(free.prompt_price, Some(0.0));
        assert_eq!(free.completion_price, Some(0.0));
        assert_eq!(free.is_free(), Some(true));
        assert!(free.tool_capable && free.parallel_tool_calls && free.reasoning_capable);

        let paid = catalog.model("vendor/paid-model").expect("paid model");
        assert_eq!(paid.context_length, Some(1_048_576));
        assert_eq!(paid.max_completion_tokens, Some(65_536));
        assert_eq!(paid.prompt_price, Some(0.000003));
        assert_eq!(paid.completion_price, Some(0.000015));
        assert_eq!(paid.is_free(), Some(false));
        // `tools` listed without `parallel_tool_calls`: calls serialize.
        assert!(paid.tool_capable && !paid.parallel_tool_calls && !paid.reasoning_capable);
    }

    #[test]
    fn unknown_prices_are_none_not_zero() {
        let catalog = catalog();
        let unpriced = catalog.model("vendor/unpriced-model").expect("model");
        assert_eq!(unpriced.prompt_price, None);
        assert_eq!(unpriced.completion_price, None);
        assert_eq!(unpriced.is_free(), None);
    }

    #[test]
    fn capabilities_come_from_the_catalog() {
        let catalog = catalog();
        let free = catalog
            .capabilities("vendor/free-model:free")
            .expect("caps");
        assert_eq!(free.max_context_tokens, 200_000);
        assert_eq!(free.max_output_tokens, Some(8192));
        assert!(free.tool_calls && free.parallel_tool_calls && free.reasoning);

        let paid = catalog.capabilities("vendor/paid-model").expect("caps");
        assert_eq!(paid.max_context_tokens, 1_048_576);
        assert!(paid.tool_calls && !paid.parallel_tool_calls && !paid.reasoning);
    }

    #[test]
    fn capabilities_unknown_or_windowless_model_is_none() {
        let catalog = catalog();
        assert!(catalog.capabilities("nope").is_none());
    }

    #[test]
    fn context_without_window_is_not_capable() {
        let parsed = parse_catalog(
            r#"{"data":[{"id":"a","supported_parameters":["tools"]}]}"#,
            1,
        )
        .expect("parses");
        assert_eq!(parsed.len(), 1);
        assert!(parsed.models[0].tool_capable);
        // No window advertised: resolve to None rather than guessing.
        assert!(parsed.capabilities("a").is_none());
    }

    #[test]
    fn number_and_string_prices_both_parse() {
        let parsed = parse_catalog(
            r#"{"data":[{"id":"a","pricing":{"prompt":0.5,"completion":"0.25"}}]}"#,
            1,
        )
        .expect("parses");
        let model = &parsed.models[0];
        assert_eq!(model.prompt_price, Some(0.5));
        assert_eq!(model.completion_price, Some(0.25));
    }

    #[test]
    fn catalog_round_trips_through_cache_json() {
        let catalog = catalog();
        let json = serde_json::to_string(&catalog).expect("serializes");
        let back: OpenRouterCatalog = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, catalog);
    }

    #[test]
    fn http_status_classification() {
        let cases = [
            (400, ModelErrorKind::InvalidRequest),
            (401, ModelErrorKind::Auth),
            (403, ModelErrorKind::Auth),
            (402, ModelErrorKind::QuotaExhausted),
            (404, ModelErrorKind::InvalidRequest),
            (408, ModelErrorKind::Transport),
            (429, ModelErrorKind::RateLimited),
            (503, ModelErrorKind::Overloaded),
            (500, ModelErrorKind::Server),
        ];
        for (code, kind) in cases {
            let status = reqwest::StatusCode::from_u16(code).expect("status");
            let error = OpenRouterProvider::classify_status(status, "body");
            assert_eq!(error.kind, kind, "HTTP {code}");
            assert_eq!(error.is_retryable(), kind.is_retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn context_overflow_body_classifies_distinctly() {
        let status = reqwest::StatusCode::from_u16(400).expect("status");
        let error = OpenRouterProvider::classify_status(
            status,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        );
        assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
        assert!(!error.is_retryable());
    }

    #[test]
    fn in_stream_error_finish_reason_is_retryable() {
        assert_eq!(
            <OpenRouterProvider as WireEndpoint>::in_stream_failure("error"),
            Some(ModelErrorKind::Server)
        );
        assert_eq!(
            <OpenRouterProvider as WireEndpoint>::in_stream_failure("stop"),
            None
        );
    }

    #[test]
    fn config_debug_redacts_api_key() {
        let config = OpenRouterConfig::new("sk-or-secret-value");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-or-secret-value"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }

    #[test]
    fn default_base_url_is_openrouter() {
        assert_eq!(OpenRouterConfig::new("k").base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn attribution_headers_are_optional_and_ordered() {
        let config = OpenRouterConfig::new("k")
            .with_referer("https://example.test")
            .with_title("Loom");
        let provider = OpenRouterProvider::new(config, catalog()).expect("provider");
        let headers = provider.extra_headers();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0.as_str(), "http-referer");
        assert_eq!(headers[0].1, "https://example.test");
        assert_eq!(headers[1].0.as_str(), "x-title");
        assert_eq!(headers[1].1, "Loom");

        let bare =
            OpenRouterProvider::new(OpenRouterConfig::new("k"), catalog()).expect("provider");
        assert!(bare.extra_headers().is_empty());
    }

    #[test]
    fn provider_capabilities_track_the_bound_catalog() {
        let provider =
            OpenRouterProvider::new(OpenRouterConfig::new("k"), catalog()).expect("provider");
        assert!(provider.capabilities("vendor/free-model:free").is_some());
        assert!(provider.capabilities("vendor/paid-model").is_some());
        assert!(provider.capabilities("unknown").is_none());
        assert_eq!(provider.catalog().len(), 3);
    }
}
