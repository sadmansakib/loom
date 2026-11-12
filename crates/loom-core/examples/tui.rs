//! A small, interactive terminal frontend built only on the `loom-core` facade.
//!
//! ```sh
//! DEEPSEEK_API_KEY=… cargo run -p loom-core --example tui -- \
//!     --provider deepseek --model deepseek-chat "inspect this repository"
//! ```
//!
//! `i` queues steering, `p` pauses at the next turn boundary, Esc cancels,
//! and `q` cancels and exits. This is an example frontend, not a second CLI.

use std::collections::VecDeque;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use anyhow::{Context, bail};
use crossterm::event::{Event, KeyCode, KeyEventKind, poll, read};
use loom_core::kernel::event::{ChannelEventSink, EventKind};
use loom_core::{Engine, EngineConfig, EventEnvelope, SessionId, TaskControl, TaskSummary};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

const EVENT_LOG_LIMIT: usize = 240;
const BG: Color = Color::Rgb(18, 17, 14);
const PANEL: Color = Color::Rgb(24, 22, 18);
const BORDER: Color = Color::Rgb(57, 51, 40);
const IVORY: Color = Color::Rgb(233, 227, 215);
const DIM: Color = Color::Rgb(154, 146, 133);
const AMBER: Color = Color::Rgb(227, 168, 87);
const SAGE: Color = Color::Rgb(134, 193, 159);
const CORAL: Color = Color::Rgb(238, 130, 114);
const VIOLET: Color = Color::Rgb(169, 155, 214);
const BLUE: Color = Color::Rgb(141, 180, 232);
const TEAL: Color = Color::Rgb(111, 196, 196);

enum WorkerMessage {
    Started(SessionId),
    Finished(Result<TaskSummary, String>),
}

#[derive(Clone)]
struct LogItem {
    marker: &'static str,
    color: Color,
    text: String,
    provisional: bool,
    italic: bool,
}

#[derive(Default)]
struct App {
    task: String,
    model: String,
    session: Option<SessionId>,
    timeline: VecDeque<LogItem>,
    recent_events: VecDeque<String>,
    status: String,
    status_color: Color,
    input: String,
    editing: bool,
    finished: bool,
    quit_after_finish: bool,
    turns: u32,
    requests: u32,
    retries: u32,
    tools_ok: u32,
    tools_failed: u32,
    tools_cancelled: u32,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    events_seen: u64,
    events_dropped: u64,
    provisional_index: Option<usize>,
    provisional_reasoning_index: Option<usize>,
    reasoning_chars: usize,
}

fn main() -> anyhow::Result<()> {
    let (provider, model, task) = parse_args()?;
    let (sink, mut event_rx) = ChannelEventSink::new();
    let (worker_tx, worker_rx) = mpsc::channel();
    let control = TaskControl::new();
    let worker_control = control.clone();
    let shown_model = model.clone();
    let shown_task = task.clone();

    std::thread::Builder::new()
        .name("loom-tui-task".into())
        .spawn(move || {
            let result = run_task(provider, model, task, sink, worker_control, &worker_tx);
            let _ = worker_tx.send(WorkerMessage::Finished(result));
        })
        .context("start Loom task thread")?;

    let app = App {
        task: shown_task,
        model: shown_model,
        status: "Starting".into(),
        status_color: AMBER,
        ..App::default()
    };
    let mut app = app;

    let result = ratatui::run(|terminal| {
        loop {
            drain_worker(&worker_rx, &mut app);
            drain_events(&mut event_rx, &mut app);
            terminal.draw(|frame| draw(frame, &app))?;

            if app.finished && app.quit_after_finish {
                break Ok(());
            }
            if poll(Duration::from_millis(33))? {
                if let Event::Key(key) = read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key(key.code, &mut app, &control);
                    }
                }
            }
        }
    });
    if result.is_err() {
        control.cancel();
    }
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

fn run_task(
    provider: String,
    model: String,
    task: String,
    sink: ChannelEventSink,
    control: TaskControl,
    messages: &mpsc::Sender<WorkerMessage>,
) -> Result<TaskSummary, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let engine = Engine::open_async(EngineConfig {
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
        .map_err(|error| error.to_string())?;
        let mut session = engine.new_session();
        let _ = messages.send(WorkerMessage::Started(session.id));
        engine
            .run_task(&mut session, task, &sink, &control)
            .await
            .map_err(|error| error.to_string())
    })
}

fn drain_worker(receiver: &mpsc::Receiver<WorkerMessage>, app: &mut App) {
    loop {
        match receiver.try_recv() {
            Ok(WorkerMessage::Started(session)) => {
                app.session = Some(session);
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
                if app.status == "Running" || app.status == "Starting" {
                    app.status = "Completed".into();
                    app.status_color = SAGE;
                }
            }
            Ok(WorkerMessage::Finished(Err(error))) => {
                app.finished = true;
                app.status = format!("Failed · {error}");
                app.status_color = CORAL;
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

fn drain_events(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<EventEnvelope>, app: &mut App) {
    while let Ok(envelope) = receiver.try_recv() {
        app.events_seen = envelope.sequence;
        reduce_event(&envelope, app);
        app.recent_events.push_front(format!(
            "#{:<5} {}",
            envelope.sequence,
            event_name(&envelope.kind)
        ));
        app.recent_events.truncate(5);
    }
}

fn reduce_event(envelope: &EventEnvelope, app: &mut App) {
    match &envelope.kind {
        EventKind::TaskStarted => push_log(app, "›", BLUE, "Task accepted", false),
        EventKind::TurnStarted { turn } => {
            app.turns = *turn;
            push_log(app, "●", AMBER, format!("turn {turn}"), false);
        }
        EventKind::ModelRequestStarted { attempt } => {
            app.requests += 1;
            app.reasoning_chars = 0;
            app.provisional_index = None;
            app.status = format!("Streaming · attempt {attempt}");
            app.status_color = AMBER;
            push_log(
                app,
                "⁙",
                AMBER,
                format!("request · attempt {attempt}"),
                false,
            );
        }
        EventKind::ReasoningDelta { text } => {
            app.reasoning_chars += text.chars().count();
            let index = match app.provisional_reasoning_index {
                Some(index) => index,
                None => {
                    push_log(app, "▸", VIOLET, "reasoning · provisional", true);
                    let index = app.timeline.len() - 1;
                    app.provisional_reasoning_index = Some(index);
                    index
                }
            };
            if let Some(item) = app.timeline.get_mut(index) {
                item.text.push_str(text);
                item.italic = true;
            }
        }
        EventKind::TextDelta { text } => {
            let index = match app.provisional_index {
                Some(index) => index,
                None => {
                    push_log(app, "┆", VIOLET, "streaming · provisional", true);
                    let index = app.timeline.len() - 1;
                    app.provisional_index = Some(index);
                    index
                }
            };
            if let Some(item) = app.timeline.get_mut(index) {
                item.text.push_str(text);
            }
        }
        EventKind::ModelRetry {
            attempt,
            kind,
            delay_ms,
            ..
        } => {
            app.retries += 1;
            app.reasoning_chars = 0;
            discard_provisional(app);
            discard_provisional_reasoning(app);
            app.status = format!("Retrying · attempt {attempt}");
            app.status_color = AMBER;
            push_log(
                app,
                "↻",
                AMBER,
                format!("retry {attempt} · {kind:?} · wait {} ms", delay_ms),
                false,
            );
        }
        EventKind::ModelCompleted { response } => {
            discard_provisional(app);
            discard_provisional_reasoning(app);
            app.status = "Running".into();
            app.status_color = AMBER;
            app.input_tokens += response.usage.input_tokens;
            app.output_tokens += response.usage.output_tokens;
            app.cached_tokens += response.usage.cached_input_tokens.unwrap_or_default();
            let body = response.message.content.trim();
            if let Some(reasoning) = response.message.reasoning.as_ref() {
                if !reasoning.text.trim().is_empty() {
                    push_log(app, "▸", VIOLET, reasoning.text.clone(), false);
                    if let Some(item) = app.timeline.back_mut() {
                        item.italic = true;
                    }
                }
            }
            if !body.is_empty() {
                push_log(app, "│", IVORY, body, false);
            }
            for call in &response.message.tool_calls {
                push_log(
                    app,
                    "├",
                    BLUE,
                    format!("{} · {}", call.name, call.arguments.to_string()),
                    false,
                );
            }
        }
        EventKind::ToolStarted {
            name, arguments, ..
        } => push_log(
            app,
            "├",
            BLUE,
            format!("running {name} · {arguments}"),
            false,
        ),
        EventKind::ToolCompleted {
            name,
            status,
            content_head,
            ..
        } => {
            let (marker, color, outcome) = match status {
                loom_core::kernel::transcript::ToolStatus::Success => {
                    app.tools_ok += 1;
                    ("✓", SAGE, "ok")
                }
                loom_core::kernel::transcript::ToolStatus::Error(_) => {
                    app.tools_failed += 1;
                    ("×", CORAL, "failed")
                }
                loom_core::kernel::transcript::ToolStatus::Cancelled { started } => {
                    app.tools_cancelled += 1;
                    ("△", AMBER, if *started { "interrupted" } else { "not run" })
                }
                _ => ("·", DIM, "finished"),
            };
            let detail = content_head.lines().next().unwrap_or_default();
            push_log(
                app,
                marker,
                color,
                format!("{name} · {outcome}  {detail}"),
                false,
            );
        }
        EventKind::SteeringApplied { messages, .. } => push_log(
            app,
            "›",
            AMBER,
            format!("applied {messages} steering message(s)"),
            false,
        ),
        EventKind::TaskCompleted => {
            app.status = "Completed".into();
            app.status_color = SAGE;
            push_log(app, "✓", SAGE, "Task completed", false);
        }
        EventKind::TaskFailed { reason } => {
            app.status = "Failed".into();
            app.status_color = CORAL;
            push_log(app, "×", CORAL, format!("Task failed · {reason}"), false);
        }
        EventKind::TaskCancelled => {
            app.status = "Cancelled".into();
            app.status_color = AMBER;
            push_log(app, "⊘", AMBER, "Task cancelled", false);
        }
        EventKind::TaskPaused => {
            app.status = "Paused".into();
            app.status_color = AMBER;
            push_log(app, "Ⅱ", AMBER, "Paused at turn boundary", false);
        }
        EventKind::HarnessNotice { category, detail } => {
            push_log(app, "!", AMBER, format!("{category} · {detail}"), false)
        }
        _ => {}
    }
}

fn push_log(
    app: &mut App,
    marker: &'static str,
    color: Color,
    text: impl Into<String>,
    provisional: bool,
) {
    app.timeline.push_back(LogItem {
        marker,
        color,
        text: text.into(),
        provisional,
        italic: false,
    });
    while app.timeline.len() > EVENT_LOG_LIMIT {
        app.timeline.pop_front();
        app.provisional_index = app.provisional_index.and_then(|index| index.checked_sub(1));
        app.provisional_reasoning_index = app
            .provisional_reasoning_index
            .and_then(|index| index.checked_sub(1));
    }
}

fn discard_provisional_reasoning(app: &mut App) {
    if let Some(index) = app.provisional_reasoning_index.take() {
        if index < app.timeline.len() {
            app.timeline.remove(index);
            if let Some(text_index) = app.provisional_index {
                app.provisional_index = text_index.checked_sub(usize::from(text_index > index));
            }
        }
    }
}

fn discard_provisional(app: &mut App) {
    let mut removed_index = None;
    if let Some(index) = app.provisional_index.take() {
        if index < app.timeline.len() {
            app.timeline.remove(index);
            removed_index = Some(index);
        }
    }
    if let Some(removed) = removed_index {
        if let Some(reasoning_index) = app.provisional_reasoning_index {
            app.provisional_reasoning_index =
                reasoning_index.checked_sub(usize::from(reasoning_index > removed));
        }
    }
}

fn event_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TaskStarted => "TaskStarted",
        EventKind::TurnStarted { .. } => "TurnStarted",
        EventKind::ModelRequestStarted { .. } => "ModelRequestStarted",
        EventKind::TextDelta { .. } => "TextDelta",
        EventKind::ReasoningDelta { .. } => "ReasoningDelta",
        EventKind::ModelRetry { .. } => "ModelRetry",
        EventKind::ModelCompleted { .. } => "ModelCompleted",
        EventKind::ToolStarted { .. } => "ToolStarted",
        EventKind::ToolCompleted { .. } => "ToolCompleted",
        EventKind::TaskCompleted => "TaskCompleted",
        EventKind::TaskFailed { .. } => "TaskFailed",
        EventKind::TaskCancelled => "TaskCancelled",
        _ => "Event",
    }
}

fn handle_key(code: KeyCode, app: &mut App, control: &TaskControl) {
    if app.editing {
        match code {
            KeyCode::Esc => {
                app.editing = false;
                app.input.clear();
            }
            KeyCode::Enter => {
                if !app.input.trim().is_empty() {
                    control.steer(std::mem::take(&mut app.input));
                    app.status = "Steering queued".into();
                    app.status_color = AMBER;
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
        KeyCode::Char('q') | KeyCode::Esc => app.quit_after_finish = true,
        _ => {}
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    if area.width < 44 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("Terminal too small · resize to at least 44×12")
                .style(Style::default().fg(AMBER).bg(BG)),
            area,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);
    draw_header(frame, rows[0], app);

    let show_sidebar = area.width >= 108;
    let (timeline_area, sidebar_area) = if show_sidebar {
        let columns =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(36)]).split(rows[1]);
        (columns[0], Some(columns[1]))
    } else {
        (rows[1], None)
    };
    draw_timeline(frame, timeline_area, app);
    if let Some(sidebar) = sidebar_area {
        draw_sidebar(frame, sidebar, app);
    }
    draw_composer(frame, rows[2], app);
    draw_help(frame, rows[3], app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let session = app
        .session
        .map(|id| id.to_string())
        .unwrap_or_else(|| "opening session".into());
    let left = Line::from(vec![
        Span::styled(
            " ◆ Loom ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("· {session}"), Style::default().fg(DIM)),
    ]);
    let right = Line::from(vec![
        Span::styled("● ", Style::default().fg(app.status_color)),
        Span::styled(
            &app.status,
            Style::default()
                .fg(app.status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", app.model), Style::default().fg(BLUE)),
    ]);
    let right_width = (right.width() as u16 + 1).min(area.width.saturating_sub(16));
    let split =
        Layout::horizontal([Constraint::Min(16), Constraint::Length(right_width)]).split(area);
    frame.render_widget(
        Paragraph::new(left).style(Style::default().bg(BG)),
        split[0],
    );
    frame.render_widget(
        Paragraph::new(right).alignment(ratatui::layout::Alignment::Right),
        split[1],
    );
}

fn draw_timeline(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let task = Line::from(vec![
        Span::styled(
            "› ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            &app.task,
            Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut lines = vec![task, Line::from("")];
    for item in &app.timeline {
        let marker_style = Style::default().fg(item.color).add_modifier(Modifier::BOLD);
        let text_style = if item.provisional {
            Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC)
        } else {
            let style = Style::default().fg(if item.italic { VIOLET } else { IVORY });
            if item.italic {
                style.add_modifier(Modifier::ITALIC)
            } else {
                style
            }
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", item.marker), marker_style),
            Span::styled(item.text.as_str(), text_style),
        ]));
    }
    if app.reasoning_chars > 0 && !app.finished {
        lines.push(Line::from(Span::styled(
            format!("  ▸ reasoning · {} chars", app.reasoning_chars),
            Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC),
        )));
    }
    let visible = area.height.saturating_sub(1) as usize;
    let scroll = lines.len().saturating_sub(visible).min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let blocks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Min(5),
    ])
    .split(area);
    let session = app
        .session
        .map(|id| id.to_string())
        .unwrap_or_else(|| "waiting".into());
    panel(
        frame,
        blocks[0],
        "TASK",
        vec![
            styled_line("● ", "RUN", app.status_color),
            styled_line("id ", &session, DIM),
        ],
    );
    panel(
        frame,
        blocks[1],
        "RUN SO FAR",
        vec![
            metric("turns", app.turns.to_string(), AMBER),
            metric("requests", app.requests.to_string(), BLUE),
            metric("retries", app.retries.to_string(), AMBER),
            metric(
                "tools",
                format!(
                    "{} ok · {} err · {} stop",
                    app.tools_ok, app.tools_failed, app.tools_cancelled
                ),
                SAGE,
            ),
        ],
    );
    panel(
        frame,
        blocks[2],
        "TOKENS",
        vec![
            metric("input", compact(app.input_tokens), IVORY),
            metric("cached", compact(app.cached_tokens), TEAL),
            metric("output", compact(app.output_tokens), IVORY),
            metric("events", app.events_seen.to_string(), DIM),
        ],
    );
    let recent = if app.recent_events.is_empty() {
        vec![Line::from(Span::styled(
            "waiting for events…",
            Style::default().fg(DIM),
        ))]
    } else {
        app.recent_events
            .iter()
            .map(|event| Line::from(Span::styled(event.clone(), Style::default().fg(DIM))))
            .collect()
    };
    panel(frame, blocks[3], "EVENTS", recent);
}

fn panel(frame: &mut ratatui::Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(Span::styled(title, Style::default().fg(DIM)))
                    .borders(Borders::LEFT | Borders::TOP)
                    .border_style(Style::default().fg(BORDER))
                    .style(Style::default().bg(PANEL)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn styled_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(label.to_owned(), Style::default().fg(DIM)),
        Span::styled(
            value.to_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn metric(label: &str, value: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(DIM)),
        Span::styled(value, Style::default().fg(color)),
    ])
}

fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let (title, text, color) = if app.editing {
        (
            "STEER · ENTER SENDS · ESC CANCELS",
            app.input.as_str(),
            AMBER,
        )
    } else if app.finished {
        ("TASK COMPLETE", "Press q to exit", app.status_color)
    } else {
        ("INPUT", "Press i to queue a follow-up", DIM)
    };
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(color).bg(PANEL))
            .block(
                Block::default()
                    .title(Span::styled(title, Style::default().fg(color)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if app.editing { AMBER } else { BORDER }))
                    .style(Style::default().bg(PANEL)),
            ),
        area,
    );
}

fn draw_help(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let hint = if app.finished {
        " q quit   ·   Esc close"
    } else {
        " i steer   ·   p pause   ·   Esc cancel   ·   q cancel & quit"
    };
    let footer = Line::from(vec![
        Span::styled(hint, Style::default().fg(DIM)),
        Span::raw("   "),
        Span::styled(
            format!(
                "events {} · dropped {}",
                app.events_seen, app.events_dropped
            ),
            Style::default().fg(if app.events_dropped == 0 { DIM } else { CORAL }),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(ratatui::layout::Alignment::Center),
        area,
    );
}
