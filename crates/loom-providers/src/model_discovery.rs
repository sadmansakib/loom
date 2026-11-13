//! Authenticated model-ID discovery for built-in providers.
//!
//! Provider model endpoints generally advertise IDs, not the capability
//! metadata Loom needs to run a model safely. Keep endpoint/auth details here
//! and only offer IDs supported by the corresponding adapter capability
//! table. Catalog-driven providers (`OpenRouter` and custom gateways) use their
//! richer catalog APIs instead.

use std::time::Duration;

use serde_json::Value;

use crate::openrouter::CatalogError;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Fetches a built-in provider's model IDs using its models endpoint.
///
/// If `base_url` is supplied, discovery uses `{base_url}/models`; this is
/// useful for compatible proxies and local test endpoints. IDs that the
/// selected Loom adapter does not yet recognize are omitted because its
/// capability table is required to validate model execution.
///
/// # Errors
///
/// Returns [`CatalogError`] if the provider is unsupported, the request fails,
/// the server rejects it, or the response is not a recognized models envelope.
pub async fn discover_builtin_models(
    provider: &str,
    base_url: Option<&str>,
    api_key: &str,
) -> Result<Vec<String>, CatalogError> {
    let (url, anthropic_headers) = if let Some(base_url) = base_url {
        (format!("{}/models", base_url.trim_end_matches('/')), false)
    } else {
        match provider {
            "deepseek" => (
                format!("{}/models", super::deepseek::DEFAULT_BASE_URL),
                false,
            ),
            "anthropic" => (
                format!("{}/v1/models", super::anthropic::DEFAULT_BASE_URL),
                true,
            ),
            "glm" => (format!("{}/models", super::glm::DEFAULT_BASE_URL), false),
            "nvidia" => (format!("{}/models", super::nvidia::DEFAULT_BASE_URL), false),
            "groq" => (format!("{}/models", super::groq::DEFAULT_BASE_URL), false),
            _ => {
                return Err(CatalogError::Decode(format!(
                    "provider `{provider}` has no built-in models endpoint"
                )));
            }
        }
    };

    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(CatalogError::Request)?;
    let mut request = client.get(url);
    if anthropic_headers {
        request = request.header("x-api-key", api_key).header(
            "anthropic-version",
            super::anthropic_wire::ANTHROPIC_VERSION,
        );
    } else {
        request = request.bearer_auth(api_key);
    }
    let response = request.send().await.map_err(CatalogError::Request)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(CatalogError::Status {
            status: status.as_u16(),
            body: body.chars().take(512).collect(),
        });
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(CatalogError::Request)?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .ok_or_else(|| CatalogError::Decode("models response has no data/models array".into()))?;

    let mut models = entries
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .filter(|id| supports_model(provider, id))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    Ok(models)
}

fn supports_model(provider: &str, model: &str) -> bool {
    match provider {
        "deepseek" => super::deepseek::model_capabilities(model).is_some(),
        "anthropic" => super::anthropic::model_capabilities(model).is_some(),
        "glm" => super::glm::model_capabilities(model).is_some(),
        "nvidia" => super::nvidia::model_capabilities(model).is_some(),
        "groq" => super::groq::model_capabilities(model).is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::discover_builtin_models;

    #[tokio::test]
    async fn discovers_live_ids_and_filters_models_without_adapter_capabilities() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "deepseek-v4-pro"},
                    {"id": "unknown-future-model"},
                    {"id": "deepseek-flash"},
                    {"id": "deepseek-flash"}
                ]
            })))
            .mount(&server)
            .await;

        let models = discover_builtin_models("deepseek", Some(&server.uri()), "test-key")
            .await
            .expect("model discovery");
        assert_eq!(models, ["deepseek-flash", "deepseek-v4-pro"]);
    }
}
