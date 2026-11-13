//! Catalog snapshots, fetching, and parsing for OpenRouter.

use loom_kernel::model::ModelCapabilities;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{TimeoutConfig, body_excerpt, build_client};

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
    /// A snapshot from a user-supplied models URL (see
    /// [`crate::custom::fetch_custom_catalog`]); `source` keys its
    /// cache to the endpoint it came from.
    Custom(crate::custom::CustomCatalog),
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
///
/// # Errors
///
/// Returns [`CatalogError`] when the client cannot be built, the
/// request times out or fails, the endpoint answers with a
/// non-success status, or the body exceeds the byte cap, is not valid
/// UTF-8, or does not parse as a catalog.
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
///
/// # Errors
///
/// Returns [`CatalogError`] when the request times out or fails, the
/// endpoint answers with a non-success status, or the body exceeds
/// the byte cap, is not valid UTF-8, or does not parse as a catalog.
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
///
/// # Errors
///
/// Returns [`serde_json::Error`] when `body` does not match the
/// `/models` envelope.
pub fn parse_catalog(body: &str, fetched_at: u64) -> Result<OpenRouterCatalog, serde_json::Error> {
    let raw: ModelsResponse = serde_json::from_str(body)?;
    Ok(OpenRouterCatalog {
        fetched_at,
        models: raw.data.into_iter().map(reduce_model).collect(),
    })
}

/// Current Unix time in seconds; `0` if the clock is before the epoch.
#[must_use]
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
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    (parsed >= 0.0).then_some(parsed)
}
