//! Per-connection configuration for the ACP server.

use std::sync::Arc;

use loom_core::EngineOverrides;
use loom_core::kernel::agent::DEFAULT_MAX_TURNS;

/// How the engine is configured for every `session/new`. The
/// production `main` builds this from CLI flags / environment; tests
/// swap in a scripted provider through the overrides factory.
pub struct ServerOptions {
    /// Provider adapter id (`deepseek`, `anthropic`, ...), as in
    /// `loom run --provider`. Loom picks no provider on its own.
    pub provider: String,
    /// Model id, validated against the provider's capability table.
    pub model: String,
    /// Optional base-URL override (proxies, gateways).
    pub base_url: Option<String>,
    /// The loom database: credentials and (with `persist`) durable
    /// events, resolved exactly like the CLI resolves them.
    pub db_path: std::path::PathBuf,
    /// Turn ceiling, mirroring the CLI default.
    pub max_turns: u32,
    /// Fresh engine overrides per session. A factory, not a value:
    /// overrides own state (a scripted provider), so every engine
    /// open must get its own.
    pub overrides_factory: Arc<dyn Fn() -> EngineOverrides + Send + Sync>,
    /// Whether editor-forwarded `mcp_servers` in `session/new` are
    /// attached (roadmap item 37 tail). Off by default: attaching a
    /// server the operator did not configure would grant it process
    /// spawn (and, for the tools it offers, network) inside the
    /// workspace sandbox. With the flag on, stdio servers run through
    /// the same hygiene pipeline as manifest servers, all tools load
    /// eagerly, and nothing is written to the workspace lockfile
    /// (see `docs/acp-tails-design.md`).
    pub allow_client_mcp: bool,
    /// How long a `session/request_permission` round-trip may hang
    /// before the program is denied and the task moves on. An editor
    /// that never answers (dialog closed, extension wedged) must not
    /// stall the prompt forever; the timeout fails closed.
    pub permission_timeout: std::time::Duration,
}

impl ServerOptions {
    /// Server options for a provider/model pair with the standard loom
    /// database location, turn ceiling, and no overrides.
    #[must_use]
    pub fn new(provider: String, model: String, db_path: std::path::PathBuf) -> Self {
        Self {
            provider,
            model,
            base_url: None,
            db_path,
            max_turns: DEFAULT_MAX_TURNS,
            overrides_factory: Arc::new(EngineOverrides::default),
            allow_client_mcp: false,
            permission_timeout: std::time::Duration::from_secs(120),
        }
    }

    /// Sets the base-URL override.
    #[must_use]
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.base_url = base_url;
        self
    }

    /// Sets the per-session overrides factory.
    #[must_use]
    pub fn with_overrides_factory(
        mut self,
        factory: Arc<dyn Fn() -> EngineOverrides + Send + Sync>,
    ) -> Self {
        self.overrides_factory = factory;
        self
    }

    /// Admits editor-forwarded `mcp_servers` in `session/new`
    /// (operator opt-in; see the field docs).
    #[must_use]
    pub fn with_allow_client_mcp(mut self, allow: bool) -> Self {
        self.allow_client_mcp = allow;
        self
    }
}
