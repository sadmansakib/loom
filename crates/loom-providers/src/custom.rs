//! The custom adapter: any OpenAI-compatible endpoint the user points
//! at, described entirely by two URLs and a key.
//!
//! Some teams run their own gateways (LiteLLM, vLLM, an internal
//! proxy, a vendor Loom has no adapter for). They all speak the
//! OpenAI chat wire, but their model catalogs differ in field names
//! and detail. This adapter is deliberately schema-tolerant: it
//! reads `GET /models` from any of the common shapes ({`data`: [...]},
//! {`models`: [...]}, or a bare array) and sniffs the well-known
//! spellings of the facts Loom needs. Whatever the catalog omits
//! falls back to safe defaults (see [`DEFAULT_CONTEXT_TOKENS`]); only
//! the model id itself is required, so a typo in `--model` still
//! fails loudly as an unknown model instead of guessing.
//!
//! Chat requests ride the plain OpenAI wire (no extra fields), so
//! anything that accepts the OpenAI chat-completions shape works
//! unchanged. Status classification follows the shared OpenAI-family
//! error model.

use std::sync::Arc;

use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelProvider, ModelRequest, ModelStream,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{TimeoutConfig, body_excerpt, build_client};
use crate::openai_wire::{WireOptions, build_request_body};
use crate::openrouter::names_context_overflow;
use crate::secret::SecretString;
use crate::wire_stream::{WireEndpoint, stream_endpoint};

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

/// Cap on the catalog response body, shared scale with the OpenRouter
/// fetch; a cap keeps a misbehaving endpoint from ballooning memory.
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

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
}

impl CustomConfig {
    /// Creates a config for `base_url` with default timeouts.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: SecretString::new(api_key),
            timeouts: TimeoutConfig::default(),
        }
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
/// Fields the catalog omitted arrive as their defaults; see
/// [`CustomCatalog::capabilities`] for how each default resolves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomModel {
    /// Model id as sent on the wire.
    pub id: String,
    /// Advertised input token limit; `None` when unlisted.
    pub context_length: Option<u32>,
    /// Advertised output token limit; `None` when unlisted.
    pub max_completion_tokens: Option<u32>,
    /// Whether the model accepts tool calls (default: yes).
    pub tool_capable: bool,
    /// Whether the model accepts parallel tool calls (default: no —
    /// serializing calls is always safe).
    pub parallel_tool_calls: bool,
    /// Whether the model supports explicit reasoning controls
    /// (default: no; the plain wire sends none either way).
    pub reasoning_capable: bool,
    /// USD per input token, when the catalog prices it.
    pub prompt_price: Option<f64>,
    /// USD per output token, when the catalog prices it.
    pub completion_price: Option<f64>,
}

/// A model-catalog snapshot fetched from a user-supplied URL.
///
/// Serializable so the engine can cache it beside the session store;
/// `source` records the URL it came from so a cached file is never
/// reused for a different endpoint, and `fetched_at` drives TTL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomCatalog {
    /// The models URL this snapshot was fetched from.
    pub source: String,
    /// Unix seconds at which the snapshot was fetched.
    pub fetched_at: u64,
    /// Every model the endpoint advertised, in wire order.
    pub models: Vec<CustomModel>,
}

impl CustomCatalog {
    /// The catalog entry for `id`, if present.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&CustomModel> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Capabilities for `id`, or `None` when the model is not in the
    /// catalog. Unknown ids stay `None` (the engine fails loudly with
    /// an unknown-model error) while silent fields resolve to safe
    /// defaults: the [`DEFAULT_CONTEXT_TOKENS`] window, tool calls
    /// on, parallel calls and reasoning off, no output cap.
    #[must_use]
    pub fn capabilities(&self, id: &str) -> Option<ModelCapabilities> {
        let model = self.model(id)?;
        let mut capabilities =
            ModelCapabilities::new(model.context_length.unwrap_or(DEFAULT_CONTEXT_TOKENS))
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

/// Fetches `GET {models_url}` (a full URL) and reduces it to a
/// catalog stamped with `source = models_url` and the current time.
///
/// Builds its own connection-pooled client; the header timeout bounds
/// time-to-headers and the idle timeout bounds the body read.
pub async fn fetch_custom_catalog(
    models_url: &str,
    api_key: &str,
    timeouts: TimeoutConfig,
) -> Result<CustomCatalog, crate::openrouter::CatalogError> {
    let client = build_client(&timeouts)?;
    fetch_custom_catalog_with(&client, models_url, api_key, timeouts).await
}

/// [`fetch_custom_catalog`] for callers that already own a client.
pub async fn fetch_custom_catalog_with(
    client: &reqwest::Client,
    models_url: &str,
    api_key: &str,
    timeouts: TimeoutConfig,
) -> Result<CustomCatalog, crate::openrouter::CatalogError> {
    use futures::StreamExt;

    let send = client.get(models_url).bearer_auth(api_key).send();
    let response = match tokio::time::timeout(timeouts.header, send).await {
        Err(_) => {
            return Err(crate::openrouter::CatalogError::Timeout);
        }
        Ok(Err(error)) => return Err(crate::openrouter::CatalogError::Request(error)),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    if !status.is_success() {
        let body = crate::http::read_error_body(response, timeouts.idle).await;
        return Err(crate::openrouter::CatalogError::Status {
            status: status.as_u16(),
            body: body_excerpt(&body),
        });
    }
    // Bounded, validated read, same discipline as the OpenRouter
    // fetch: over-cap or non-UTF-8 bodies are protocol failures.
    let read = async {
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(crate::openrouter::CatalogError::Request)?;
            if buf.len() + chunk.len() > MAX_CATALOG_BYTES {
                return Err(crate::openrouter::CatalogError::Decode(format!(
                    "catalog body exceeded the {MAX_CATALOG_BYTES}-byte cap"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        String::from_utf8(buf).map_err(|_| {
            crate::openrouter::CatalogError::Decode("catalog body is not valid UTF-8".to_owned())
        })
    };
    let body = match tokio::time::timeout(timeouts.idle, read).await {
        Err(_) => return Err(crate::openrouter::CatalogError::Timeout),
        Ok(result) => result?,
    };
    parse_custom_catalog(&body, models_url, crate::openrouter::now_epoch_secs())
        .map_err(crate::openrouter::CatalogError::Decode)
}

/// Parses a models response body into a catalog.
///
/// Accepts the three envelope shapes seen in the wild — OpenAI's
/// `{data: [...]}` (also OpenRouter, most gateways), `{models: [...]}`,
/// and a bare array — and tolerates per-entry field variations; see
/// the field sniffing in [`reduce_entry`].
pub fn parse_custom_catalog(
    body: &str,
    source: &str,
    fetched_at: u64,
) -> Result<CustomCatalog, String> {
    let raw: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let entries: &[Value] = match &raw {
        Value::Array(entries) => entries,
        Value::Object(map) => match map.get("data").and_then(Value::as_array) {
            Some(entries) => entries,
            None => map
                .get("models")
                .and_then(Value::as_array)
                .ok_or_else(|| "catalog envelope: no data/models array".to_owned())?,
        },
        _ => return Err("catalog envelope: expected an object or array".to_owned()),
    };
    // Entries without an id carry nothing addressable; skip rather
    // than fail the whole catalog (some gateways prepend metadata
    // objects to model lists).
    let models = entries.iter().filter_map(reduce_entry).collect();
    Ok(CustomCatalog {
        source: source.to_owned(),
        fetched_at,
        models,
    })
}

/// Field spellings seen across OpenAI-compatible catalogs for the
/// input window, tried in order of specificity.
const CONTEXT_FIELDS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_tokens",
    "max_context_length",
    "context_len",
    "max_model_len",
    "max_input_tokens",
];

/// Field spellings for the output cap; `max_tokens` is checked last
/// because some vendors use it for the total budget, not the output.
const OUTPUT_FIELDS: &[&str] = &["max_completion_tokens", "max_output_tokens", "max_tokens"];

/// Reduces one raw entry to the facts Loom uses; `None` when the
/// entry has no usable id.
fn reduce_entry(entry: &Value) -> Option<CustomModel> {
    let object = entry.as_object()?;
    let id = object.get("id")?.as_str()?.to_owned();

    let parameters = object
        .get("supported_parameters")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let supports = |name: &str| parameters.iter().any(|entry| entry == name);

    let context_length = first_u32(object, CONTEXT_FIELDS);
    let max_completion_tokens = first_u32(object, OUTPUT_FIELDS).or_else(|| {
        object
            .get("top_provider")
            .and_then(Value::as_object)
            .and_then(|top| first_u32(top, OUTPUT_FIELDS))
    });

    let tool_capable = supports("tools")
        || supports("tool_calls")
        || first_bool(object, &["supports_tools", "tool_support", "tools"]).unwrap_or(true);
    let parallel_tool_calls = supports("parallel_tool_calls")
        || first_bool(
            object,
            &["supports_parallel_tool_calls", "parallel_tool_calls"],
        )
        .unwrap_or(false);
    let reasoning_capable = supports("reasoning")
        || supports("reasoning_effort")
        || first_bool(object, &["supports_reasoning", "reasoning_capable"]).unwrap_or(false);

    let pricing = object.get("pricing").and_then(Value::as_object);
    let prompt_price = pricing.and_then(|pricing| price(pricing.get("prompt")));
    let completion_price = pricing.and_then(|pricing| price(pricing.get("completion")));

    Some(CustomModel {
        id,
        context_length,
        max_completion_tokens,
        tool_capable,
        parallel_tool_calls,
        reasoning_capable,
        prompt_price,
        completion_price,
    })
}

/// First present field among `fields`, as a number (JSON number or
/// numeric string) narrowed to `u32`.
fn first_u32(object: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<u32> {
    fields
        .iter()
        .find_map(|field| numeric_u32(object.get(*field)))
}

/// First present field among `fields`, as a bool.
fn first_bool(object: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<bool> {
    fields
        .iter()
        .find_map(|field| object.get(*field).and_then(Value::as_bool))
}

/// Number-or-numeric-string to `u32`; fractional values truncate
/// toward zero and everything past `u32::MAX` saturates.
fn numeric_u32(value: Option<&Value>) -> Option<u32> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    Some(number.max(0.0).min(u32::MAX as f64) as u32)
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
    /// Fails only when the TLS backend cannot be initialized.
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
        // Plain OpenAI wire: no dialect fields. A gateway that wants
        // extra body parameters would need them appended here, the
        // same way the hosted dialects do.
        build_request_body(request, &WireOptions::default())
    }

    fn extra_headers(&self) -> Vec<(reqwest::header::HeaderName, String)> {
        Vec::new()
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

    /// Many gateways (LiteLLM and OpenRouter-format proxies alike)
    /// echo the actual USD charge back in `usage.cost`; read it when
    /// present so cost reporting survives the hop through a gateway.
    fn dialect_usage(
        extra: &serde_json::Map<String, serde_json::Value>,
    ) -> crate::openai_wire::DialectUsage {
        #[derive(serde::Deserialize)]
        struct ReportedUsage {
            #[serde(default)]
            cost: Option<f64>,
        }
        let parsed =
            serde_json::from_value::<ReportedUsage>(serde_json::Value::Object(extra.clone())).ok();
        crate::openai_wire::DialectUsage {
            cached_input_tokens: None,
            cost_usd: parsed.and_then(|usage| usage.cost),
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
        assert_eq!(full.prompt_price, Some(0.000003));
        assert_eq!(full.completion_price, Some(0.000015));
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

        let a = catalog.model("a").expect("a");
        assert_eq!(a.context_length, Some(32_768));
        assert_eq!(a.max_completion_tokens, Some(4096));

        let b = catalog.model("b").expect("b");
        assert_eq!(b.context_length, None);
        assert_eq!(b.max_completion_tokens, Some(512));
        assert!(!b.tool_capable && b.parallel_tool_calls && b.reasoning_capable);

        let c = catalog.model("c").expect("c");
        assert!(!c.tool_capable);

        let d = catalog.model("d").expect("d");
        assert!(d.tool_capable);
        assert!(!d.parallel_tool_calls && !d.reasoning_capable);

        // max_completion_tokens wins over max_tokens; both present.
        let e = catalog.model("e").expect("e");
        assert_eq!(e.max_completion_tokens, Some(2048));
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
    fn dialect_reads_reported_cost() {
        use crate::openai_wire::DialectUsage;
        let mut extra = serde_json::Map::new();
        extra.insert("cost".to_string(), serde_json::Value::from(0.000123));
        assert_eq!(
            <CustomProvider as WireEndpoint>::dialect_usage(&extra),
            DialectUsage {
                cached_input_tokens: None,
                cost_usd: Some(0.000123),
            }
        );
        assert_eq!(
            <CustomProvider as WireEndpoint>::dialect_usage(&Default::default()),
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
