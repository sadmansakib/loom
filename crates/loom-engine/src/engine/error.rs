//! Error type for the engine surface: why opening, running, or
//! persisting failed.

use std::path::PathBuf;

use loom_kernel::id::SessionId;
use loom_kernel::store::StoreError;

use crate::auth;

/// Why opening an engine, running a task, or persisting a session failed.
///
/// Terminal *task* failures are not here: they travel as
/// [`loom_kernel::agent::TaskError`] through [`crate::engine::Engine::run_task`], because a
/// failed task is a normal outcome a frontend renders, not a setup error.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The requested provider has no adapter in this build.
    #[error(
        "unknown provider `{0}`: no adapter is registered for it (register an OpenAI-compatible gateway with `loom provider add`)"
    )]
    UnknownProvider(String),
    /// A catalog-driven provider was selected without a catalog.
    #[error("provider `{provider}` needs a model catalog, but none was loaded")]
    CatalogMissing {
        /// The provider that requires the catalog.
        provider: String,
    },
    /// A provider described entirely by user-supplied URLs was
    /// selected without its mandatory base URL.
    #[error("provider `{provider}` needs --base-url (the chat API base URL)")]
    MissingBaseUrl {
        /// The provider that requires the base URL.
        provider: String,
    },
    /// The workspace does not exist or cannot be resolved.
    #[error("workspace `{path}` does not exist or is not readable")]
    Workspace {
        /// The workspace path as supplied.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The account store could not be opened.
    #[error("opening the account store: {0}")]
    Store(#[source] auth::AuthError),
    /// The workspace's command policy manifest exists but is invalid
    /// (roadmap: command execution policy). Failing the open keeps a
    /// broken manifest from silently widening execution to default.
    #[error("loading the command policy: {0}")]
    CommandPolicy(#[from] loom_tools::policy::PolicyError),
    /// No credential could be resolved (ADR-009).
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    /// The provider client could not be constructed.
    ///
    /// Boxed rather than naming `reqwest::Error` so the facade does not leak
    /// the HTTP client's type; the cause is still reachable via
    /// [`std::error::Error::source`].
    #[error("building the provider: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The model is not in the provider's capability table.
    #[error("model `{model}` is unknown to provider `{provider}`: check --model")]
    UnknownModel {
        /// Provider that was requested.
        provider: String,
        /// Model that was not found.
        model: String,
    },
    /// The model's context window is below the pruning floor.
    #[error(
        "model `{model}` has a {tokens}-token context window, below the minimum Loom needs to \
         recover context"
    )]
    ModelWindowTooSmall {
        /// The offending model.
        model: String,
        /// Its advertised context window.
        tokens: u32,
    },
    /// The model must support reasoning: an effort is always sent.
    #[error(
        "model `{model}` does not support reasoning, but a reasoning effort is always sent; \
         choose a reasoning model"
    )]
    ModelWithoutReasoning {
        /// The offending model.
        model: String,
    },
    /// A built-in tool could not be registered.
    #[error("registering {tool}: {message}")]
    RegisterTool {
        /// Tool name being registered.
        tool: &'static str,
        /// The registration failure.
        message: String,
    },
    /// The session store could not be opened, read, or written.
    #[error("session store: {0}")]
    Session(#[source] StoreError),
    /// The session to continue does not exist (or persistence is off).
    #[error("session {id} was not found: nothing to continue")]
    SessionNotFound {
        /// The id that did not resolve.
        id: SessionId,
    },
}
