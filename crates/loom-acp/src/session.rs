//! Session state shared by the ACP handlers: the registry of live
//! sessions, prompt exclusivity, and id mapping.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema as acp;
use loom_core::kernel::agent::TaskControl;
use loom_core::kernel::id::SessionId as KernelSessionId;
use loom_core::kernel::session::Session;
use loom_core::{Engine, EngineConfig, ReasoningEffort};
use tokio::sync::Mutex as AsyncMutex;

use crate::options::ServerOptions;

/// The engine side of one ACP session. Guarded by a tokio mutex so a
/// prompt can hold it across the task future while cancellation reaches
/// the separate control slot without waiting for that task.
pub(crate) struct SessionCore {
    pub(crate) engine: Arc<Engine>,
    pub(crate) session: Session,
}

/// What the session map stores: the serialized core plus the slot
/// holding the in-flight prompt's [`TaskControl`]. A cancel needs the
/// slot without contending with the prompt.
#[derive(Clone)]
pub(crate) struct SessionHandle {
    pub(crate) core: Arc<AsyncMutex<SessionCore>>,
    pub(crate) active_prompt: Arc<Mutex<Option<TaskControl>>>,
}

impl SessionHandle {
    pub(crate) fn new(core: Arc<AsyncMutex<SessionCore>>) -> Self {
        Self {
            core,
            active_prompt: Arc::new(Mutex::new(None)),
        }
    }
}

pub(crate) fn begin_prompt(
    slot: &Arc<Mutex<Option<TaskControl>>>,
) -> Option<(TaskControl, ActivePrompt)> {
    let mut active = lock_or_recover(slot);
    if active.is_some() {
        return None;
    }
    let control = TaskControl::new();
    *active = Some(control.clone());
    Some((
        control,
        ActivePrompt {
            slot: Arc::clone(slot),
        },
    ))
}

/// Clears a prompt's cancellation slot on every return path, including
/// cancellation or unwinding of its spawned task.
pub(crate) struct ActivePrompt {
    slot: Arc<Mutex<Option<TaskControl>>>,
}

impl Drop for ActivePrompt {
    fn drop(&mut self) {
        lock_or_recover(&self.slot).take();
    }
}

#[derive(Default)]
pub(crate) struct SessionRegistry {
    pub(crate) sessions: HashMap<acp::SessionId, SessionHandle>,
    loading: HashSet<acp::SessionId>,
}

/// Shared state behind every handler: the options and the live
/// sessions keyed by ACP session id.
#[derive(Clone)]
pub struct AcpState {
    pub(crate) options: Arc<ServerOptions>,
    pub(crate) sessions: Arc<Mutex<SessionRegistry>>,
}

impl AcpState {
    /// Fresh state for a connection serving the given options.
    #[must_use]
    pub fn new(options: ServerOptions) -> Self {
        Self {
            options: Arc::new(options),
            sessions: Arc::new(Mutex::new(SessionRegistry::default())),
        }
    }
}

/// Builds the shared engine configuration for a session rooted at `workspace`.
pub(crate) fn engine_config(
    options: &ServerOptions,
    workspace: std::path::PathBuf,
) -> EngineConfig {
    EngineConfig {
        provider: options.provider.clone(),
        model: options.model.clone(),
        base_url: options.base_url.clone(),
        headers: Vec::new(),
        models_url: None,
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace,
        system_prompt: None,
        max_turns: options.max_turns,
        turn_budget: loom_core::kernel::agent::TurnBudget::default(),
        context_editing: None,
        db_path: options.db_path.clone(),
        persist: true,
        catalog: None,
        scout: None,
        routing: None,
        stall_escalation_model: None,
        background_commands: false,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        mcp: false,
        lsp: false,
        budgets: loom_core::tools::ToolBudgets::default(),
        prune: None,
        autofix: false,
    }
}

/// Locks a std mutex, recovering from poisoning: session bookkeeping
/// holds no invariant a panicked handler could break.
pub(crate) fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Parses an ACP session id back into the kernel id it was minted
/// from: a `UUIDv7` string. Everything else never named a loom session.
pub(crate) fn kernel_session_id(acp_id: &acp::SessionId) -> Result<KernelSessionId, String> {
    let error = "session ids must be canonical UUIDv7 values minted by session/new";
    let uuid = uuid::Uuid::parse_str(&acp_id.0)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
        .filter(|uuid| *uuid.to_string() == *acp_id.0)
        .ok_or_else(|| error.to_owned())?;
    KernelSessionId::try_from(uuid).map_err(|_| error.to_owned())
}

pub(crate) fn reserve_prompt(
    state: &AcpState,
    session_id: &acp::SessionId,
) -> Result<(SessionHandle, TaskControl, ActivePrompt), &'static str> {
    let handle = lock_or_recover(&state.sessions)
        .sessions
        .get(session_id)
        .cloned()
        .ok_or("unknown session")?;
    let (control, active_prompt) = begin_prompt(&handle.active_prompt)
        .ok_or("a prompt is already running for this session")?;
    Ok((handle, control, active_prompt))
}

/// Reserves a session id while its engine is being opened, then atomically
/// replaces the reservation with a live handle. Dropping an uncommitted
/// reservation makes failed or cancelled opens retryable.
pub(crate) struct SessionReservation {
    registry: Arc<Mutex<SessionRegistry>>,
    id: acp::SessionId,
    committed: bool,
}

impl SessionReservation {
    /// Atomically reserves an ID unless it is active or already being opened.
    pub(crate) fn reserve(
        registry: &Arc<Mutex<SessionRegistry>>,
        id: acp::SessionId,
    ) -> Option<Self> {
        let mut registry_guard = lock_or_recover(registry);
        if registry_guard.sessions.contains_key(&id) || !registry_guard.loading.insert(id.clone()) {
            return None;
        }
        Some(Self {
            registry: Arc::clone(registry),
            id,
            committed: false,
        })
    }

    pub(crate) fn install(mut self, handle: SessionHandle) {
        {
            let mut registry = lock_or_recover(&self.registry);
            registry.loading.remove(&self.id);
            registry.sessions.insert(self.id.clone(), handle);
        }
        self.committed = true;
    }
}

impl Drop for SessionReservation {
    fn drop(&mut self) {
        if !self.committed {
            lock_or_recover(&self.registry).loading.remove(&self.id);
        }
    }
}
