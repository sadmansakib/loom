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
    ANTHROPIC_PROVIDER_NAME, AnthropicConfig, AnthropicProvider, CUSTOM_PROVIDER_NAME,
    CustomConfig, CustomProvider, DEEPSEEK_PROVIDER_NAME, DeepSeekConfig, DeepSeekProvider,
    GLM_PROVIDER_NAME, GROQ_PROVIDER_NAME, GlmConfig, GlmProvider, GroqConfig, GroqProvider,
    NVIDIA_PROVIDER_NAME, NvidiaConfig, NvidiaProvider, OPENROUTER_PROVIDER_NAME, OpenRouterConfig,
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

/// Resolves the configured model's capabilities: catalog before table.
///
/// The catalog — supplied by the frontend or fetched by `open_async` —
/// is the dynamic authority: refreshing it adds models with no code
/// change, which is the point of having one. When both sources know the
/// model, the catalog wins; a smaller advertised window than the
/// deployment's real one only makes the compiler prune earlier (safe),
/// while the reverse direction would overflow. The adapter's built-in
/// table is the offline fallback — deterministic, network-free — and
/// when neither knows the model, `None` fails loud at setup rather than
/// guessing a window.
fn resolve_capabilities(
    config: &EngineConfig,
    provider: &dyn ModelProvider,
) -> Option<ModelCapabilities> {
    match config.catalog.as_ref() {
        Some(ProviderCatalog::OpenRouter(catalog)) => catalog
            .capabilities(&config.model)
            .or_else(|| provider.capabilities(&config.model)),
        Some(ProviderCatalog::Custom(catalog)) => catalog
            .capabilities(&config.model)
            .or_else(|| provider.capabilities(&config.model)),
        None => provider.capabilities(&config.model),
    }
}

/// The authority that fixed the configured model's capabilities, and
/// the model it fixed them for — one fact, surfaced once per engine as
/// a `CapabilitiesResolved` event (v8). Mirrors [`resolve_capabilities`]:
/// a catalog row wins, otherwise the provider's own table (under
/// replay, the recording).
pub(crate) fn resolution_source(
    config: &EngineConfig,
    provider: &dyn ModelProvider,
) -> (&'static str, String) {
    let catalog_hit = match config.catalog.as_ref() {
        Some(ProviderCatalog::OpenRouter(catalog)) => catalog.capabilities(&config.model).is_some(),
        Some(ProviderCatalog::Custom(catalog)) => catalog.capabilities(&config.model).is_some(),
        None => false,
    };
    let source = if catalog_hit {
        "catalog"
    } else {
        provider.capability_source()
    };
    (source, config.model.clone())
}

/// A constructed provider plus the capabilities resolved for the
/// configured model.
pub struct ProviderHandle {
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
pub fn build(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    match config.provider.as_str() {
        DEEPSEEK_PROVIDER_NAME => build_deepseek(config, api_key),
        ANTHROPIC_PROVIDER_NAME => build_anthropic(config, api_key),
        OPENROUTER_PROVIDER_NAME => build_openrouter(config, api_key),
        GLM_PROVIDER_NAME => build_glm(config, api_key),
        NVIDIA_PROVIDER_NAME => build_nvidia(config, api_key),
        GROQ_PROVIDER_NAME => build_groq(config, api_key),
        CUSTOM_PROVIDER_NAME => build_custom(config, api_key),
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
    let capabilities =
        resolve_capabilities(config, &provider).ok_or_else(|| unknown_model(config))?;
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
    let capabilities =
        resolve_capabilities(config, &provider).ok_or_else(|| unknown_model(config))?;
    check_window(config, &capabilities)?;
    Ok(ProviderHandle {
        provider: Box::new(provider),
        capabilities,
    })
}

/// Builds the GLM adapter for Z.ai's metered API.
///
/// Like DeepSeek, the adapter always sends a `thinking` parameter, so
/// reasoning-capable models are required (every row in its table is
/// reasoning-capable anyway).
fn build_glm(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let mut provider_config =
        GlmConfig::new(api_key).with_reasoning_effort(config.reasoning_effort);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = GlmProvider::new(provider_config)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities =
        resolve_capabilities(config, &provider).ok_or_else(|| unknown_model(config))?;
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

/// Builds the NVIDIA adapter for NIM hosted inference.
///
/// Sends a top-level `reasoning_effort` on every request, so
/// reasoning-capable models are required (every row in its table is
/// reasoning-capable anyway).
fn build_nvidia(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let mut provider_config =
        NvidiaConfig::new(api_key).with_reasoning_effort(config.reasoning_effort);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = NvidiaProvider::new(provider_config)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities =
        resolve_capabilities(config, &provider).ok_or_else(|| unknown_model(config))?;
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

/// Builds the Groq adapter for GroqCloud inference.
///
/// Sends a top-level `reasoning_effort` on every request, so
/// reasoning-capable models are required (every row in its table is
/// reasoning-capable anyway).
fn build_groq(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let mut provider_config =
        GroqConfig::new(api_key).with_reasoning_effort(config.reasoning_effort);
    if let Some(base_url) = &config.base_url {
        provider_config = provider_config.with_base_url(base_url.clone());
    }
    let provider = GroqProvider::new(provider_config)
        .map_err(|error| EngineError::Provider(Box::new(error)))?;
    let capabilities =
        resolve_capabilities(config, &provider).ok_or_else(|| unknown_model(config))?;
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

/// Builds the custom adapter from the frontend-supplied catalog.
///
/// Everything the adapter needs came from the user: `base_url` names
/// the chat endpoint (mandatory — there is no default to fall back
/// to), the catalog lists the models. Capabilities come from the
/// catalog entry; an id it does not list fails loudly rather than
/// guessing, while fields the entry omits use the adapter's safe
/// defaults. No reasoning requirement: the plain OpenAI wire sends no
/// reasoning fields either way.
fn build_custom(config: &EngineConfig, api_key: &str) -> Result<ProviderHandle, EngineError> {
    let catalog = match &config.catalog {
        Some(ProviderCatalog::Custom(catalog)) => catalog.clone(),
        _ => {
            return Err(EngineError::CatalogMissing {
                provider: config.provider.clone(),
            });
        }
    };
    let Some(base_url) = config.base_url.clone() else {
        return Err(EngineError::MissingBaseUrl {
            provider: config.provider.clone(),
        });
    };
    let provider_config = CustomConfig::new(base_url, api_key);
    let provider = CustomProvider::new(provider_config, catalog)
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

/// Builds a provider for a one-shot model call outside the engine.
///
/// The eval harness's self-optimization loop (roadmap item 20.3,
/// `loom-eval`'s `selfopt` module) needs exactly one thing from this
/// crate: a validated adapter to send a single proposal prompt through.
/// It has no workspace, no session, no tools — so this helper fills a
/// minimal [`EngineConfig`] (no catalog, no persistence, a throwaway
/// database path) and routes through the same [`build`] every frontend
/// uses, inheriting all setup-time validation: unknown provider, unknown
/// model, reasoning requirements, and the context-window floor.
///
/// Catalog-driven adapters (`openrouter`, `custom`) are rejected with
/// [`EngineError::CatalogMissing`] — they cannot resolve capabilities
/// from data this helper does not take. Table-driven adapters
/// (`deepseek`, `anthropic`, `glm`, `nvidia`, `groq`) work as-is.
pub fn one_shot_provider(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key: &str,
) -> Result<ProviderHandle, EngineError> {
    let config = EngineConfig {
        provider: provider.to_owned(),
        model: model.to_owned(),
        base_url: base_url.map(str::to_owned),
        models_url: None,
        reasoning_effort: loom_providers::ReasoningEffort::default(),
        account: None,
        workspace: std::env::temp_dir().join("loom-one-shot"),
        system_prompt: None,
        max_turns: 1,
        db_path: std::env::temp_dir().join("loom-one-shot").join("loom.db"),
        persist: false,
        catalog: None,
        scout: None,
        symbols: false,
        git: false,
        skills: false,
        mcp: false,
        lsp: false,
        budgets: loom_tools::ToolBudgets::default(),
        prune: None,
    };
    build(&config, api_key)
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

    fn custom_catalog_config(model: &str, body: &str) -> crate::engine::EngineConfig {
        let mut config = crate::engine::test_config();
        config.provider = "custom".to_owned();
        config.model = model.to_owned();
        config.base_url = Some("http://gateway.test/v1".to_owned());
        config.catalog = Some(ProviderCatalog::Custom(
            loom_providers::parse_custom_catalog(body, "http://gateway.test/v1/models", 1)
                .expect("fixture"),
        ));
        config
    }

    #[test]
    fn custom_resolves_capabilities_with_defaults_for_silent_fields() {
        let config = custom_catalog_config("bare", r#"[{"id": "bare"}]"#);
        let handle = build(&config, "key").expect("builds");
        assert_eq!(
            handle.capabilities.max_context_tokens,
            loom_providers::DEFAULT_CONTEXT_TOKENS
        );
        assert_eq!(handle.capabilities.max_output_tokens, None);
        assert!(handle.capabilities.tool_calls);
        assert!(!handle.capabilities.parallel_tool_calls);
        assert!(!handle.capabilities.reasoning);
    }

    #[test]
    fn custom_resolves_advertised_fields() {
        let config = custom_catalog_config(
            "full",
            r#"[{"id": "full", "context_window": 200000,
                 "max_output_tokens": 8192,
                 "supported_parameters": ["tools", "parallel_tool_calls", "reasoning"]}]"#,
        );
        let handle = build(&config, "key").expect("builds");
        assert_eq!(handle.capabilities.max_context_tokens, 200_000);
        assert_eq!(handle.capabilities.max_output_tokens, Some(8192));
        assert!(handle.capabilities.tool_calls);
        assert!(handle.capabilities.parallel_tool_calls);
        assert!(handle.capabilities.reasoning);
    }

    #[test]
    fn custom_without_base_url_fails_loudly() {
        let mut config = custom_catalog_config("bare", r#"[{"id": "bare"}]"#);
        config.base_url = None;
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::MissingBaseUrl { .. }));
    }

    #[test]
    fn custom_without_catalog_fails_loudly() {
        let mut config = custom_catalog_config("bare", r#"[{"id": "bare"}]"#);
        config.catalog = None;
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::CatalogMissing { .. }));
    }

    #[test]
    fn custom_model_outside_catalog_is_unknown() {
        let config = custom_catalog_config("typo", r#"[{"id": "real"}]"#);
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(error, EngineError::UnknownModel { .. }));
    }

    #[test]
    fn custom_advertised_window_below_the_floor_is_rejected() {
        let config = custom_catalog_config("tiny", r#"[{"id": "tiny", "context_length": 8192}]"#);
        let error = build(&config, "key").expect_err("must reject");
        assert!(matches!(
            error,
            EngineError::ModelWindowTooSmall { tokens: 8192, .. }
        ));
    }
}
