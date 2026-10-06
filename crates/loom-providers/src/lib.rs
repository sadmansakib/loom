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
//! serves none) and OpenRouter (capabilities and pricing resolved from
//! the provider's model catalog, which the frontend fetches and caches).

mod deepseek;
mod http;
mod openai_wire;
mod openrouter;
mod secret;
mod sse;
mod wire_stream;

pub use deepseek::PROVIDER_NAME as DEEPSEEK_PROVIDER_NAME;
pub use deepseek::{
    DeepSeekConfig, DeepSeekProvider, ParseReasoningEffortError, ReasoningEffort, ThinkingMode,
    model_capabilities,
};
pub use http::TimeoutConfig;
pub use openrouter::PROVIDER_NAME as OPENROUTER_PROVIDER_NAME;
pub use openrouter::{
    CatalogError, DEFAULT_BASE_URL as OPENROUTER_DEFAULT_BASE_URL, OpenRouterCatalog,
    OpenRouterConfig, OpenRouterModel, OpenRouterProvider, fetch_catalog, fetch_catalog_with,
    now_epoch_secs, parse_catalog,
};
pub use secret::SecretString;
