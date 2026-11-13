//! Official interactive terminal frontend for Loom.
//!
//! ```sh
//! DEEPSEEK_API_KEY=… cargo run -p loom-tui -- \
//!     --provider deepseek --model deepseek-chat "inspect this repository"
//! ```
//!
//! `i` queues steering, `p` pauses at the next turn boundary, Esc cancels,
//! and `q` cancels and exits. The UI intentionally depends on the stable
//! `loom-core` facade; rendering remains isolated in this crate.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
use loom_core::{Engine, EngineConfig, EventEnvelope, SessionId, TaskControl, TaskSummary};
use loom_kernel::event::{ChannelEventSink, EventKind};
use loom_kernel::transcript::ToolStatus;
use ratatui::style::Color;

const EVENT_LOG_LIMIT: usize = 240;
pub(crate) const BG: Color = Color::Rgb(18, 16, 14);
pub(crate) const PANEL: Color = Color::Rgb(26, 22, 18);
pub(crate) const BORDER: Color = Color::Rgb(57, 51, 40);
pub(crate) const IVORY: Color = Color::Rgb(233, 227, 215);
pub(crate) const DIM: Color = Color::Rgb(154, 146, 133);
pub(crate) const AMBER: Color = Color::Rgb(227, 168, 87);
pub(crate) const SAGE: Color = Color::Rgb(134, 193, 159);
pub(crate) const CORAL: Color = Color::Rgb(238, 130, 114);
pub(crate) const VIOLET: Color = Color::Rgb(169, 155, 214);
pub(crate) const BLUE: Color = Color::Rgb(141, 180, 232);
pub(crate) const TEAL: Color = Color::Rgb(111, 196, 196);

enum WorkerMessage {
    Started(SessionId),
    Finished(Result<TaskSummary, String>),
}

enum TaskCommand {
    Run(String, TaskControl),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct LogItem {
    pub(crate) marker: &'static str,
    pub(crate) color: Color,
    pub(crate) text: String,
    pub(crate) provisional: bool,
    pub(crate) italic: bool,
    pub(crate) detail: Option<String>,
    pub(crate) expanded: bool,
}

#[derive(Clone)]
pub(crate) struct RetryRecord {
    pub(crate) attempt: u32,
    pub(crate) kind: String,
    pub(crate) delay_ms: u64,
    pub(crate) response_started: bool,
}

#[derive(Clone)]
pub(crate) struct ToolRecord {
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) call_id: String,
    pub(crate) state: ToolRecordState,
    pub(crate) detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolRecordState {
    Running,
    Success,
    Failed,
    Interrupted,
    NotRun,
}

#[derive(Default)]
pub(crate) struct App {
    pub(crate) task: String,
    pub(crate) model: String,
    pub(crate) session: Option<SessionId>,
    pub(crate) run_id: u32,
    pub(crate) timeline: VecDeque<LogItem>,
    pub(crate) recent_events: VecDeque<String>,
    pub(crate) status: String,
    pub(crate) status_color: Color,
    pub(crate) input: String,
    pub(crate) editing: bool,
    pub(crate) finished: bool,
    pub(crate) quit_after_finish: bool,
    pub(crate) turns: u32,
    pub(crate) requests: u32,
    pub(crate) retries: u32,
    pub(crate) max_attempts: u32,
    pub(crate) tools_ok: u32,
    pub(crate) tools_failed: u32,
    pub(crate) tools_cancelled: u32,
    pub(crate) input_tokens: u64,
    pub(crate) cached_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) wall_time: Duration,
    pub(crate) events_seen: u64,
    pub(crate) events_dropped: u64,
    pub(crate) provisional_index: Option<usize>,
    pub(crate) provisional_reasoning_index: Option<usize>,
    pub(crate) current_turn_index: Option<usize>,
    pub(crate) current_turn_input: u64,
    pub(crate) current_turn_output: u64,
    pub(crate) reasoning_chars: usize,
    pub(crate) started_at: Option<Instant>,
    pub(crate) elapsed: Duration,
    pub(crate) sidebar_visible: bool,
    pub(crate) show_help: bool,
    pub(crate) show_reasoning: bool,
    pub(crate) selected_item: Option<usize>,
    pub(crate) retries_log: Vec<RetryRecord>,
    pub(crate) tools: Vec<ToolRecord>,
    pub(crate) tool_indices: HashMap<String, usize>,
    pub(crate) last_cache_pct: String,
    pub(crate) last_interrupt: Option<Instant>,
}

/// Run the official interactive terminal frontend.
pub fn run() -> anyhow::Result<()> {
    let (provider, model, task) = parse_args()?;
    let (sink, mut event_rx) = ChannelEventSink::new();
    let (worker_tx, worker_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let mut control = TaskControl::new();
    let worker_control = control.clone();
    let shown_model = model.clone();
    let shown_task = task.clone();

    std::thread::Builder::new()
        .name("loom-tui-task".into())
        .spawn(move || {
            run_session(
                provider,
                model,
                task,
                sink,
                worker_control,
                &worker_tx,
                command_rx,
            );
        })
        .context("start Loom task thread")?;

    let mut app = App {
        task: shown_task,
        model: shown_model,
        status: "Starting".into(),
        status_color: AMBER,
        max_attempts: 4,
        started_at: Some(Instant::now()),
        ..App::default()
    };

    let result = ratatui::run(|terminal| {
        loop {
            drain_worker(&worker_rx, &mut app);
            events::drain_events(&mut event_rx, &mut app);
            if !app.finished {
                app.elapsed = app
                    .started_at
                    .map_or(Duration::ZERO, |start| start.elapsed());
            }
            terminal.draw(|frame| ui::draw(frame, &app))?;

            if app.finished && app.quit_after_finish {
                break Ok(());
            }
            if poll(Duration::from_millis(33))?
                && let Event::Key(key) = read()?
                && key.kind == KeyEventKind::Press
            {
                handle_key(key, &mut app, &command_tx, &mut control);
            }
        }
    });
    if result.is_err() {
        control.cancel();
    }
    let _ = command_tx.send(TaskCommand::Shutdown);
    result
}

fn parse_args() -> anyhow::Result<(String, String, String)> {
    let mut args = std::env::args().skip(1);
    let mut provider = None;
    let mut model = None;
    let mut task = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--provider" => provider = args.next(),
            "--model" => model = args.next(),
            "--" => {
                task.extend(args.by_ref());
                break;
            }
            _ if arg.starts_with('-') => bail!("unknown option: {arg}"),
            _ => task.push(arg),
        }
    }
    let provider = provider.context("usage: tui --provider P --model M TASK…")?;
    let model = model.context("usage: tui --provider P --model M TASK…")?;
    let task = task.join(" ");
    anyhow::ensure!(!task.is_empty(), "usage: tui --provider P --model M TASK…");
    Ok((provider, model, task))
}

fn run_session(
    provider: String,
    model: String,
    task: String,
    sink: ChannelEventSink,
    control: TaskControl,
    messages: &mpsc::Sender<WorkerMessage>,
    commands: mpsc::Receiver<TaskCommand>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string());
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = messages.send(WorkerMessage::Finished(Err(error)));
            return;
        }
    };
    let engine = runtime.block_on(async {
        Engine::open_async(EngineConfig {
            provider,
            model,
            base_url: None,
            models_url: None,
            headers: Vec::new(),
            reasoning_effort: Default::default(),
            account: None,
            workspace: std::env::current_dir().map_err(|error| error.to_string())?,
            system_prompt: None,
            max_turns: 50,
            turn_budget: Default::default(),
            context_editing: None,
            db_path: loom_core::paths::db_path(),
            persist: false,
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
        })
        .await
        .map_err(|error| error.to_string())
    });
    let engine = match engine {
        Ok(engine) => engine,
        Err(error) => {
            let _ = messages.send(WorkerMessage::Finished(Err(error)));
            return;
        }
    };
    let mut session = engine.new_session();
    let _ = messages.send(WorkerMessage::Started(session.id));
    let mut next = Some((task, control));
    while let Some((task, control)) = next.take() {
        let result = runtime
            .block_on(engine.run_task(&mut session, task, &sink, &control))
            .map_err(|error| error.to_string());
        let _ = messages.send(WorkerMessage::Finished(result));
        match commands.recv() {
            Ok(TaskCommand::Run(task, control)) => {
                let _ = messages.send(WorkerMessage::Started(session.id));
                next = Some((task, control));
            }
            Ok(TaskCommand::Shutdown) | Err(_) => break,
        }
    }
}

fn drain_worker(receiver: &mpsc::Receiver<WorkerMessage>, app: &mut App) {
    loop {
        match receiver.try_recv() {
            Ok(WorkerMessage::Started(session)) => {
                app.session = Some(session);
                if app.finished {
                    app.run_id = app.run_id.saturating_add(1);
                }
                app.status = "Running".into();
                app.status_color = AMBER;
            }
            Ok(WorkerMessage::Finished(Ok(summary))) => {
                app.finished = true;
                app.turns = summary.turns;
                app.requests = summary.model_requests;
                app.input_tokens = summary.input_tokens;
                app.output_tokens = summary.output_tokens;
                app.cached_tokens = summary.cached_input_tokens.unwrap_or_default();
                app.events_dropped = summary.events_dropped;
                app.elapsed = summary.wall_time;
                app.wall_time = summary.wall_time;
                if app.status == "Running" || app.status == "Starting" {
                    app.status = "Completed".into();
                    app.status_color = SAGE;
                }
            }
            Ok(WorkerMessage::Finished(Err(error))) => {
                app.finished = true;
                app.elapsed = app
                    .started_at
                    .map_or(Duration::ZERO, |start| start.elapsed());
                app.wall_time = app.elapsed;
                app.status = format!("Failed · {error}");
                app.status_color = CORAL;
                if !app
                    .timeline
                    .iter()
                    .any(|item| item.text.starts_with("Task failed"))
                {
                    events::push_log(app, "×", CORAL, format!("Task failed · {error}"), false);
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

mod events;

fn handle_key(
    key: KeyEvent,
    app: &mut App,
    commands: &mpsc::Sender<TaskCommand>,
    control: &mut TaskControl,
) {
    let code = key.code;
    if key.modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('o') {
        toggle_expanded(app);
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('r') {
        app.show_reasoning = !app.show_reasoning;
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        if app.finished {
            app.quit_after_finish = true;
            return;
        }
        let now = Instant::now();
        if app
            .last_interrupt
            .is_some_and(|last| now.duration_since(last) <= Duration::from_secs(2))
        {
            app.quit_after_finish = true;
        } else {
            app.last_interrupt = Some(now);
            control.cancel();
            app.status = "Cancelled by you".into();
            app.status_color = AMBER;
        }
        return;
    }
    if app.editing {
        match code {
            KeyCode::Esc => {
                app.editing = false;
                app.input.clear();
            }
            KeyCode::Enter => {
                if !app.input.trim().is_empty() {
                    let message = std::mem::take(&mut app.input);
                    if app.finished {
                        *control = TaskControl::new();
                        app.finished = false;
                        app.quit_after_finish = false;
                        app.status = "Starting next task".into();
                        app.status_color = AMBER;
                        app.run_id = app.run_id.saturating_add(1);
                        app.started_at = Some(Instant::now());
                        app.elapsed = Duration::ZERO;
                        app.turns = 0;
                        app.requests = 0;
                        app.retries = 0;
                        app.tools_ok = 0;
                        app.tools_failed = 0;
                        app.tools_cancelled = 0;
                        app.input_tokens = 0;
                        app.cached_tokens = 0;
                        app.output_tokens = 0;
                        app.wall_time = Duration::ZERO;
                        app.tools.clear();
                        app.tool_indices.clear();
                        app.retries_log.clear();
                        app.current_turn_index = None;
                        app.provisional_index = None;
                        app.provisional_reasoning_index = None;
                        app.task = message.clone();
                        app.timeline.clear();
                        app.recent_events.clear();
                        events::push_log(app, "›", BLUE, format!("next task · {}", message), false);
                        let _ = commands.send(TaskCommand::Run(message, control.clone()));
                    } else {
                        control.steer(message);
                        app.status = "Steering queued".into();
                        app.status_color = AMBER;
                    }
                }
                app.editing = false;
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(character) => app.input.push(character),
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Char('i') if !app.finished => {
            app.editing = true;
            app.input.clear();
        }
        KeyCode::Char('i') if app.finished => {
            app.editing = true;
            app.input.clear();
        }
        KeyCode::Up if app.finished => {
            app.editing = true;
            app.input = app.task.clone();
        }
        KeyCode::Char('p') if !app.finished => {
            control.pause();
            app.status = "Pausing…".into();
            app.status_color = AMBER;
        }
        KeyCode::Esc if !app.finished => {
            control.cancel();
            app.status = "Cancelling…".into();
            app.status_color = AMBER;
        }
        KeyCode::Char('q') if !app.finished => {
            app.quit_after_finish = true;
            control.cancel();
            app.status = "Cancelling before exit…".into();
            app.status_color = AMBER;
        }
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter if app.finished => {
            app.quit_after_finish = true
        }
        KeyCode::Tab => app.sidebar_visible = !app.sidebar_visible,
        KeyCode::Char('?') => app.show_help = !app.show_help,
        KeyCode::Up => {
            let end = app.timeline.len().saturating_sub(1);
            app.selected_item = Some(app.selected_item.unwrap_or(end).saturating_sub(1));
        }
        KeyCode::Down => {
            let end = app.timeline.len().saturating_sub(1);
            app.selected_item = Some((app.selected_item.unwrap_or(0) + 1).min(end));
        }
        _ => {}
    }
}

fn toggle_expanded(app: &mut App) {
    if app.timeline.is_empty() {
        return;
    }
    let index = app
        .selected_item
        .unwrap_or_else(|| app.timeline.len().saturating_sub(1));
    if let Some(item) = app.timeline.get_mut(index)
        && item.detail.is_some()
    {
        item.expanded = !item.expanded;
    }
}

mod ui;

#[cfg(test)]
mod tests {
    use super::events::{display_marker, reduce_event};
    use super::ui::{retry_progress, short_id, status_glyph};
    use super::*;
    use loom_kernel::id::TaskId;
    use loom_kernel::model::StopReason;
    use loom_kernel::model::{ModelResponse, Usage};
    use loom_kernel::transcript::AssistantMessage;

    fn event(sequence: u64, kind: EventKind) -> EventEnvelope {
        EventEnvelope {
            schema_version: loom_kernel::event::EVENT_SCHEMA_VERSION,
            sequence,
            task_id: TaskId::new(),
            kind,
        }
    }

    #[test]
    fn retry_discards_provisional_output_and_records_attempt() {
        let mut app = App::default();
        reduce_event(
            &event(1, EventKind::ModelRequestStarted { attempt: 1 }),
            &mut app,
        );
        reduce_event(
            &event(
                2,
                EventKind::TextDelta {
                    text: "partial".into(),
                },
            ),
            &mut app,
        );
        reduce_event(
            &event(
                3,
                EventKind::ModelRetry {
                    attempt: 1,
                    kind: loom_kernel::model::ModelErrorKind::RateLimited,
                    delay_ms: 250,
                    response_started: true,
                },
            ),
            &mut app,
        );

        assert!(app.timeline.iter().all(|item| item.text != "partial"));
        assert_eq!(app.retries, 1);
        assert_eq!(app.retries_log[0].delay_ms, 250);
        assert!(app.retries_log[0].response_started);
    }

    #[test]
    fn completed_response_commits_authoritative_content_and_usage() {
        let mut app = App::default();
        reduce_event(
            &event(1, EventKind::ModelRequestStarted { attempt: 1 }),
            &mut app,
        );
        reduce_event(
            &event(
                2,
                EventKind::TextDelta {
                    text: "streamed".into(),
                },
            ),
            &mut app,
        );
        let response = ModelResponse {
            message: AssistantMessage {
                content: "authoritative".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage {
                input_tokens: 100,
                output_tokens: 25,
                cached_input_tokens: Some(60),
                ..Usage::default()
            },
        };
        reduce_event(&event(3, EventKind::ModelCompleted { response }), &mut app);

        assert!(app.timeline.iter().any(|item| item.text == "authoritative"));
        assert!(app.timeline.iter().all(|item| item.text != "streamed"));
        assert_eq!(app.input_tokens, 100);
        assert_eq!(app.output_tokens, 25);
        assert_eq!(app.cached_tokens, 60);
    }

    #[test]
    fn status_glyphs_are_semantic_and_duration_is_compact() {
        assert_eq!(status_glyph("Completed"), "✓");
        assert_eq!(status_glyph("Failed"), "×");
        assert_eq!(status_glyph("Cancelled"), "⊘");
        assert_eq!(ui::format_duration(Duration::from_secs(125)), "02:05");
    }

    #[test]
    fn ascii_mode_uses_readable_timeline_markers() {
        assert_eq!(display_marker("↻", true), "~");
        assert_eq!(display_marker("✓", true), "+");
        assert_eq!(display_marker("↻", false), "↻");
    }

    #[test]
    fn cancellation_keeps_committed_events_and_marks_partial_output() {
        let mut app = App::default();
        reduce_event(&event(1, EventKind::TaskStarted), &mut app);
        reduce_event(
            &event(
                2,
                EventKind::TextDelta {
                    text: "partial".into(),
                },
            ),
            &mut app,
        );
        reduce_event(&event(3, EventKind::TaskCancelled), &mut app);
        assert!(app.timeline.iter().any(|item| item.text == "Task accepted"));
        assert!(
            app.timeline
                .iter()
                .any(|item| item.text.contains("interrupted"))
        );
        assert!(app.timeline.iter().all(|item| item.text != "partial"));
        assert_eq!(app.status, "Cancelled");
    }

    #[test]
    fn short_id_is_safe_for_short_values() {
        assert_eq!(short_id("1234567890"), "12345678");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn retry_progress_uses_attempt_number_and_caps_at_maximum() {
        let retries = vec![RetryRecord {
            attempt: 3,
            kind: "overloaded".into(),
            delay_ms: 8000,
            response_started: false,
        }];
        assert_eq!(retry_progress(&retries, 4), (3, 4));
        assert_eq!(retry_progress(&retries, 2), (2, 2));
        assert_eq!(retry_progress(&[], 4), (0, 4));
    }
}
