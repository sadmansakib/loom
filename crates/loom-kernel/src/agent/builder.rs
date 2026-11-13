//! Agent construction and configuration: the seams, the builder
//! setters for every documented knob, and the persistence accessors
//! (§§6.1, 6.4, 18; ADR-012).

use super::Agent;
use super::control::{LEASE_TTL_MILLIS, LeaseGuard, lease_holder_for, lease_owner_id};
use super::error::persistence;
use super::{
    DEFAULT_MAX_TOOL_CONCURRENCY, DEFAULT_MAX_TURNS, DEFAULT_TOOL_TIMEOUT, OutputReservation,
    RetryPolicy, TaskBudget, TaskError, ToolSurface, TurnBudget,
};
use crate::context::{AgentConfig, ContextCompiler, FactsSource, NoFacts};
use crate::id::SessionId;
use crate::model::ModelProvider;
use crate::session::Session;
use crate::store::{ArtifactStore, SessionStore, StoreError};
use crate::tool::ToolRegistry;
use crate::tool_cache::ToolCache;
use std::sync::{Arc, Mutex};
use std::time::Duration;

impl Agent {
    /// Assembles an agent from its seams, using documented defaults for the
    /// retry policy, turn ceiling, output reservation, tool timeout, and
    /// tool concurrency.
    #[must_use]
    pub fn new(
        provider: Box<dyn ModelProvider>,
        compiler: Box<dyn ContextCompiler>,
        registry: ToolRegistry,
        config: AgentConfig,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            compiler,
            registry,
            config,
            model: model.into(),
            retry: RetryPolicy::default(),
            max_turns: DEFAULT_MAX_TURNS,
            turn_budget: TurnBudget::default(),
            budget: TaskBudget::default(),
            output_reservation: OutputReservation::Auto,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            max_tool_concurrency: DEFAULT_MAX_TOOL_CONCURRENCY,
            store: None,
            artifacts: None,
            facts: Arc::new(NoFacts),
            cache: Arc::new(Mutex::new(ToolCache::new())),
            loop_shaping: true,
            surface: None,
        }
    }

    /// Replaces the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the maximum number of model turns (minimum 1).
    #[must_use]
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns.max(1);
        self
    }

    /// Sets the staged turn budget (roadmap item 26). The default is
    /// inert — `max_turns` governs alone — so nothing changes until a
    /// frontend opts in.
    #[must_use]
    pub fn with_turn_budget(mut self, turn_budget: TurnBudget) -> Self {
        self.turn_budget = turn_budget;
        self
    }

    /// Sets an explicit output-token reservation (§8), overriding the
    /// automatic [`OutputReservation::Auto`] derivation. The value is
    /// still clamped to half the model's context window.
    #[must_use]
    pub fn with_reserved_output_tokens(mut self, reserved_output_tokens: u32) -> Self {
        self.output_reservation = OutputReservation::Fixed(reserved_output_tokens);
        self
    }

    /// Sets the task's resource envelope (§14). Unset ceilings are unbounded.
    #[must_use]
    pub fn with_budget(mut self, budget: TaskBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the per-tool-call timeout (§6.6).
    #[must_use]
    pub fn with_tool_timeout(mut self, tool_timeout: Duration) -> Self {
        self.tool_timeout = tool_timeout;
        self
    }

    /// Sets the bound on concurrent read-only tool calls (§10).
    #[must_use]
    pub fn with_max_tool_concurrency(mut self, max_tool_concurrency: usize) -> Self {
        self.max_tool_concurrency = max_tool_concurrency.max(1);
        self
    }

    /// Attaches a durable [`SessionStore`] (§§6.1, 18; ADR-012).
    ///
    /// With a store attached the loop is **persist-then-commit**: each item,
    /// task record, and turn is written to the store *before* the in-memory
    /// [`Session`] mirrors it, so a crash can never lose the record of an
    /// effect that already ran. A store failure returns
    /// [`TaskError::Persistence`] and leaves the in-memory session unchanged
    /// for that step. Without a store the loop behaves exactly as before
    /// (in-memory only).
    #[must_use]
    pub fn with_store(mut self, store: Box<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Attaches an artifact store (§§12, ADR-012 decision 3).
    ///
    /// With a store attached, a tool result too large for the inline budget is
    /// spilled to the store (content-addressed) and the transcript keeps a
    /// compact [`ArtifactRef`](crate::artifact::ArtifactRef) the model can
    /// refetch with `artifact_read`. Without one, oversized output is cut as
    /// before.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Attaches a repository-facts source (2E, ADR-015).
    ///
    /// The source is scanned **once per task**, at task start, and the
    /// resulting [`KnownFacts`](crate::context::KnownFacts) is projected as a
    /// stable system message after the system prompt. Because the snapshot is
    /// fixed for the task, the cached prefix is never churned mid-task. With
    /// no source attached the block is empty and the projection is unchanged.
    #[must_use]
    pub fn with_facts_source(mut self, facts: Box<dyn FactsSource>) -> Self {
        self.facts = Arc::from(facts);
        self
    }

    /// Attaches the session's dynamic tool surface (slice D2). The
    /// surface is shared with the meta-tools that queue loads into it;
    /// the loop promotes queues at turn boundaries, appends loaded
    /// tools after the base registry (append-only, so the provider's
    /// cached prompt prefix survives loads), and evicts tools idle for
    /// the surface policy's threshold. Without a surface the loop is
    /// byte-identical to before.
    #[must_use]
    pub fn with_surface(mut self, surface: Arc<ToolSurface>) -> Self {
        self.surface = Some(surface);
        self
    }

    /// The registry view for the current turn: the base registry with
    /// the surface's loaded tools appended in load order. With no
    /// surface attached this is a cheap clone of the base.
    pub(super) fn combined_registry(&self) -> ToolRegistry {
        match &self.surface {
            Some(surface) => surface.extend_registry(&self.registry),
            None => self.registry.clone(),
        }
    }

    /// Promotes queued loads and evicts idle tools at a turn boundary.
    /// Evictions are logged, never emitted as protocol events — the
    /// event schema stays frozen.
    pub(super) fn reconcile_surface(&self) {
        if let Some(surface) = &self.surface {
            let evicted = surface.end_turn();
            if !evicted.is_empty() {
                tracing::info!(tools = ?evicted, "tool surface evicted idle tools");
            }
        }
    }

    /// Disables the loop's evidence-shaped feedback (the bare-loop arm,
    /// roadmap item 1): the completion gate stops refusing `EndTurns` with
    /// unverified mutations, the failure ledger stops refusing identical
    /// repeats of failed calls, and tools stop receiving the
    /// recent-changes list. The default (`true`) is the production loop.
    #[must_use]
    pub fn with_loop_shaping(mut self, loop_shaping: bool) -> Self {
        self.loop_shaping = loop_shaping;
        self
    }

    /// The model identifier this agent targets.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Loads a persisted session from the attached store, if any.
    ///
    /// Returns `Ok(None)` when no store is attached or the session is unknown,
    /// so a frontend can treat "no persistence" and "not found" alike when
    /// deciding whether to start fresh.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the attached store fails to load
    /// (backend failure or a corrupt stored session).
    pub fn load_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        match &self.store {
            Some(store) => store.load(id),
            None => Ok(None),
        }
    }

    /// Lists persisted session ids (creation order), empty without a store.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the attached store cannot enumerate
    /// the sessions.
    pub fn session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        match &self.store {
            Some(store) => store.session_ids(),
            None => Ok(Vec::new()),
        }
    }

    /// Acquires the session's single-writer lease (ADR-008): the
    /// returned guard releases it when dropped. `Ok(None)` when no
    /// store is attached — nothing durable to guard.
    ///
    /// A denied lease is [`TaskError::SessionLeased`]: another process
    /// is driving this session (or a crashed writer's lease has not
    /// expired). Nothing has been persisted at that point.
    pub(super) fn acquire_session_lease(
        &self,
        session: &SessionId,
    ) -> Result<Option<LeaseGuard<'_>>, TaskError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        // The lease lives on the session row, so make sure the row
        // exists before acquiring (idempotent): a brand-new session is
        // leased before its first item is persisted.
        store.ensure_session(session).map_err(persistence)?;
        match store.acquire_lease(session, &lease_owner_id(), LEASE_TTL_MILLIS) {
            Ok(Some(lease)) => Ok(Some(LeaseGuard {
                store: store.as_ref(),
                lease,
            })),
            Ok(None) => Err(lease_holder_for(store.as_ref(), session)),
            Err(error) => Err(persistence(error)),
        }
    }
}
