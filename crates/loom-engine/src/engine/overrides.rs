//! Composition seams for opening an engine: the override points tests
//! and frontends use to inject providers, tools, compilers, and routing
//! stubs (see [`crate::engine::Engine::open`]).

use std::sync::Arc;

use loom_kernel::agent::Agent;
use loom_kernel::context::ContextCompiler;
use loom_kernel::model::ModelProvider;
use loom_kernel::tool::ToolRegistry;
use loom_tools::policy::AskResolver;

/// A function that wraps the provider an open produced — built from
/// credentials or handed in via [`EngineOverrides::provider`] — so an
/// embedder can record or shadow model traffic without duplicating the
/// build path.
pub type ProviderHook = Arc<dyn Fn(Box<dyn ModelProvider>) -> Box<dyn ModelProvider> + Send + Sync>;

/// Composition overrides for embedders that need to swap one production
/// choice without re-wiring the whole agent (loom-eval's harness is the
/// first consumer; a GUI with per-view toolsets will be the second).
///
/// Every field is optional: `None` keeps [`crate::engine::Engine::open`]'s production
/// choice, so an override only ever narrows the gap between an embedder
/// and the real frontend path — the drift risk the overrides exist to
/// remove. Fields that are set replace the production choice entirely;
/// the embedder owns what it replaces (an overridden compiler picks its
/// own estimator, for example).
#[derive(Default)]
pub struct EngineOverrides {
    /// A provider handed in as a boxed trait object, replacing the
    /// credential → adapter build entirely. Offline replay uses this
    /// (no secret is resolved, no network client is built).
    pub provider: Option<Box<dyn ModelProvider>>,
    /// Credential resolved by a frontend while presenting model discovery.
    /// Supplying it avoids a second OS-keychain access when the selected
    /// model's engine is opened.
    pub credential: Option<loom_providers::SecretString>,
    /// Wraps whichever provider results — built from credentials, or
    /// supplied via [`Self::provider`] — so an embedder can record or
    /// shadow the model traffic without duplicating the build path.
    pub provider_hook: Option<ProviderHook>,
    /// A tool registry built by the embedder, replacing the
    /// manifest-driven built-in set.
    pub registry: Option<ToolRegistry>,
    /// A scout child agent handed in as an injected seam, replacing the
    /// [`crate::scout::ScoutConfig`]-driven composition (offline tests use this; the
    /// engine never resolves credentials for it).
    pub scout: Option<Arc<Agent>>,
    /// Tier agents handed in as an injected seam, replacing the
    /// [`crate::routing::RoutingConfig`]-driven composition (offline
    /// tests use this; the engine never resolves credentials for
    /// them). The first agent per tier wins.
    pub routed: Option<Vec<(crate::routing::Tier, Arc<Agent>)>>,
    /// Test seam for item 36: an injected escalation agent (label,
    /// agent) skips the credential build exactly like `routed`.
    pub stall_escalation: Option<(String, Arc<Agent>)>,
    /// A context compiler built by the embedder, replacing the default
    /// pruning chain.
    pub compiler: Option<Box<dyn ContextCompiler>>,
    /// The bare-loop arm (roadmap item 1): strips every harness lever
    /// the full engine adds, leaving read/edit/write/shell/list plus the
    /// plain append-only compiler — the "basic harness" baseline the
    /// G4 claim is measured against. Concretely: no repository facts,
    /// no pruning compiler, no completion gate, no failure-ledger
    /// refusal, and a registry reduced to the five primitive tools.
    /// Composition with the other overrides: an embedder-supplied
    /// registry or compiler still wins (the eval harness pins its own
    /// per-task registry); `bare_loop` only removes what the engine
    /// itself would have added.
    pub bare_loop: bool,
    /// Overrides the fast-path default (roadmap item 3). `None` keeps
    /// the production choice — fast paths on — so a mechanical
    /// instruction costs zero model calls; `Some(false)` disables them
    /// (the bare-loop arm does this implicitly). Fast paths only ever
    /// replace the loop when the workspace's own compiler verifies the
    /// edit green, and revert on failure, so disabling them is a
    /// measurement choice, not a correctness one.
    pub fast_paths: Option<bool>,
    /// An approval callback for `Ask` policy verdicts (roadmap item
    /// 37). `None` keeps today's behavior — an `Ask` verdict fails
    /// closed as a refusal naming the missing frontend. The ACP server
    /// supplies one that bridges the ask to the client's
    /// `request_permission`, so an editor operator becomes the
    /// approver.
    pub ask: Option<AskResolver>,
    /// An approval callback for file writes (`write_file`), asked before
    /// each write runs. `None` keeps writes unattended, as today. The TUI
    /// supplies one so the operator approves each write unless they chose
    /// "never ask again" for the workspace.
    pub write_approval: Option<loom_tools::WriteApproval>,
}

impl EngineOverrides {
    /// Sets a provider that replaces the credential-driven build.
    #[must_use]
    pub fn with_provider(mut self, provider: Box<dyn ModelProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Supplies a credential already resolved by the frontend.
    #[must_use]
    pub fn with_credential(mut self, credential: loom_providers::SecretString) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Sets a hook that wraps the resulting provider.
    #[must_use]
    pub fn with_provider_hook(mut self, hook: ProviderHook) -> Self {
        self.provider_hook = Some(hook);
        self
    }

    /// Sets a registry that replaces the built-in tools.
    #[must_use]
    pub fn with_registry(mut self, registry: ToolRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Sets a compiler that replaces the default pruning chain.
    #[must_use]
    pub fn with_compiler(mut self, compiler: Box<dyn ContextCompiler>) -> Self {
        self.compiler = Some(compiler);
        self
    }

    /// Selects the bare-loop arm ([`EngineOverrides::bare_loop`]).
    #[must_use]
    pub fn bare_loop(mut self) -> Self {
        self.bare_loop = true;
        self
    }

    /// Sets the approval callback for `Ask` verdicts
    /// ([`EngineOverrides::ask`]).
    #[must_use]
    pub fn with_ask_resolver(mut self, ask: AskResolver) -> Self {
        self.ask = Some(ask);
        self
    }

    /// Sets the approval callback for file writes
    /// ([`EngineOverrides::write_approval`]).
    #[must_use]
    pub fn with_write_approval(mut self, approval: loom_tools::WriteApproval) -> Self {
        self.write_approval = Some(approval);
        self
    }
}
