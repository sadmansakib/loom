//! The ACP server loop: registering handlers on the agent side of a
//! connection and the per-request handlers behind them.

use std::sync::Arc;

use agent_client_protocol::role::acp::{Agent, Client};
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{ConnectTo, Error};
use loom_core::Engine;
use loom_core::kernel::agent::TaskControl;
use loom_core::kernel::id::SessionId as KernelSessionId;
use loom_core::kernel::session::Session;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::mcp::forwarded_set;
use crate::permission::ask_permission;
use crate::session::{
    AcpState, ActivePrompt, SessionCore, SessionHandle, SessionReservation, engine_config,
    kernel_session_id, lock_or_recover, reserve_prompt,
};
use crate::updates::{SessionUpdateSink, prompt_text, stop_reason};

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
    let load_state = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state.clone();
    Agent
        .v1()
        .on_receive_request(
            move |request: acp::InitializeRequest,
                  responder: agent_client_protocol::v1::Responder<acp::InitializeResponse>,
                  _cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                let response = acp::InitializeResponse::new(request.protocol_version)
                    .agent_capabilities(acp::AgentCapabilities::new().load_session(true))
                    .agent_info(acp::Implementation::new("loom", env!("CARGO_PKG_VERSION")));
                async move { responder.respond(response) }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: acp::LoadSessionRequest,
                  responder: agent_client_protocol::v1::Responder<acp::LoadSessionResponse>,
                  cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                // Engine open (and, with forwarded servers, process
                // spawn) is heavyweight; keep it off the dispatch loop.
                let state = load_state.clone();
                let task_cx = cx.clone();
                async move {
                    cx.spawn(async move {
                        handle_load_session(state, request, responder, task_cx).await
                    })
                }
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
                let prompt = reserve_prompt(&state, &request.session_id);
                async move {
                    match prompt {
                        Ok((handle, control, active_prompt)) => cx.spawn(async move {
                            handle_prompt(
                                request,
                                responder,
                                task_cx,
                                handle,
                                control,
                                active_prompt,
                            )
                            .await
                        }),
                        Err(message) => {
                            responder.respond_with_error(Error::invalid_request().data(message))
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            move |notification: acp::CancelNotification,
                  _cx: agent_client_protocol::v1::ConnectionTo<Client>| {
                let handle = lock_or_recover(&cancel_state.sessions)
                    .sessions
                    .get(&notification.session_id)
                    .cloned();
                let control =
                    handle.and_then(|handle| lock_or_recover(&handle.active_prompt).clone());
                if let Some(control) = control {
                    control.cancel();
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
    let config = engine_config(&options, request.cwd.clone());
    // The ACP session id *is* the kernel session id in string form, so
    // a later connection's `session/load` names the same durable
    // transcript. Kernel ids are UUIDv7; editors treat them as opaque
    // strings either way.
    let kernel = KernelSessionId::new();
    let acp_session_id = acp::SessionId::new(kernel.as_uuid().to_string());
    let Some(reservation) = SessionReservation::reserve(&state.sessions, acp_session_id.clone())
    else {
        return responder.respond_with_internal_error("generated a duplicate session id");
    };
    // The ask bridge: a policy `Ask` becomes the client's
    // `session/request_permission`. Fails closed on any transport
    // failure or non-allow outcome, matching the engine's no-resolver
    // default. Built before the engine opens so the resolver rides in
    // the overrides.
    let ask_cx = cx.clone();
    let ask_session = acp_session_id.clone();
    let ask_timeout = state.options.permission_timeout;
    let mut overrides = (options.overrides_factory)();
    overrides.ask = Some(Arc::new(move |request| {
        let cx = ask_cx.clone();
        let session = ask_session.clone();
        // Full-argv approval (review F-13): the operator sees the
        // exact command line, not just the program name — `sh` can no
        // longer hide `-c 'curl … | sh'` behind a one-word title.
        let program = request.program.clone();
        let line = request.render();
        let permission = ask_permission(cx, session, program, line, ask_timeout);
        Box::pin(permission)
    }));
    // Editor-forwarded servers (item 37 tail): stdio-only, admitted
    // only under the operator flag; without it they are warned about
    // and ignored, byte-for-byte the pre-flag behavior. A non-stdio
    // entry under the flag is a hard `session/new` error — silently
    // attaching half the request would hide the refusal.
    let forwarded = match forwarded_set(&state, &request) {
        Ok(decls) => decls,
        Err(message) => {
            return responder.respond_with_error(Error::invalid_request().data(message));
        }
    };
    let engine = match Engine::open_async_with_forwarded_mcp(config, overrides, forwarded).await {
        Ok(engine) => engine,
        Err(error) => {
            return responder.respond_with_internal_error(format!("opening the engine: {error}"));
        }
    };
    let core = Arc::new(AsyncMutex::new(SessionCore {
        engine: Arc::new(engine),
        session: Session::new(kernel),
    }));
    reservation.install(SessionHandle::new(core));
    responder.respond(acp::NewSessionResponse::new(acp_session_id))
}

/// `session/load`: resume a persisted kernel session under the ACP id
/// that created it. Opens a fresh engine (the connection may be a new
/// process) and rehydrates the durable transcript from the shared
/// database; a later `session/prompt` on the same id continues it.
async fn handle_load_session(
    state: AcpState,
    request: acp::LoadSessionRequest,
    responder: agent_client_protocol::v1::Responder<acp::LoadSessionResponse>,
    _cx: agent_client_protocol::v1::ConnectionTo<Client>,
) -> Result<(), Error> {
    // The ACP id is the kernel id's string form: a UUIDv7. Anything
    // else never named a loom session.
    let kernel = match kernel_session_id(&request.session_id) {
        Ok(kernel) => kernel,
        Err(message) => {
            return responder.respond_with_error(Error::invalid_request().data(message));
        }
    };
    let Some(reservation) =
        SessionReservation::reserve(&state.sessions, request.session_id.clone())
    else {
        return responder.respond_with_error(
            Error::invalid_request()
                .data("session is already active or loading on this connection"),
        );
    };
    let options = state.options.clone();
    let config = engine_config(&options, request.cwd.clone());
    let engine = match Engine::open_async_with(config, (options.overrides_factory)()).await {
        Ok(engine) => engine,
        Err(error) => {
            return responder.respond_with_internal_error(format!("opening the engine: {error}"));
        }
    };
    // `None` covers both "persistence off" and "no such id" — same
    // answer either way: the client named something we cannot resume.
    // `None` covers both "persistence off" and "no such id" — same
    // answer either way: the client named something we cannot resume.
    let Ok(Some(session)) = engine.load_session(&kernel) else {
        return responder.respond_with_error(Error::invalid_request().data("unknown session"));
    };
    let core = Arc::new(AsyncMutex::new(SessionCore {
        engine: Arc::new(engine),
        session,
    }));
    reservation.install(SessionHandle::new(core));
    // The loaded session keeps its id: response and request agree by
    // construction, and modes/config options stay per-connection.
    responder.respond(acp::LoadSessionResponse::new())
}

/// `session/prompt`: run one task to completion, forwarding updates,
/// then answer with the mapped stop reason.
const UPDATE_BUFFER: usize = 128;

async fn handle_prompt(
    request: acp::PromptRequest,
    responder: agent_client_protocol::v1::Responder<acp::PromptResponse>,
    cx: agent_client_protocol::v1::ConnectionTo<Client>,
    handle: SessionHandle,
    control: TaskControl,
    _active_prompt: ActivePrompt,
) -> Result<(), Error> {
    let prompt = prompt_text(&request.prompt);
    let (tx, mut rx) = mpsc::channel(UPDATE_BUFFER);
    let sink = SessionUpdateSink {
        tx,
        control: control.clone(),
    };
    let forward_session = request.session_id.clone();
    let forward_cx = cx;
    let forward_control = control.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            let notification = acp::SessionNotification::new(forward_session.clone(), update);
            if forward_cx.send_notification(notification).is_err() {
                // The peer is gone; stop the engine task instead of
                // continuing work whose updates can no longer be read.
                forward_control.cancel();
                break;
            }
        }
    });
    let mut core = handle.core.lock().await;
    let engine = core.engine.clone();
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
