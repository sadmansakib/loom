//! Model provider adapters for Loom.
//!
//! Each adapter implements `loom_kernel::model::ModelProvider` for one wire
//! protocol on top of the shared OpenAI-compatible streaming driver
//! (`openai_wire` for request/accumulation, `wire_stream` for the SSE
//! machine). Adapters own only configuration and error classification:
//! SSE decoding, timeout enforcement, drop-to-cancel, and normalization
//! of wire stop signals into `loom_kernel` semantics are shared, so no
//! provider specifics leak into the core.
//!
//! Shipped adapters: DeepSeek (built-in capability table, since its API
//! serves none), OpenRouter (capabilities and pricing resolved from
//! the provider's model catalog, which the frontend fetches and caches),
//! Anthropic (its own Messages API driver — see [`anthropic`] for
//! why it does not share the OpenAI wire machine), GLM (Z.ai's
//! metered API on the OpenAI-compatible wire — see [`glm`] for why the
//! Coding Plan endpoint is off-limits), NVIDIA (NIM hosted
//! inference — see [`nvidia`] for why the free tier matters to the
//! eval backlog), Groq (hosted inference on the shared wire), and
//! Custom (any user-supplied OpenAI-compatible endpoint — see
//! [`custom`] for how two URLs and a key describe a provider).

mod anthropic;
mod anthropic_wire;
mod custom;
mod deepseek;
mod glm;
mod groq;
mod http;
mod nvidia;
mod openai_wire;
mod openrouter;
mod secret;
mod sse;
mod wire_stream;

pub use anthropic::{
    AnthropicConfig, AnthropicProvider, DEFAULT_BASE_URL as ANTHROPIC_DEFAULT_BASE_URL,
    PROVIDER_NAME as ANTHROPIC_PROVIDER_NAME,
};
pub use anthropic_wire::ANTHROPIC_VERSION as ANTHROPIC_API_VERSION;
pub use custom::PROVIDER_NAME as CUSTOM_PROVIDER_NAME;
pub use custom::{
    CustomCatalog, CustomConfig, CustomModel, CustomProvider, DEFAULT_CONTEXT_TOKENS,
    fetch_custom_catalog, fetch_custom_catalog_with, parse_custom_catalog,
};
pub use deepseek::PROVIDER_NAME as DEEPSEEK_PROVIDER_NAME;
pub use deepseek::{
    DeepSeekConfig, DeepSeekProvider, ParseReasoningEffortError, ReasoningEffort, ThinkingMode,
    model_capabilities,
};
pub use glm::PROVIDER_NAME as GLM_PROVIDER_NAME;
pub use glm::{GlmConfig, GlmProvider, model_capabilities as glm_model_capabilities};
pub use groq::PROVIDER_NAME as GROQ_PROVIDER_NAME;
pub use groq::{GroqConfig, GroqProvider, model_capabilities as groq_model_capabilities};
pub use http::TimeoutConfig;
pub use nvidia::PROVIDER_NAME as NVIDIA_PROVIDER_NAME;
pub use nvidia::{NvidiaConfig, NvidiaProvider, model_capabilities as nvidia_model_capabilities};
pub use openrouter::PROVIDER_NAME as OPENROUTER_PROVIDER_NAME;
pub use openrouter::{
    CatalogError, DEFAULT_BASE_URL as OPENROUTER_DEFAULT_BASE_URL, OpenRouterCatalog,
    OpenRouterConfig, OpenRouterModel, OpenRouterProvider, ProviderCatalog, fetch_catalog,
    fetch_catalog_with, now_epoch_secs, parse_catalog,
};
pub use secret::SecretString;
