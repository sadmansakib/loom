//! `loom-acp`: the loom engine as an [Agent Client Protocol](https://agentclientprotocol.com)
//! (ACP) server, so editors (Zed and every other ACP client) can drive
//! loom as an external agent.
//!
//! Scope (roadmap item 37):
//!
//! * One `loom-acp` process serves one editor connection over the ACP
//!   stdio transport — newline-delimited JSON-RPC 2.0 on stdin/stdout.
//!   The spec forbids anything but valid ACP messages on stdout, so
//!   this crate never prints; `main` initializes `tracing` on stderr
//!   only.
//! * One engine per ACP session: `session/new` opens an
//!   [`Engine`] on the session's `cwd` and keeps one kernel
//!   [`Session`] plus one [`TaskControl`] per ACP session id.
//! * `session/prompt` runs one task to completion, forwarding engine
//!   events as `session/update` notifications in emission order; the
//!   task outcome maps onto the ACP [`StopReason`].
//! * `session/cancel` maps to [`TaskControl::cancel`] — hard stop, the
//!   kernel session survives, and the in-flight `session/prompt`
//!   resolves with `StopReason::Cancelled` as the spec requires.
//! * A policy `Ask` verdict bridges to the client's
//!   `session/request_permission` (one `execute`-kind allow option,
//!   two rejects). Any transport failure, a client
//!   `RequestPermissionOutcome::Cancelled`, or a rejected option fails
//!   closed (deny) — which is exactly the engine's own behavior with
//!   no ask resolver attached.
//!
//! Deliberately absent: `session/load` (not advertised in
//! capabilities, so clients must not send it), `set_model` /
//! `set_session_mode` (the default router answers with
//! method-not-found), and the client's forwarded `mcp_servers` in
//! `session/new` (loom brings its own manifest-driven MCP attachment;
//! attaching editor-forwarded servers is a recorded open tail).
//!
//! The engine is reached through `loom-core` only (ADR-021). The
//! tests drive this whole server in-process over an
//! [`agent_client_protocol::Channel`] pair with a scripted provider
//! (the documented offline seam, `EngineOverrides::provider`), so wire
//! behavior — notification order, stop reasons, the permission bridge
//! — is asserted without a network or an editor.
//!
//! [`Engine`]: loom_core::Engine
//! [`Session`]: loom_core::kernel::Session
//! [`TaskControl`]: loom_core::kernel::agent::TaskControl
//! [`TaskControl::cancel`]: loom_core::kernel::agent::TaskControl::cancel
//! [`StopReason`]: agent_client_protocol::v1::schema::StopReason

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::role::acp::{Agent, Client};
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{ConnectTo, Error};
use loom_core::kernel::agent::{DEFAULT_MAX_TURNS, TaskControl, TaskError};
use loom_core::kernel::event::{EventKind, EventSink};
use loom_core::kernel::id::{SessionId as KernelSessionId, TaskId};
use loom_core::kernel::session::{Session, TodoItem, TodoStatus};
use loom_core::kernel::transcript::ToolStatus;
use loom_core::{Engine, EngineConfig, EngineOverrides, ReasoningEffort};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

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
}

/// The engine side of one ACP session. Guarded by a tokio mutex so a
/// prompt can hold it across the task future while `session/cancel`
/// reaches the (separately stored, cheaply cloned) control handle.
struct SessionCore {
    engine: Arc<Engine>,
    session: Session,
    control: TaskControl,
}

/// What the session map stores: the serialized core plus the control
/// handle clone a cancel needs without contending with the prompt.
#[derive(Clone)]
struct SessionHandle {
    core: Arc<AsyncMutex<SessionCore>>,
    control: TaskControl,
}

/// Shared state behind every handler: the options and the live
/// sessions keyed by ACP session id.
#[derive(Clone)]
pub struct AcpState {
    options: Arc<ServerOptions>,
    sessions: Arc<Mutex<HashMap<acp::SessionId, SessionHandle>>>,
}

impl AcpState {
    /// Fresh state for a connection serving the given options.
    #[must_use]
    pub fn new(options: ServerOptions) -> Self {
        Self {
            options: Arc::new(options),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Locks a std mutex, recovering from poisoning: session bookkeeping
/// holds no invariant a panicked handler could break.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The engine-event → ACP-update bridge. `EventSink::emit` is sync and
/// runs inside the task future; each update lands on an unbounded
/// channel that a per-prompt forwarder drains onto the wire, so
/// emission order survives without blocking the engine.
struct SessionUpdateSink {
    session: acp::SessionId,
    tx: mpsc::UnboundedSender<acp::SessionUpdate>,
}

impl EventSink for SessionUpdateSink {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        for update in updates_for(&self.session, &kind) {
            if self.tx.send(update).is_err() {
                // Forwarder gone (prompt aborted): drop the update;
                // the prompt response carries the outcome.
                break;
            }
        }
    }
}

/// Builds and drives the agent side of an ACP connection over
/// `transport` until the client closes it. `main` passes
/// [`agent_client_protocol::Stdio`]; tests pass one end of an
/// [`agent_client_protocol::Channel::duplex`] pair.
///
/// # Errors
///
/// Propagates connection-level failures (transport error, protocol
/// violation). Individual request failures are answered on the wire,
/// not propagated.
pub async fn serve<Transport>(transport: Transport, state: AcpState) -> Result<(), Error>
where
    Transport: ConnectTo<Agent> + 'static,
{
    let new_state = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state.clone();
    Agent
        .v1()
        .on_receive_request(
            move |request: acp::InitializeRequest,
                  responder: agent_client_protocol::v1::Responder<acp::InitializeResponse>,
                  _cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                let response = acp::InitializeResponse::new(request.protocol_version)
                    .agent_capabilities(acp::AgentCapabilities::new())
                    .agent_info(acp::Implementation::new("loom", env!("CARGO_PKG_VERSION")));
                async move { responder.respond(response) }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: acp::NewSessionRequest,
                  responder: agent_client_protocol::v1::Responder<acp::NewSessionResponse>,
                  cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                // Engine open is synchronous filesystem work; keep it
                // off the dispatch loop.
                let state = new_state.clone();
                let task_cx = cx.clone();
                async move {
                    cx.spawn(
                        async move { handle_new_session(state, request, responder, task_cx).await },
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: acp::PromptRequest,
                  responder: agent_client_protocol::v1::Responder<acp::PromptResponse>,
                  cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                // The dispatch loop is single-task: awaiting the
                // engine run here would starve `session/cancel` (and
                // every other message) for the whole run. The prompt
                // therefore runs as a spawned connection task that
                // responds from inside; this handler returns at once.
                let state = prompt_state.clone();
                let task_cx = cx.clone();
                async move {
                    cx.spawn(async move { handle_prompt(state, request, responder, task_cx).await })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            move |notification: acp::CancelNotification,
                  _cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                if let Some(handle) = lock_or_recover(&cancel_state.sessions)
                    .get(&notification.session_id)
                    .cloned()
                {
                    handle.control.cancel();
                }
                async { Ok(()) }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

/// `session/new`: open one engine on the request's `cwd`, bridge
/// `Ask` verdicts to the client, and register the session.
async fn handle_new_session(
    state: AcpState,
    request: acp::NewSessionRequest,
    responder: agent_client_protocol::v1::Responder<acp::NewSessionResponse>,
    cx: agent_client_protocol::v1::ConnectionTo<Client>,
) -> Result<(), Error> {
    let options = state.options.clone();
    let config = EngineConfig {
        provider: options.provider.clone(),
        model: options.model.clone(),
        base_url: options.base_url.clone(),
        models_url: None,
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace: request.cwd.clone(),
        system_prompt: None,
        max_turns: options.max_turns,
        turn_budget: Default::default(),
        context_editing: None,
        db_path: options.db_path.clone(),
        persist: true,
        catalog: None,
        scout: None,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        mcp: false,
        lsp: false,
        budgets: Default::default(),
        prune: None,
        autofix: false,
    };
    // The ask bridge: a policy `Ask` becomes the client's
    // `session/request_permission`. Fails closed on any transport
    // failure or non-allow outcome, matching the engine's no-resolver
    // default. Built before the engine opens so the resolver rides in
    // the overrides.
    let acp_session_id = acp::SessionId::new(uuid::Uuid::now_v7().to_string());
    let ask_cx = cx.clone();
    let ask_session = acp_session_id.clone();
    let mut overrides = (options.overrides_factory)();
    overrides.ask = Some(Arc::new(move |program| {
        let cx = ask_cx.clone();
        let session = ask_session.clone();
        let permission = ask_permission(cx, session, program.to_string());
        Box::pin(permission)
    }));
    let engine = match Engine::open_with(config, overrides) {
        Ok(engine) => engine,
        Err(error) => {
            return responder.respond_with_internal_error(format!("opening the engine: {error}"));
        }
    };
    let kernel = KernelSessionId::new();
    let control = TaskControl::new();
    let core = Arc::new(AsyncMutex::new(SessionCore {
        engine: Arc::new(engine),
        session: Session::new(kernel),
        control: control.clone(),
    }));
    lock_or_recover(&state.sessions)
        .insert(acp_session_id.clone(), SessionHandle { core, control });
    responder.respond(acp::NewSessionResponse::new(acp_session_id))
}

/// Sends one `session/request_permission` for a blocked program and
/// resolves `true` only for an explicit allow.
async fn ask_permission(
    cx: agent_client_protocol::v1::ConnectionTo<Client>,
    session: acp::SessionId,
    program: String,
) -> bool {
    let title = format!("Run `{program}`");
    let mut fields = acp::ToolCallUpdateFields::new();
    fields.title = Some(title);
    fields.kind = Some(acp::ToolKind::Execute);
    fields.status = Some(acp::ToolCallStatus::InProgress);
    let tool_call = acp::ToolCallUpdate::new(acp::ToolCallId::new(program), fields);
    let request = acp::RequestPermissionRequest::new(
        session,
        tool_call,
        vec![
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("allow"),
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("reject"),
                "Deny",
                acp::PermissionOptionKind::RejectOnce,
            ),
            acp::PermissionOption::new(
                acp::PermissionOptionId::new("reject-always"),
                "Always deny",
                acp::PermissionOptionKind::RejectAlways,
            ),
        ],
    );
    match cx.send_request(request).block_task().await {
        Ok(response) => match response.outcome {
            acp::RequestPermissionOutcome::Selected(selected) => *selected.option_id.0 == *"allow",
            // Cancelled, or a future outcome kind: fail closed.
            _ => false,
        },
        // Transport failure: fail closed.
        Err(_) => false,
    }
}

/// `session/prompt`: run one task to completion, forwarding updates,
/// then answer with the mapped stop reason.
async fn handle_prompt(
    state: AcpState,
    request: acp::PromptRequest,
    responder: agent_client_protocol::v1::Responder<acp::PromptResponse>,
    _cx: agent_client_protocol::v1::ConnectionTo<Client>,
) -> Result<(), Error> {
    let Some(handle) = lock_or_recover(&state.sessions)
        .get(&request.session_id)
        .cloned()
    else {
        return responder.respond_with_error(Error::invalid_request().data("unknown session"));
    };
    let prompt = prompt_text(&request.prompt);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = SessionUpdateSink {
        session: request.session_id.clone(),
        tx,
    };
    let forward_session = request.session_id.clone();
    let forward_cx = _cx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            let notification = acp::SessionNotification::new(forward_session.clone(), update);
            if forward_cx.send_notification(notification).is_err() {
                break;
            }
        }
    });
    let mut core = handle.core.lock().await;
    let engine = core.engine.clone();
    let control = core.control.clone();
    let outcome = engine
        .run_task(&mut core.session, prompt, &sink, &control)
        .await;
    drop(sink);
    drop(core);
    // All updates are on the wire before the response: the forwarder
    // drains the closed channel to exhaustion first.
    let _ = forwarder.await;
    match outcome {
        Ok(_) => responder.respond(acp::PromptResponse::new(acp::StopReason::EndTurn)),
        Err(error) => match stop_reason(&error) {
            Some(reason) => responder.respond(acp::PromptResponse::new(reason)),
            None => responder.respond_with_internal_error(format!("task failed: {error}")),
        },
    }
}

/// Maps a task error onto an ACP stop reason. `None` means the task
/// died of something that is not a client-facing stop condition
/// (model, protocol, or engine-internal failure) and should surface
/// as a JSON-RPC error instead.
fn stop_reason(error: &TaskError) -> Option<acp::StopReason> {
    match error {
        TaskError::Cancelled => Some(acp::StopReason::Cancelled),
        TaskError::MaxTurns { .. } => Some(acp::StopReason::MaxTurnRequests),
        TaskError::BudgetExhausted { .. } => Some(acp::StopReason::MaxTokens),
        // Pause is a loom-internal boundary no ACP client triggers;
        // the committed turns read as a normal end of turn.
        TaskError::Paused => Some(acp::StopReason::EndTurn),
        _ => None,
    }
}

/// Flattens the prompt's content blocks into the plain task input the
/// engine takes. Non-text blocks (images, resources) are skipped:
/// loom's model wire is text today, and dropping is honest — the task
/// proceeds on the text it was given.
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let acp::ContentBlock::Text(text_block) = block {
            text.push_str(&text_block.text);
        }
    }
    text
}

/// Maps one engine event onto zero or more ACP session updates.
///
/// Skipped by design (internal harness telemetry, not conversation
/// content): turn lifecycle, fast-path and capability facts, context
/// pruning/handoff, model request/retry/completion accounting,
/// autofix bookkeeping, steering, budget extensions, loop nudges,
/// terminal task state. An editor sees message chunks, thoughts, tool
/// activity, and the plan — the things a human reads.
fn updates_for(session: &acp::SessionId, kind: &EventKind) -> Vec<acp::SessionUpdate> {
    let _ = session;
    let text_block = |text: &str| acp::ContentBlock::Text(acp::TextContent::new(text.to_owned()));
    match kind {
        EventKind::TextDelta { text } => {
            vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(text_block(text)),
            )]
        }
        EventKind::ReasoningDelta { text } => {
            vec![acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(text_block(text)),
            )]
        }
        EventKind::ToolStarted {
            name,
            call_id,
            arguments,
        } => vec![acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(call_id.to_string()), name.clone())
                .name(name.clone())
                .kind(tool_kind(name))
                .status(acp::ToolCallStatus::InProgress)
                .raw_input(serde_json::Value::String(arguments.clone())),
        )],
        EventKind::ToolCompleted {
            name,
            call_id,
            status,
            content_head,
        } => {
            let mut fields = acp::ToolCallUpdateFields::new();
            fields.title = Some(name.clone());
            fields.status = Some(tool_status(*status));
            fields.content = Some(vec![acp::ToolCallContent::Content(acp::Content::new(
                text_block(content_head),
            ))]);
            vec![acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(acp::ToolCallId::new(call_id.to_string()), fields),
            )]
        }
        EventKind::TodoUpdated { items, .. } => vec![acp::SessionUpdate::Plan(acp::Plan::new(
            items.iter().map(plan_entry).collect(),
        ))],
        _ => Vec::new(),
    }
}

/// Maps a tool name onto the ACP tool kind for editor display.
fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read_file" | "list_dir" | "artifact_read" => acp::ToolKind::Read,
        "search" => acp::ToolKind::Search,
        "write_file" | "edit_file" | "replace_symbol" => acp::ToolKind::Edit,
        "run_command" | "verify" => acp::ToolKind::Execute,
        "todo" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Maps a tool outcome status; a cancelled call reads as failed — ACP
/// has no cancelled tool status, and the cancelled task's stop reason
/// already says it.
fn tool_status(status: ToolStatus) -> acp::ToolCallStatus {
    match status {
        ToolStatus::Success => acp::ToolCallStatus::Completed,
        ToolStatus::Error(_) => acp::ToolCallStatus::Failed,
        ToolStatus::Cancelled { .. } => acp::ToolCallStatus::Failed,
        _ => acp::ToolCallStatus::Failed,
    }
}

/// Maps a kernel todo item onto an ACP plan entry. An abandoned item
/// (`Cancelled`) reads as completed: ACP plans have no abandoned
/// state, and leaving it pending would resurrect work the loop gave
/// up on.
fn plan_entry(item: &TodoItem) -> acp::PlanEntry {
    acp::PlanEntry::new(
        item.text.clone(),
        acp::PlanEntryPriority::Medium,
        match item.status {
            TodoStatus::Pending => acp::PlanEntryStatus::Pending,
            TodoStatus::InProgress => acp::PlanEntryStatus::InProgress,
            // Done, Cancelled, and any future state read as completed:
            // ACP plans have no abandoned state, and leaving work
            // pending would resurrect something the loop gave up on.
            _ => acp::PlanEntryStatus::Completed,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> acp::SessionId {
        acp::SessionId::new("test-session")
    }

    fn kinds(updates: &[acp::SessionUpdate]) -> String {
        updates
            .iter()
            .map(|u| match u {
                acp::SessionUpdate::AgentMessageChunk(_) => "message",
                acp::SessionUpdate::AgentThoughtChunk(_) => "thought",
                acp::SessionUpdate::ToolCall(_) => "tool_call",
                acp::SessionUpdate::ToolCallUpdate(_) => "tool_update",
                acp::SessionUpdate::Plan(_) => "plan",
                _ => "other",
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn text_and_reasoning_deltas_map_to_chunks() {
        let updates = updates_for(
            &session(),
            &EventKind::TextDelta {
                text: "hello".into(),
            },
        );
        assert_eq!(kinds(&updates), "message");
        let acp::SessionUpdate::AgentMessageChunk(chunk) = &updates[0] else {
            panic!("must be a message chunk");
        };
        match &chunk.content {
            acp::ContentBlock::Text(text) => assert_eq!(text.text, "hello"),
            other => panic!("must be text, got {other:?}"),
        }

        let updates = updates_for(
            &session(),
            &EventKind::ReasoningDelta {
                text: "thinking".into(),
            },
        );
        assert_eq!(kinds(&updates), "thought");
    }

    #[test]
    fn tool_events_map_with_kind_status_and_content() {
        let call_id = loom_core::kernel::transcript::ToolCallId::try_from("call-1".to_owned())
            .expect("valid call id");
        let started = updates_for(
            &session(),
            &EventKind::ToolStarted {
                name: "run_command".into(),
                call_id: call_id.clone(),
                arguments: "{\"program\":\"cargo\"}".into(),
            },
        );
        assert_eq!(kinds(&started), "tool_call");
        let acp::SessionUpdate::ToolCall(call) = &started[0] else {
            panic!("must be a tool call");
        };
        assert_eq!(call.title, "run_command");
        assert!(matches!(call.kind, acp::ToolKind::Execute));
        assert!(matches!(call.status, acp::ToolCallStatus::InProgress));

        let completed = updates_for(
            &session(),
            &EventKind::ToolCompleted {
                name: "run_command".into(),
                call_id,
                status: ToolStatus::Error(loom_core::kernel::transcript::ToolFailure::UnknownTool),
                content_head: "boom".into(),
            },
        );
        assert_eq!(kinds(&completed), "tool_update");
        let acp::SessionUpdate::ToolCallUpdate(update) = &completed[0] else {
            panic!("must be a tool update");
        };
        assert_eq!(
            update.fields.status,
            Some(acp::ToolCallStatus::Failed),
            "an errored call reads as failed"
        );
        let Some(content) = &update.fields.content else {
            panic!("content carried");
        };
        match &content[0] {
            acp::ToolCallContent::Content(block) => match &block.content {
                acp::ContentBlock::Text(text) => assert_eq!(text.text, "boom"),
                other => panic!("must be text, got {other:?}"),
            },
            other => panic!("must be content, got {other:?}"),
        }
    }

    #[test]
    fn tool_kind_table_covers_the_registry() {
        assert!(matches!(tool_kind("read_file"), acp::ToolKind::Read));
        assert!(matches!(tool_kind("list_dir"), acp::ToolKind::Read));
        assert!(matches!(tool_kind("artifact_read"), acp::ToolKind::Read));
        assert!(matches!(tool_kind("search"), acp::ToolKind::Search));
        assert!(matches!(tool_kind("write_file"), acp::ToolKind::Edit));
        assert!(matches!(tool_kind("edit_file"), acp::ToolKind::Edit));
        assert!(matches!(tool_kind("replace_symbol"), acp::ToolKind::Edit));
        assert!(matches!(tool_kind("run_command"), acp::ToolKind::Execute));
        assert!(matches!(tool_kind("verify"), acp::ToolKind::Execute));
        assert!(matches!(tool_kind("todo"), acp::ToolKind::Think));
        assert!(matches!(tool_kind("mystery"), acp::ToolKind::Other));
    }

    #[test]
    fn todo_updates_map_to_plan_entries() {
        let updates = updates_for(
            &session(),
            &EventKind::TodoUpdated {
                revision: 2,
                items: vec![
                    TodoItem {
                        id: "1".into(),
                        text: "first".into(),
                        status: TodoStatus::Done,
                    },
                    TodoItem {
                        id: "2".into(),
                        text: "second".into(),
                        status: TodoStatus::InProgress,
                    },
                ],
            },
        );
        assert_eq!(kinds(&updates), "plan");
        let acp::SessionUpdate::Plan(plan) = &updates[0] else {
            panic!("must be a plan");
        };
        assert_eq!(plan.entries.len(), 2);
        assert!(matches!(
            plan.entries[0].status,
            acp::PlanEntryStatus::Completed
        ));
        assert!(matches!(
            plan.entries[1].status,
            acp::PlanEntryStatus::InProgress
        ));
    }

    #[test]
    fn harness_telemetry_maps_to_nothing() {
        for kind in [
            EventKind::TurnStarted { turn: 1 },
            EventKind::ModelCompleted {
                response: loom_core::kernel::model::ModelResponse {
                    message: loom_core::kernel::transcript::AssistantMessage {
                        content: String::new(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        stop_reason: loom_core::kernel::model::StopReason::EndTurn,
                    },
                    usage: Default::default(),
                },
            },
            EventKind::LoopNudge {
                text: "nudge".into(),
            },
        ] {
            assert!(
                updates_for(&session(), &kind).is_empty(),
                "internal telemetry must not reach the editor"
            );
        }
    }

    #[test]
    fn stop_reasons_map_onto_the_acp_set() {
        assert!(matches!(
            stop_reason(&TaskError::Cancelled),
            Some(acp::StopReason::Cancelled)
        ));
        assert!(matches!(
            stop_reason(&TaskError::MaxTurns { max_turns: 3 }),
            Some(acp::StopReason::MaxTurnRequests)
        ));
        assert!(matches!(
            stop_reason(&TaskError::BudgetExhausted {
                limit: "input_tokens"
            }),
            Some(acp::StopReason::MaxTokens)
        ));
        assert!(matches!(
            stop_reason(&TaskError::Paused),
            Some(acp::StopReason::EndTurn)
        ));
        assert!(
            stop_reason(&TaskError::Internal("engine".into())).is_none(),
            "hard failures surface as errors, not stop reasons"
        );
    }

    #[test]
    fn prompt_text_flattens_text_blocks_only() {
        let blocks = vec![
            text_content("hello "),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("link", "file:///x")),
            text_content("world"),
        ];
        assert_eq!(prompt_text(&blocks), "hello world");
    }

    fn text_content(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text.to_owned()))
    }
}
