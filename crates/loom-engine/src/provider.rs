//! Provider selection: the one place in the engine that knows the
//! concrete adapters.
//!
//! The rest of the engine (and the whole kernel) talks to
//! `dyn ModelProvider`. Adding an adapter is a new match arm here plus a
//! factory function; nothing else changes. Each factory owns its
//! adapter's configuration and its own setup-time constraints, so
//! adapter-specific rules cannot leak into the composition root.

use loom_kernel::model::{ModelCapabilities, ModelProvider};
use loom_providers::{
    ANTHROPIC_PROVIDER_NAME, AnthropicConfig, AnthropicProvider, DEEPSEEK_PROVIDER_NAME,
    DeepSeekConfig, DeepSeekProvider, OPENROUTER_PROVIDER_NAME, OpenRouterConfig,
    OpenRouterProvider, ProviderCatalog,
};

use crate::engine::{EngineConfig, EngineError};

/// Smallest context window Loom can operate in.
///
/// The pruning compiler only reclaims memory below a 64K-token floor, so
/// a model with a smaller window would overflow its input budget with no
/// recovery strategy. Reject such models at setup, loudly, instead of
/// failing them mid-task.
pub(crate) const MIN_CONTEXT_TOKENS: u32 = 64 * 1024;

/// A constructed provider plus the capabilities resolved for the
/// configured model.
pub(crate) struct ProviderHandle {
    /// The adapter, ready to stream.
    pub provider: Box<dyn ModelProvider>,
    /// Capabilities for [`EngineConfig::model`], already validated
    /// against the adapter's setup constraints.
    pub capabilities: ModelCapabilities,
}

impl std::fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderHandle")
            .field("provider", &self.provider.name())
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

/// Builds the adapter named by `config.provider`.
///
/// `api_key` is the resolved credential; the provider wraps it
/// immediately, so it never travels further in the clear.
pub(crate) fn build(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    match config.provider.as_str() {
        DEEPSEEK_PROVIDER_NAME => build_deepseek(config, api_key),
        ANTHROPIC_PROVIDER_NAME => build_anthropic(config, api_key),
        OPENROUTER_PROVIDER_NAME => build_openrouter(config, api_key),
        other => Err(EngineError::UnknownProvider(other.to_owned())),
    }
}

/// Builds the DeepSeek adapter.
///
/// DeepSeek serves no capability document, so the adapter's table is the
/// authority. Reasoning is required: the adapter always sends
/// `reasoning_effort`, and a model without reasoning support would
/// silently ignore it.
fn build_deepseek(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let mut provider_config =
        DeepSeekConfig::new(api_key).with_reasoning_effort(config.reasoning_effort);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = DeepSeekProvider::new(provider_config)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities = provider
        .capabilities(&config.model)
        .ok_or_else(|| unknown_model(config))?;
    if !capabilities.reasoning {
        return Err(EngineError::ModelWithoutReasoning {
            model: config.model.clone(),
        });
    }
    check_window(config, &capabilities)?;
    Ok(ProviderHandle {
        provider: Box::new(provider),
        capabilities,
    })
}

/// Builds the Anthropic Messages API adapter.
///
/// The static table in the adapter is the authority (no capability
/// document exists to reduce). No reasoning requirement: unlike
/// DeepSeek, this adapter never sends a reasoning parameter, so a model
/// that ignores thinking costs nothing. `reasoning_effort` is likewise
/// ignored — Anthropic takes `effort`, which this adapter does not send.
fn build_anthropic(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let mut provider_config = AnthropicConfig::new(api_key);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = AnthropicProvider::new(provider_config)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities = provider
        .capabilities(&config.model)
        .ok_or_else(|| unknown_model(config))?;
    check_window(config, &capabilities)?;
    Ok(ProviderHandle {
        provider: Box::new(provider),
        capabilities,
    })
}

/// Builds the OpenRouter adapter from the frontend-supplied catalog.
///
/// The catalog is the authority for capabilities and pricing; without
/// one the adapter cannot resolve the model, and guessing a window would
/// be worse than failing. No reasoning requirement: the adapter speaks
/// the plain OpenAI wire and does not send provider-specific reasoning
/// fields.
fn build_openrouter(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let catalog = match &config.catalog {
        Some(ProviderCatalog::OpenRouter(catalog)) => catalog.clone(),
        _ => {
            return Err(EngineError::CatalogMissing {
                provider: config.provider.clone(),
            });
        }
    };
    let mut provider_config = OpenRouterConfig::new(api_key);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = OpenRouterProvider::new(provider_config, catalog)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities = provider
        .capabilities(&config.model)
        .ok_or_else(|| unknown_model(config))?;
    check_window(config, &capabilities)?;
    Ok(ProviderHandle {
        provider: Box::new(provider),
        capabilities,
    })
}

/// The model is not resolvable by the selected adapter.
fn unknown_model(config: &EngineConfig) -> EngineError {
    EngineError::UnknownModel {
        provider: config.provider.clone(),
        model: config.model.clone(),
    }
}

/// Rejects models below the pruning floor ([`MIN_CONTEXT_TOKENS`]).
fn check_window(
    config: &EngineConfig,
    capabilities: &ModelCapabilities,
) -> Result<(), EngineError> {
    if capabilities.max_context_tokens < MIN_CONTEXT_TOKENS {
        return Err(EngineError::ModelWindowTooSmall {
            model: config.model.clone(),
            tokens: capabilities.max_context_tokens,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_resolves_a_current_model() {
        let mut config = crate::engine::test_config();
        config.provider = "anthropic".to_owned();
        config.model = "claude-opus-5-5".to_owned();
        let handle = build(&config, "key").expect("builds");
        assert_eq!(handle.capabilities.max_context_tokens, 1_000_000);
        assert_eq!(handle.capabilities.max_output_tokens, Some(128_000));
        assert!(handle.capabilities.reasoning);
        assert!(handle.capabilities.tool_calls);
    }

    #[test]
    fn anthropic_accepts_models_without_reasoning() {
        // Unlike DeepSeek, the adapter sends no reasoning parameter, so a
        // non-thinking entry is a configuration choice, not an error.
        let mut config = crate::engine::test_config();
        config.provider = "anthropic".to_owned();
        config.model = "claude-haiku-4-5".to_owned();
        let handle = build(&config, "key").expect("builds");
        assert!(!handle.capabilities.reasoning);
        assert_eq!(handle.capabilities.max_context_tokens, 200_000);
    }

    #[test]
    fn anthropic_model_outside_the_table_is_unknown() {
        let mut config = crate::engine::test_config();
        config.provider = "anthropic".to_owned();
        config.model = "claude-opus-5".to_owned(); // dateless alias
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::UnknownModel { .. }));
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let mut config = crate::engine::test_config();
        config.provider = "nope".to_owned();
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::UnknownProvider(name) if name == "nope"));
    }

    #[test]
    fn openrouter_without_catalog_fails_loudly() {
        let mut config = crate::engine::test_config();
        config.provider = "openrouter".to_owned();
        config.catalog = None;
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::CatalogMissing { .. }));
    }

    #[test]
    fn openrouter_catalog_resolves_capabilities() {
        let mut config = crate::engine::test_config();
        config.provider = "openrouter".to_owned();
        config.model = "vendor/model:free".to_owned();
        config.catalog = Some(ProviderCatalog::OpenRouter(
            loom_providers::parse_catalog(
                r#"{"data":[{"id":"vendor/model:free","context_length":200000,
                    "pricing":{"prompt":"0","completion":"0"},
                    "supported_parameters":["tools"]}]}"#,
                1,
            )
            .expect("fixture"),
        ));
        let handle = build(&config, "key").expect("builds");
        assert_eq!(handle.capabilities.max_context_tokens, 200_000);
        assert!(handle.capabilities.tool_calls);
    }

    #[test]
    fn openrouter_model_outside_catalog_is_unknown() {
        let mut config = crate::engine::test_config();
        config.provider = "openrouter".to_owned();
        config.model = "vendor/missing".to_owned();
        config.catalog = Some(ProviderCatalog::OpenRouter(
            loom_providers::parse_catalog(
                r#"{"data":[{"id":"other","context_length":100000}]}"#,
                1,
            )
            .expect("fixture"),
        ));
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::UnknownModel { .. }));
    }

    #[test]
    fn models_below_the_pruning_floor_are_rejected() {
        let mut config = crate::engine::test_config();
        config.provider = "openrouter".to_owned();
        config.model = "vendor/tiny".to_owned();
        config.catalog = Some(ProviderCatalog::OpenRouter(
            loom_providers::parse_catalog(
                r#"{"data":[{"id":"vendor/tiny","context_length":8192}]}"#,
                1,
            )
            .expect("fixture"),
        ));
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(
            error,
            EngineError::ModelWindowTooSmall { tokens: 8192, .. }
        ));
    }
}
