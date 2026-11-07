//! `loom chat`: the interactive session surface.
//!
//! `loom run` is one task per process; this module keeps one session
//! open across many tasks typed at a prompt. Everything below the CLI
//! is unchanged — the same [`Engine`], the same event stream, the same
//! persistence — so a chat session is resumable with `loom run
//! --session` and inspectable with `loom stats` / `loom rewind` while
//! it lives.
//!
//! Threading is deliberately plain: a worker thread owns the engine
//! and the session for the process's whole life (the engine is not
//! shared across threads), the main thread owns stdin and the prompt.
//! They speak through an mpsc channel and two shared cells — the
//! current session id (for `/rewind` and `/stats`) and a "task live"
//! flag (for prompt switching). Control flows the other way through
//! [`TaskControl`] clones, which is what they exist for (§6.5): a line
//! typed while a task runs steers it at the next turn boundary
//! (roadmap item 25), `/pause` stops after the current turn, `/cancel`
//! aborts mid-flight. The session survives all three.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use loom_core::Engine;
use loom_core::engine::TaskControl;
use loom_core::kernel::id::SessionId;

use crate::args::{ChatArgs, RewindArgs, StatsArgs};

/// What the main thread asks the worker to do.
enum WorkerMsg {
    /// Run one task on the session; `control` is the UI-side handle.
    Task { text: String, control: TaskControl },
    /// Drop the current session and start a fresh one.
    New,
    /// Continue from a session the store already holds (a `/rewind`
    /// fork).
    Adopt(SessionId),
}

/// What a typed line means, given whether a task is running. Parsed by
/// [`dispatch`] — a pure function, so the command surface is
/// unit-testable without an engine.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Whitespace only — re-prompt.
    Empty,
    /// A task for the session (idle) or steering for the running one.
    Text(String),
    Pause,
    Cancel,
    Exit,
    Help,
    Model,
    Sessions,
    Stats,
    New,
    /// `/rewind [IDX] [--restore]`: fork at a boundary (or list them
    /// without IDX); restore the workspace too.
    Rewind {
        at: Option<u32>,
        restore: bool,
    },
    /// A `/command` this surface does not know (carries the line for
    /// the hint).
    Unknown(String),
}

/// Interprets one input line. `/pause` and `/cancel` only mean
/// something while a task runs; task text means different things
/// idle (a new task) vs running (steering).
fn dispatch(line: &str, running: bool) -> Action {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Action::Empty;
    }
    let Some(command) = trimmed.strip_prefix('/') else {
        return Action::Text(trimmed.to_owned());
    };
    let mut words = command.split_whitespace();
    let head = words.next().unwrap_or_default();
    let rest: Vec<&str> = words.collect();
    match head {
        "help" | "?" => Action::Help,
        "exit" | "quit" | "q" => Action::Exit,
        "model" => Action::Model,
        "sessions" => Action::Sessions,
        "stats" => Action::Stats,
        "new" => Action::New,
        "pause" if running => Action::Pause,
        "cancel" if running => Action::Cancel,
        "rewind" => parse_rewind(&rest),
        _ => Action::Unknown(trimmed.to_owned()),
    }
}

/// `/rewind`, `/rewind IDX`, `/rewind IDX --restore`. Anything else
/// falls to [`Action::Unknown`] rather than guessing an index.
fn parse_rewind(rest: &[&str]) -> Action {
    let mut at = None;
    let mut restore = false;
    for word in rest {
        match *word {
            "--restore" => restore = true,
            digits if digits.bytes().all(|b| b.is_ascii_digit()) && !digits.is_empty() => {
                match digits.parse::<u32>() {
                    Ok(index) if at.is_none() => at = Some(index),
                    _ => return Action::Unknown(format!("/rewind {}", rest.join(" "))),
                }
            }
            _ => return Action::Unknown(format!("/rewind {}", rest.join(" "))),
        }
    }
    Action::Rewind { at, restore }
}

/// Runs the interactive session and returns the process exit code.
///
/// Setup failures propagate as `Err`; everything after the prompt is
/// interactive, so task failures print and the REPL continues — the
/// session is the durable artifact, not the exit code.
pub fn chat(args: ChatArgs) -> anyhow::Result<ExitCode> {
    if args.flags.best_of.is_some() {
        anyhow::bail!("chat does not support --best-of; run parallel rollouts with `loom run`");
    }
    if args.flags.trace.is_some() {
        eprintln!("note: --trace is per-task export; ignored in chat");
    }

    // Same preflight as `loom run`: a host that denies the sandbox
    // would fail every task one opaque `bwrap:` at a time.
    if std::env::var_os("LOOM_SKIP_SANDBOX_PREFLIGHT").is_none()
        && let Err(error) = loom_core::tools::sandbox::preflight()
    {
        anyhow::bail!(
            "sandbox preflight failed at setup:\n{error}\n\
             (Set LOOM_SKIP_SANDBOX_PREFLIGHT=1 to attempt the run anyway.)"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();

    let (config, overrides) = crate::run::engine_config_from(&args.flags)?;
    let engine = runtime.block_on(Engine::open_async_with(config, overrides))?;

    // The worker owns the engine and the session for the process's
    // life. A resumed session (`--session ID`) loads the persisted
    // transcript; a fresh one records the tier-1 start checkpoint so
    // `/rewind --restore` can reset the tree to this point.
    let json = args.flags.json;
    let (session, resumed) = match args.flags.session.as_deref() {
        Some(raw) => {
            let id = raw.parse().map_err(|_| {
                anyhow::anyhow!("parsing --session {raw:?} (expected an id from `loom sessions`)")
            })?;
            let session = engine
                .load_session(&id)?
                .ok_or_else(|| anyhow::anyhow!("unknown session {id} (see `loom sessions`)"))?;
            (session, true)
        }
        None => (engine.new_session(), false),
    };
    let session_id = Arc::new(Mutex::new(session.id));
    if !resumed {
        crate::run::record_start_checkpoint(&session.id, &args.flags.workspace, args.flags.no_save);
    }

    let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
    let task_live = Arc::new(AtomicBool::new(false));
    let worker_session_id = Arc::clone(&session_id);

    /// Reads the shared session id. The lock guards one copyable id;
    /// a poisoned lock still carries the last value written.
    fn shared_session_id(cell: &Mutex<SessionId>) -> SessionId {
        *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the shared session id (worker side).
    fn set_shared_session_id(cell: &Mutex<SessionId>, id: SessionId) {
        *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = id;
    }
    let worker_task_live = Arc::clone(&task_live);
    let workspace = args.flags.workspace.clone();
    let no_save = args.flags.no_save;
    let worker = std::thread::spawn(move || {
        let mut session = session;
        for message in rx {
            match message {
                WorkerMsg::Task { text, control } => {
                    // The consumers are per task, exactly like `loom
                    // run`: the renderer owns stdout for the task's
                    // duration, the decisions footer prints after.
                    let decision_log = crate::run::DecisionLog::default();
                    let (sink, renderer_thread, tracer_thread) =
                        crate::run::spawn_consumers(json, None, decision_log.clone());
                    let signal = handle.spawn(crate::run::ctrl_c_ladder(control.clone()));
                    let outcome =
                        handle.block_on(engine.run_task(&mut session, text, &sink, &control));
                    let _ =
                        crate::run::join_consumers(sink, signal, renderer_thread, tracer_thread);
                    match &outcome {
                        Ok(summary) => {
                            crate::output::print_summary(summary, "completed", &session.id, json)
                        }
                        Err(error) => crate::output::print_failure(error, json),
                    }
                    crate::run::print_decisions(&decision_log);
                    set_shared_session_id(&worker_session_id, session.id);
                    worker_task_live.store(false, Ordering::SeqCst);
                }
                WorkerMsg::New => {
                    session = engine.new_session();
                    crate::run::record_start_checkpoint(&session.id, &workspace, no_save);
                    set_shared_session_id(&worker_session_id, session.id);
                    println!("new session {}", session.id);
                }
                WorkerMsg::Adopt(id) => match engine.load_session(&id) {
                    Ok(Some(loaded)) => {
                        session = loaded;
                        set_shared_session_id(&worker_session_id, session.id);
                        println!("continuing from fork {id}");
                    }
                    Ok(None) => eprintln!("warning: fork {id} is not in the store"),
                    Err(error) => eprintln!("warning: could not adopt fork {id}: {error}"),
                },
            }
        }
    });

    eprintln!(
        "loom chat — provider {} model {} (max {} turns per task)",
        args.flags.provider, args.flags.model, args.flags.max_turns
    );
    eprintln!(
        "session {} — resume later with `loom run --session {}`",
        shared_session_id(&session_id),
        shared_session_id(&session_id)
    );
    eprintln!("type a task to run it; /help lists commands; Ctrl-D exits");
    if args.flags.bg_commands {
        eprintln!(
            "background commands on: `run_background` detaches, completions steer in at the next turn boundary"
        );
    }

    // The UI-side control handle for the running task, if any.
    let mut control: Option<TaskControl> = None;
    let mut running = false;
    let opening = args.task.join(" ");
    if !opening.trim().is_empty() {
        let task_control = TaskControl::new();
        control = Some(task_control.clone());
        task_live.store(true, Ordering::SeqCst);
        running = true;
        tx.send(WorkerMsg::Task {
            text: opening,
            control: task_control,
        })
        .ok();
    }

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        // A task that finished while we were blocked reading flips the
        // phase back before the next prompt.
        if running && !task_live.load(Ordering::SeqCst) {
            running = false;
            control = None;
        }
        if running {
            eprint!("(running — type to steer, /pause, /cancel) ");
        } else {
            eprint!("loom> ");
        }
        let _ = std::io::stderr().flush();
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break, // Ctrl-D or a closed pipe ends the chat.
            Ok(_) => {}
        }

        let live = running && task_live.load(Ordering::SeqCst);
        match dispatch(&line, live) {
            Action::Empty => {}
            Action::Text(text) => {
                if live {
                    if let Some(control) = &control {
                        control.steer(text);
                        eprintln!("(queued — lands at the next turn boundary)");
                    }
                } else {
                    let task_control = TaskControl::new();
                    control = Some(task_control.clone());
                    task_live.store(true, Ordering::SeqCst);
                    running = true;
                    if tx
                        .send(WorkerMsg::Task {
                            text,
                            control: task_control,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Action::Pause => {
                if let Some(control) = &control {
                    control.pause();
                    eprintln!("(pausing after the current turn)");
                }
            }
            Action::Cancel => {
                if let Some(control) = &control {
                    control.cancel();
                    eprintln!("(aborting…)");
                }
            }
            Action::Exit => {
                if let Some(control) = &control {
                    control.cancel();
                }
                break;
            }
            Action::Help => print_help(),
            Action::Model => {
                eprintln!(
                    "provider {} / model {} / max {} turns / session {}",
                    args.flags.provider,
                    args.flags.model,
                    args.flags.max_turns,
                    shared_session_id(&session_id),
                );
            }
            Action::Sessions => {
                let _ = crate::sessions_command::run();
            }
            Action::Stats => {
                let _ = crate::stats_command::run(StatsArgs {
                    trace: None,
                    session: Some(shared_session_id(&session_id).to_string()),
                    json,
                });
            }
            Action::New => {
                if live {
                    eprintln!("(a task is running; /new needs an idle session)");
                } else if tx.send(WorkerMsg::New).is_ok() {
                    running = false;
                    control = None;
                }
            }
            Action::Rewind { at, restore } => {
                if live {
                    eprintln!("(a task is running; /rewind needs an idle session)");
                    continue;
                }
                let id = shared_session_id(&session_id);
                let rewind = crate::rewind_command::rewind(&RewindArgs {
                    session: id.to_string(),
                    at,
                    restore_workspace: restore,
                    to_start: false,
                    accept_unrestorable: false,
                });
                match rewind {
                    Ok(Some(fork)) => {
                        if tx.send(WorkerMsg::Adopt(fork)).is_ok() {
                            running = false;
                            control = None;
                        }
                    }
                    Ok(None) => {} // boundary listing; the command printed it
                    Err(error) => eprintln!("{error:#}"),
                }
            }
            Action::Unknown(command) => {
                eprintln!("unknown command {command:?} — /help lists commands");
            }
        }
    }

    // Drop the channel so the worker's loop ends, then join it before
    // the runtime goes away (the worker holds only a Handle, which
    // does not keep the runtime alive).
    drop(tx);
    let _ = worker.join();
    runtime.shutdown_timeout(Duration::from_secs(5));
    Ok(ExitCode::SUCCESS)
}

/// The command surface, one line each — also the `/help` output.
fn print_help() {
    eprintln!(
        "commands:\n\
         \x20 /help, /model, /sessions, /stats — surface information\n\
         \x20 /rewind [IDX] [--restore] — list fork boundaries, or fork\n\
         \x20 /new — start a fresh session\n\
         \x20 /pause — stop after the current turn (running tasks only)\n\
         \x20 /cancel — abort the running task mid-flight\n\
         \x20 /exit (or Ctrl-D) — end the chat; the session stays saved\n\
         anything else is a task when idle, or steering while a task runs"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_lines_are_tasks_and_running_lines_steer() {
        assert_eq!(
            dispatch("fix the failing test", false),
            Action::Text("fix the failing test".into())
        );
        assert_eq!(
            dispatch("  fix the failing test  ", false),
            Action::Text("fix the failing test".into())
        );
        assert_eq!(
            dispatch("keep the diff minimal", true),
            Action::Text("keep the diff minimal".into())
        );
    }

    #[test]
    fn blank_lines_re_prompt() {
        assert_eq!(dispatch("", false), Action::Empty);
        assert_eq!(dispatch("   ", true), Action::Empty);
    }

    #[test]
    fn command_words_and_aliases_map() {
        assert_eq!(dispatch("/help", false), Action::Help);
        assert_eq!(dispatch("/?", false), Action::Help);
        assert_eq!(dispatch("/exit", false), Action::Exit);
        assert_eq!(dispatch("/q", true), Action::Exit);
        assert_eq!(dispatch("/model", false), Action::Model);
        assert_eq!(dispatch("/sessions", false), Action::Sessions);
        assert_eq!(dispatch("/stats", false), Action::Stats);
        assert_eq!(dispatch("/new", false), Action::New);
    }

    #[test]
    fn pause_and_cancel_need_a_running_task() {
        assert_eq!(dispatch("/pause", true), Action::Pause);
        assert_eq!(dispatch("/cancel", true), Action::Cancel);
        // Idle, they are unknowns with a helpful hint, not no-ops —
        // silence would look like the command working.
        assert_eq!(dispatch("/pause", false), Action::Unknown("/pause".into()));
        assert_eq!(
            dispatch("/cancel", false),
            Action::Unknown("/cancel".into())
        );
    }

    #[test]
    fn rewind_argument_shapes_parse() {
        assert_eq!(
            dispatch("/rewind", false),
            Action::Rewind {
                at: None,
                restore: false
            }
        );
        assert_eq!(
            dispatch("/rewind 12", false),
            Action::Rewind {
                at: Some(12),
                restore: false
            }
        );
        assert_eq!(
            dispatch("/rewind 12 --restore", false),
            Action::Rewind {
                at: Some(12),
                restore: true
            }
        );
        // Non-numeric, negative, or repeated indices refuse rather
        // than guess.
        assert_eq!(
            dispatch("/rewind abc", false),
            Action::Unknown("/rewind abc".into())
        );
        assert_eq!(
            dispatch("/rewind 3 4", false),
            Action::Unknown("/rewind 3 4".into())
        );
    }

    #[test]
    fn unknown_slash_commands_carry_their_line() {
        assert_eq!(
            dispatch("/frobnicate x", false),
            Action::Unknown("/frobnicate x".into())
        );
    }
}
