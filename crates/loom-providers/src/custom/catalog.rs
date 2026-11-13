//! Catalog snapshots and tolerant parsing for custom OpenAI-compatible endpoints.

use super::{DEFAULT_CONTEXT_TOKENS, ModelCapabilities, TimeoutConfig, body_excerpt, build_client};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cap on the catalog response body, shared scale with the `OpenRouter`
/// fetch; a cap keeps a misbehaving endpoint from ballooning memory.
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

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
///
/// # Errors
///
/// Returns [`crate::openrouter::CatalogError`] when the client cannot
/// be built, the request times out or fails, the endpoint answers
/// with a non-success status, or the body exceeds the byte cap, is
/// not valid UTF-8, or does not parse as a catalog.
pub async fn fetch_custom_catalog(
    models_url: &str,
    api_key: &str,
    timeouts: TimeoutConfig,
) -> Result<CustomCatalog, crate::openrouter::CatalogError> {
    let client = build_client(&timeouts)?;
    fetch_custom_catalog_with(&client, models_url, api_key, timeouts).await
}

/// [`fetch_custom_catalog`] for callers that already own a client.
///
/// # Errors
///
/// Returns [`crate::openrouter::CatalogError`] when the request times
/// out or fails, the endpoint answers with a non-success status, or
/// the body exceeds the byte cap, is not valid UTF-8, or does not
/// parse as a catalog.
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
/// Accepts the three envelope shapes seen in the wild — `OpenAI`'s
/// `{data: [...]}` (also `OpenRouter`, most gateways), `{models: [...]}`,
/// and a bare array — and tolerates per-entry field variations; see
/// the field sniffing in [`reduce_entry`].
///
/// # Errors
///
/// Returns a descriptive `String` when the body is not valid JSON, is
/// not one of the three envelope shapes, or carries no usable
/// `data`/`models` array.
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
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            return Err("catalog envelope: expected an object or array".to_owned());
        }
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
        .map_or_default(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
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
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    // Clamped into `[0.0, u32::MAX]` above; `as` implements the
    // documented truncate-toward-zero narrowing.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(number.max(0.0).min(f64::from(u32::MAX)) as u32)
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
