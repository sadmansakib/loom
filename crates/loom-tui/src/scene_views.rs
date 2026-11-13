use std::env;
use std::time::Duration;

use loom_core::kernel::{session::TaskStatus, transcript::Item};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    PickerState,
    glyphs::{self, Glyph, GlyphSet},
    reducer::Reducer,
    theme,
};

/// Data-free preview routes. They render the same shell without fabricating task state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Empty main task view.
    Main,
    /// Empty cancellation-state view.
    Cancel,
    /// Empty retry-state view.
    Retry,
    /// Empty completion-state view.
    Complete,
    /// Empty model picker.
    Picker,
    /// Keyboard and event mapping guide.
    Help,
}

/// Render a `--scene` route without inventing task history or runtime metrics.
pub fn draw(frame: &mut Frame<'_>, scene: Scene) {
    match scene {
        Scene::Main | Scene::Cancel | Scene::Retry | Scene::Complete => {
            draw_live(
                frame,
                &Reducer::default(),
                "",
                false,
                None,
                None,
                Some(scene),
            );
        }
        Scene::Help => draw_help(frame),
        Scene::Picker => draw_empty_picker(frame),
    }
}

pub fn draw_live(
    frame: &mut Frame<'_>,
    reducer: &Reducer,
    input: &str,
    sidebar_visible: bool,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
    scene: Option<Scene>,
) {
    let area = design_area(frame.area());
    let compact = area.width < 90;
    let wide = area.width >= 120;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);
    draw_live_header(frame, rows[0], reducer, scene);
    let footer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(1)])
        .split(rows[2]);
    let show_sidebar = wide || (area.width >= 90 && sidebar_visible);
    let body = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(65), Constraint::Length(38)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(rows[1])
    };
    draw_reducer_thread(frame, body[0], reducer, compact, scene);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", fg(theme::AMBER)),
            Span::styled(input, fg(theme::IVORY)),
            Span::styled(
                format!("    queued {}", u8::from(reducer.follow_up_queued)),
                fg(theme::DIM),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(fg(theme::BORDER)),
        ),
        footer[0],
    );
    let keys = if reducer.task.status == Some(TaskStatus::Running) {
        if area.width >= 90 && area.width < 120 {
            "Esc cancel  ·  Enter queue follow-up  ·  Tab sidebar  ·  ? help"
        } else {
            "Esc cancel  ·  Enter queue follow-up  ·  ? help"
        }
    } else if area.width >= 90 && area.width < 120 {
        "Enter send task  ·  m model  ·  Tab sidebar  ·  ? help  ·  q quit"
    } else {
        "Enter send task  ·  m model  ·  ? help  ·  q quit"
    };
    frame.render_widget(Paragraph::new(keys).style(fg(theme::DIM)), footer[1]);
    if wide {
        draw_live_sidebar(frame, body[1], reducer, elapsed, dropped, scene);
    } else if show_sidebar {
        let overlay = Rect::new(
            area.x + area.width.saturating_sub(42),
            rows[1].y,
            area.width.min(42),
            rows[1].height.min(12),
        );
        draw_live_sidebar(frame, overlay, reducer, elapsed, dropped, scene);
    }
}

pub fn draw_picker(frame: &mut Frame<'_>, picker: &PickerState) {
    let area = design_area(frame.area());
    let wide = area.width >= 120;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);
    draw_header(frame, rows[0], "model picker");
    let columns = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(65), Constraint::Length(38)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(rows[1])
    };
    let mut lines = vec![heading("AVAILABLE MODELS")];
    for (index, choice) in picker.choices.iter().enumerate() {
        let selected = index == picker.selected;
        let capability = choice.capabilities.map_or_else(
            || "capabilities not reported".to_owned(),
            |cap| {
                format!(
                    "{} · {} context",
                    if cap.tool_calls { "tools" } else { "text only" },
                    cap.max_context_tokens
                )
            },
        );
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", if selected { ">" } else { " " }),
                fg(theme::AMBER),
            ),
            Span::styled(
                format!("{} · {}", choice.provider, choice.model),
                if selected {
                    theme::SELECTED
                } else {
                    fg(theme::IVORY)
                },
            ),
            Span::styled(format!("  {capability}"), fg(theme::DIM)),
        ]));
    }
    if picker.choices.is_empty() {
        lines.push(muted(
            picker.notice.as_deref().unwrap_or("No models discovered."),
        ));
    }
    let effort_text = picker
        .choices
        .get(picker.selected)
        .filter(|choice| !choice.efforts.is_empty())
        .map_or_else(
            || "not reported".to_owned(),
            |choice| {
                choice
                    .efforts
                    .iter()
                    .map(|effort| format!("{effort:?}").to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" · ")
            },
        );
    lines.push(Line::from(vec![
        Span::styled("REPORTED EFFORTS  ", fg(theme::DIM)),
        Span::styled(effort_text, fg(theme::IVORY)),
    ]));
    render_panel(frame, columns[0], "model · capabilities", lines);
    if wide {
        let selected = picker.choices.get(picker.selected);
        let detail = selected.map_or_else(
            || vec![muted("selection unavailable")],
            |choice| {
                vec![
                    heading("SELECTION"),
                    body(&format!("provider  {}", choice.provider)),
                    body(&format!("model     {}", choice.model)),
                    muted(""),
                    heading("CAPABILITIES"),
                    muted(&choice.capabilities.map_or_else(
                        || "not reported".to_owned(),
                        |cap| {
                            format!(
                                "context {} · tools {}",
                                cap.max_context_tokens, cap.tool_calls
                            )
                        },
                    )),
                ]
            },
        );
        render_panel(frame, columns[1], "selected", detail);
    }
    frame.render_widget(
        Paragraph::new(
            "↑/↓ choose model · Tab cycle reported effort · Enter apply · Esc back · r refresh",
        )
        .style(fg(theme::DIM)),
        rows[2],
    );
}

fn draw_empty_picker(frame: &mut Frame<'_>) {
    let area = design_area(frame.area());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    draw_header(frame, rows[0], "model picker");
    render_panel(
        frame,
        rows[1],
        "discovery",
        vec![muted(
            "Model discovery has not run in a static scene preview.",
        )],
    );
}

fn draw_help(frame: &mut Frame<'_>) {
    let area = design_area(frame.area());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    draw_header(frame, rows[0], "keys · events · crate seam");
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(54), Constraint::Min(1)])
        .split(rows[1]);
    let left = vec![
        heading("WHILE A TASK RUNS"),
        help_key("Esc / Ctrl-C", "cancel current task"),
        help_key("Enter", "queue follow-up for next turn"),
        help_key("Tab", "toggle sidebar (medium width)"),
        help_key("?", "close help"),
        help_key("q", "cancel task and quit"),
        heading("WHEN IDLE"),
        help_key("Enter", "send task / start next task"),
        help_key("m", "open model picker"),
        help_key("q / Esc", "quit"),
        heading("IN MODEL PICKER"),
        help_key("↑ / ↓, j / k", "move through discovered models"),
        help_key("Tab", "cycle reported effort levels"),
        help_key("r", "refresh model discovery"),
        help_key("Enter / Esc", "apply / return"),
        heading("DESIGN REFERENCE"),
        muted("Static --scene views show no task data; live values come from events and session."),
    ];
    render_panel(frame, columns[0], "controls", left);
    let event_lines = [
        ("TaskStarted", "status becomes Running"),
        ("TurnStarted", "begin a task turn"),
        ("ModelRequestStarted", "set request attempt"),
        ("TextDelta / ReasoningDelta", "provisional only"),
        ("ModelRetry", "discard provisional output"),
        ("ModelCompleted", "commit authoritative response"),
        (
            "ToolStarted / ToolCompleted",
            "update call state and output head",
        ),
        ("TaskCompleted / Failed / Cancelled", "set terminal status"),
    ];
    let mut right = vec![heading("EVENT TO WIDGET")];
    right.extend(event_lines.into_iter().map(|(event, meaning)| {
        Line::from(vec![
            Span::styled(
                format!("{event:<34}"),
                fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(meaning, fg(theme::IVORY)),
        ])
    }));
    right.push(muted(""));
    right.push(heading("CRATE SEAM"));
    right.push(body(
        "loom-cli → Engine::run_task → EventSink → channel → reducer → Ratatui",
    ));
    right.push(muted(
        "Session.transcript is authoritative; events are observations.",
    ));
    render_panel(frame, columns[1], "event stream", right);
}

fn draw_live_header(frame: &mut Frame<'_>, area: Rect, reducer: &Reducer, scene: Option<Scene>) {
    let model = if scene.is_some() {
        "model not available in static preview"
    } else {
        reducer
            .observed_model
            .as_deref()
            .unwrap_or("model not reported")
    };
    let task_id = reducer.task.task_id.map_or_else(
        || "task id unavailable".to_owned(),
        |id| format!("task {id}"),
    );
    let state = if scene.is_some() {
        "preview · no live data"
    } else {
        status_label(reducer)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {task_id} · {state}"), fg(theme::DIM)),
            Span::styled(format!("    {model}"), fg(theme::BLUE)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(fg(theme::BORDER)),
        ),
        area,
    );
}

fn draw_reducer_thread(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    compact: bool,
    scene: Option<Scene>,
) {
    let mut lines = Vec::new();
    if let Some(task_id) = reducer.task.task_id {
        lines.push(Line::from(vec![
            Span::styled("●", theme::THREAD_STYLE),
            Span::styled(format!(" task {task_id}"), fg(theme::DIM)),
        ]));
    }
    if let Some(reason) = &reducer.task.failure_reason {
        lines.push(tinted(&format!("Task failed · {reason}"), theme::CORAL));
    }
    for item in &reducer.committed_transcript {
        match item {
            Item::User { text } => lines.push(Line::from(vec![
                Span::styled("› ", fg(theme::AMBER)),
                Span::styled(text.as_str(), fg(theme::IVORY).add_modifier(Modifier::BOLD)),
            ])),
            Item::Assistant(message) => {
                if let Some(reasoning) = &message.reasoning {
                    lines.push(Line::from(Span::styled(
                        reasoning.text.as_str(),
                        fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                    )));
                }
                lines.push(body(&message.content));
            }
            Item::ToolResult(result) => lines.push(Line::from(vec![
                Span::styled("tool  ", fg(theme::TEAL).add_modifier(Modifier::BOLD)),
                Span::styled(result.call_id.as_str(), fg(theme::IVORY)),
                Span::styled(format!("  {}", result.content), fg(theme::DIM)),
            ])),
            _ => {}
        }
    }
    for retry in &reducer.retries {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} attempt {}",
                    glyphs::glyph(Glyph::Retry, glyph_set()),
                    retry.attempt
                ),
                fg(theme::CORAL),
            ),
            Span::styled(
                format!(" · {} · {} ms · discarded", retry.kind, retry.delay_ms),
                fg(theme::DIM),
            ),
        ]));
    }
    if !reducer.provisional.reasoning.is_empty() {
        lines.push(Line::from(Span::styled(
            reducer.provisional.reasoning.as_str(),
            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
        )));
    }
    if !reducer.provisional.text.is_empty() {
        lines.push(tinted(&reducer.provisional.text, theme::AMBER));
    }
    for tool in reducer.tools.values() {
        let (marker, color, state) = match tool.status {
            Some(loom_core::kernel::transcript::ToolStatus::Success) => {
                (Glyph::Check, theme::SAGE, "completed")
            }
            Some(loom_core::kernel::transcript::ToolStatus::Error(_)) => {
                (Glyph::Cross, theme::CORAL, "failed")
            }
            Some(loom_core::kernel::transcript::ToolStatus::Cancelled { started: true }) => (
                Glyph::Warning,
                theme::AMBER,
                "interrupted; side effects possible",
            ),
            Some(loom_core::kernel::transcript::ToolStatus::Cancelled { started: false }) => {
                (Glyph::Pending, theme::DIM, "not run")
            }
            Some(_) => (Glyph::Warning, theme::DIM, "finished"),
            None => (Glyph::Spinner, theme::AMBER, "running"),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", glyphs::glyph(marker, glyph_set())),
                fg(color),
            ),
            Span::styled(
                tool.name.as_str(),
                fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {state}"), fg(color)),
            Span::styled(
                tool.content_head
                    .as_deref()
                    .map_or_else(String::new, |head| format!(" · {head}")),
                fg(theme::DIM),
            ),
        ]));
    }
    if lines.is_empty() {
        let message = match scene {
            Some(Scene::Main) => "No live task data in this preview.",
            Some(Scene::Cancel) => "No cancellation data: this preview has no live task.",
            Some(Scene::Retry) => "No retry data: this preview has no live task.",
            Some(Scene::Complete) => "No completion data: this preview has no live task.",
            _ => "No task transcript yet. Enter a task to start.",
        };
        lines.push(muted(message));
    }
    if !compact && reducer.follow_up_queued {
        lines.push(tinted("follow-up queued for next turn", theme::AMBER));
    }
    render_panel(frame, area, "task thread", lines);
}

fn draw_live_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
    scene: Option<Scene>,
) {
    let reported = |value: Option<u64>| {
        if reducer.usage_reported {
            value.map_or_else(|| "not reported".to_owned(), |n| n.to_string())
        } else {
            "not reported".to_owned()
        }
    };
    let cost = reducer
        .usage
        .cost_usd
        .map_or_else(|| "not reported".to_owned(), |value| format!("${value:.4}"));
    let elapsed = elapsed.map_or_else(
        || "not available".to_owned(),
        |value| format!("{:02}:{:02}", value.as_secs() / 60, value.as_secs() % 60),
    );
    let mut lines = if scene.is_some() {
        vec![
            heading("TASK"),
            muted("no task status in static preview"),
            muted("model not available in static preview"),
            heading("LIVE USAGE · UNAVAILABLE"),
            muted("not available in static preview"),
            muted("not available in static preview"),
            muted("not available in static preview"),
            muted("not available in static preview"),
            muted("elapsed not available in static preview"),
            muted("tools not available in static preview"),
            muted(""),
        ]
    } else {
        vec![
            heading("TASK"),
            body(&format!(
                "{} · turn {} · attempt {}",
                status_label(reducer),
                reducer
                    .turn
                    .map_or_else(|| "not reported".to_owned(), |n| n.to_string()),
                reducer
                    .attempt
                    .map_or_else(|| "not reported".to_owned(), |n| n.to_string())
            )),
            muted(&format!(
                "model  {}",
                reducer.observed_model.as_deref().unwrap_or("not reported")
            )),
            muted(""),
        ]
    };
    if let Some(summary) = scene
        .is_none()
        .then_some(reducer.summary.as_ref())
        .flatten()
    {
        lines.extend([
            heading("COMPLETED RUN RECORD"),
            muted(&format!(
                "turns {} · model requests {}",
                summary.turns, summary.model_requests
            )),
            muted(&format!(
                "tools {} succeeded · {} failed",
                summary.successful_tool_calls, summary.failed_tool_calls
            )),
            muted(&format!(
                "wall time {}s · dropped events {}",
                summary.wall_time.as_secs(),
                summary.events_dropped
            )),
            if summary.model_requests > 0 {
                muted(&format!(
                    "final request census {} tokens",
                    summary.last_context_census.total
                ))
            } else {
                muted("final request census not available; no request compiled")
            },
            muted(&format!(
                "input budget {} · output {}",
                optional_budget(summary.remaining_budget.input_tokens),
                optional_budget(summary.remaining_budget.output_tokens)
            )),
            muted(&format!(
                "total budget {} · tools {}",
                optional_budget(summary.remaining_budget.total_tokens),
                summary
                    .remaining_budget
                    .tool_calls
                    .map_or_else(|| "unbounded".to_owned(), |value| value.to_string())
            )),
            muted(""),
        ]);
    }
    lines.extend([
        heading("USAGE"),
        muted(&format!(
            "input   {}",
            reported(Some(reducer.usage.input_tokens))
        )),
        muted(&format!(
            "cached  {}",
            reported(reducer.usage.cached_input_tokens)
        )),
        muted(&format!(
            "output  {}",
            reported(Some(reducer.usage.output_tokens))
        )),
        muted(&format!("cost    {cost}")),
        muted(&format!("elapsed {elapsed}")),
        muted(&format!(
            "tools   {}",
            if scene.is_some() {
                "not available".to_owned()
            } else {
                format!("{} calls observed", reducer.tools.len())
            }
        )),
        muted(""),
    ]);
    match scene {
        Some(Scene::Cancel) => {
            lines.push(heading("CANCELLATION LEDGER"));
            lines.push(muted("No cancellation event in this preview."));
            lines.push(muted("Call outcomes are not available."));
        }
        Some(Scene::Retry) => {
            lines.push(heading("RETRIES"));
            lines.push(muted("No retry events in this preview."));
            lines.push(muted("Retry policy is not exposed by the event stream."));
        }
        Some(Scene::Complete) => {
            lines.push(heading("RUN RECORD"));
            lines.push(muted("No completed task in this preview."));
        }
        _ => {}
    }
    lines.push(muted(""));
    lines.push(heading("TASK BUDGET · REMAINING"));
    if let Some(progress) = scene
        .is_none()
        .then_some(reducer.context_progress)
        .flatten()
    {
        let budget = progress.remaining_budget;
        lines.extend([
            budget_line("input tokens", budget.input_tokens),
            budget_line("output tokens", budget.output_tokens),
            budget_line("total tokens", budget.total_tokens),
            budget_line("tool calls", budget.tool_calls.map(u64::from)),
            muted(&format!(
                "wall time  {}",
                budget.wall_time.map_or_else(
                    || "unbounded".to_owned(),
                    |remaining| format!("{}s remaining", remaining.as_secs())
                )
            )),
        ]);
    } else if scene.is_some() {
        lines.push(muted("not available in static preview"));
    } else {
        lines.push(muted("not reported before request compilation"));
    }
    lines.push(heading("REQUEST CONTEXT · LATEST COMPILE"));
    if let Some(progress) = scene
        .is_none()
        .then_some(reducer.context_progress)
        .flatten()
    {
        let census = progress.census;
        lines.extend([
            muted(&format!(
                "turn {} · estimated {}/{} input tokens",
                progress.turn, census.total, progress.context_budget.max_input_tokens
            )),
            muted(&format!(
                "output reserve {} tokens",
                progress.context_budget.reserved_output_tokens
            )),
            census_line("system", census.system_prompt),
            census_line("repository facts", census.repo_facts),
            census_line("user items", census.user_items),
            census_line("assistant history", census.assistant_history),
            census_line("reasoning replay", census.reasoning_replay),
            census_line("tool results", census.tool_results),
            census_line("tool specs", census.tool_specs),
        ]);
    } else if scene.is_some() {
        lines.push(muted("not available in static preview"));
    } else {
        lines.push(muted("not reported before request compilation"));
    }
    lines.push(heading("EVENTS"));
    lines.extend(reducer.recent_events.iter().map(|event| muted(event)));
    lines.push(match (scene, dropped) {
        (Some(_), _) => muted("events unavailable in preview"),
        (None, Some(dropped)) => tinted(
            &format!("events {} · dropped {dropped}", reducer.event_count),
            if dropped == 0 {
                theme::DIM
            } else {
                theme::CORAL
            },
        ),
        (None, None) => muted("event counters not available"),
    });
    render_panel(frame, area, "run state", lines);
}

fn render_panel<'a>(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'a>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" {title} "))
                    .borders(Borders::ALL)
                    .border_style(fg(if title == "task thread" {
                        theme::THREAD
                    } else {
                        theme::BORDER
                    })),
            )
            .style(Style::default().bg(theme::BG))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, title: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  /  {title}"),
                fg(theme::IVORY).add_modifier(Modifier::BOLD),
            ),
            Span::styled("    one thread per task", fg(theme::DIM)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(fg(theme::BORDER)),
        ),
        area,
    );
}

fn help_key(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), fg(theme::AMBER)),
        Span::styled(description, fg(theme::IVORY)),
    ])
}

fn design_area(area: Rect) -> Rect {
    let width = area.width.min(154);
    let height = area.height.min(40);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        fg(theme::AMBER).add_modifier(Modifier::BOLD),
    ))
}
fn body(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), fg(theme::IVORY)))
}
fn muted(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), fg(theme::DIM)))
}
fn tinted(text: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), fg(color)))
}
fn budget_line(label: &str, value: Option<u64>) -> Line<'static> {
    muted(&format!("{label}  {}", optional_budget(value)))
}
fn census_line(label: &str, value: u32) -> Line<'static> {
    muted(&format!("{label}  {value} tokens · compiler estimate"))
}
fn optional_budget(value: Option<u64>) -> String {
    value.map_or_else(
        || "unbounded".to_owned(),
        |remaining| format!("{remaining} remaining"),
    )
}
fn status_label(reducer: &Reducer) -> &'static str {
    match reducer.task.status {
        Some(TaskStatus::Running) => "Running",
        Some(TaskStatus::Completed) => "Completed",
        Some(TaskStatus::Cancelled) => "Cancelled",
        Some(TaskStatus::Failed) => "Failed",
        Some(TaskStatus::Paused) => "Paused",
        _ => "Idle",
    }
}
fn fg(color: Color) -> Style {
    Style::default()
        .fg(adapt_color(color))
        .bg(adapt_color(theme::BG))
}
fn glyph_set() -> GlyphSet {
    if env::var_os("LOOM_ASCII").is_some_and(|value| value == "1") {
        GlyphSet::Ascii
    } else {
        GlyphSet::Unicode
    }
}
fn adapt_color(color: Color) -> Color {
    if env::var_os("NO_COLOR").is_some() {
        Color::Reset
    } else {
        color
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::kernel::{
        agent::RemainingBudget,
        context::{ContextBudget, ContextCensus},
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn render(scene: Scene) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal.draw(|frame| draw(frame, scene)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn six_static_scenes_snapshot_text_and_styles_without_runtime_data() {
        use std::{fmt::Write as _, fs, path::PathBuf};

        let scenes = [
            ("main", Scene::Main),
            ("cancel", Scene::Cancel),
            ("retry", Scene::Retry),
            ("complete", Scene::Complete),
            ("picker", Scene::Picker),
            ("help", Scene::Help),
        ];
        let mut actual = String::new();
        for (name, scene) in scenes {
            let buffer = render(scene);
            let _ = writeln!(actual, "=== {name} ===");
            actual.push_str(&snapshot(&buffer));
        }

        if env::var_os("UPDATE_TUI_SNAPSHOTS").is_some() {
            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/scenes.snap");
            fs::write(path, &actual).unwrap();
        }
        assert_eq!(actual, include_str!("../tests/snapshots/scenes.snap"));
    }

    fn snapshot(buffer: &ratatui::buffer::Buffer) -> String {
        let mut output = String::new();
        for y in 0..buffer.area.height {
            let mut row = Vec::<(String, String)>::new();
            for x in 0..buffer.area.width {
                let cell = buffer.cell((x, y)).unwrap();
                let style = format!("{:?}/{:?}/{:?}", cell.fg, cell.bg, cell.modifier);
                if let Some((text, current_style)) = row.last_mut()
                    && *current_style == style
                {
                    text.push_str(cell.symbol());
                    continue;
                }
                row.push((cell.symbol().to_owned(), style));
            }
            while row.last().is_some_and(|(text, style)| {
                text.chars().all(char::is_whitespace) && style == "Reset/Reset/empty"
            }) {
                row.pop();
            }
            if row.is_empty() {
                output.push('\n');
                continue;
            }
            for (text, style) in row {
                use std::fmt::Write as _;
                let _ = write!(output, "[{style}]{text}");
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn static_task_scenes_never_render_synthetic_run_values() {
        for scene in [Scene::Main, Scene::Cancel, Scene::Retry, Scene::Complete] {
            let rendered = render(scene)
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("not available in static preview"));
            match scene {
                Scene::Cancel => assert!(rendered.contains("No cancellation event")),
                Scene::Retry => assert!(rendered.contains("No retry events")),
                Scene::Complete => assert!(rendered.contains("No completed task")),
                Scene::Main | Scene::Picker | Scene::Help => {}
            }
        }
    }

    #[test]
    fn live_renderer_reflects_reducer_state_and_keeps_missing_usage_unknown() {
        let mut reducer = Reducer::default();
        reducer.begin_task("live task provided by user");
        reducer.observed_model = Some("model-from-event".into());
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", false, None, Some(0), None))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("live task provided by user"));
        assert!(rendered.contains("model-from-event"));
        assert!(rendered.contains("not reported"));
    }

    #[test]
    fn live_context_and_remaining_budget_render_authoritative_event_values() {
        let mut reducer = Reducer::default();
        let context_budget = ContextBudget {
            max_input_tokens: 12_000,
            reserved_output_tokens: 2_000,
        };
        let census: ContextCensus = serde_json::from_value(serde_json::json!({
            "system_prompt": 120,
            "repo_facts": 30,
            "user_items": 40,
            "assistant_history": 300,
            "reasoning_replay": 50,
            "tool_results": 60,
            "tool_specs": 70,
            "total": 670
        }))
        .unwrap();
        let remaining_budget: RemainingBudget = serde_json::from_value(serde_json::json!({
            "input_tokens": 800,
            "output_tokens": 500,
            "total_tokens": 1300,
            "tool_calls": 2,
            "wall_time": null
        }))
        .unwrap();
        reducer.context_progress = Some(crate::reducer::ContextProgress {
            turn: 3,
            context_budget,
            census,
            remaining_budget,
        });
        let mut terminal = Terminal::new(TestBackend::new(154, 60)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", false, None, Some(0), None))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("800 remaining"));
        assert!(text.contains("unbounded"));
        assert!(text.contains("estimated 670/"));
        assert!(text.contains("repository facts"));
    }

    #[test]
    fn unbounded_context_and_budget_are_not_rendered_as_zero() {
        let mut reducer = Reducer::default();
        reducer.context_progress = Some(crate::reducer::ContextProgress {
            turn: 1,
            context_budget: ContextBudget {
                max_input_tokens: 20_000,
                reserved_output_tokens: 4_000,
            },
            census: ContextCensus::default(),
            remaining_budget: RemainingBudget::default(),
        });
        let mut terminal = Terminal::new(TestBackend::new(154, 60)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", false, None, Some(0), None))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("unbounded"));
        assert!(rendered.contains("estimated 0/20000"));
        assert!(!rendered.contains("0 remaining"));
    }

    #[test]
    fn completed_summary_is_rendered_from_engine_record() {
        let mut reducer = Reducer::default();
        let task_id = loom_core::TaskId::new();
        let mut summary = loom_core::TaskSummary::empty(task_id);
        summary.turns = 4;
        summary.model_requests = 5;
        summary.input_tokens = 600;
        summary.output_tokens = 80;
        summary.successful_tool_calls = 3;
        summary.failed_tool_calls = 1;
        summary.wall_time = Duration::from_secs(12);
        summary.events_dropped = 2;
        summary.last_context_census.total = 28;
        summary.remaining_budget.input_tokens = Some(0);
        summary.remaining_budget.total_tokens = Some(9);
        reducer.set_summary(summary);

        let mut terminal = Terminal::new(TestBackend::new(154, 60)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", false, None, Some(2), None))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("COMPLETED RUN RECORD"));
        assert!(rendered.contains("turns 4 · model requests 5"));
        assert!(rendered.contains("tools 3 succeeded · 1 failed"));
        assert!(rendered.contains("wall time 12s · dropped events 2"));
        assert!(rendered.contains("final request census 28 tokens"));
        assert!(rendered.contains("input budget 0 remaining"));
        assert!(rendered.contains("unbounded"));
    }

    #[test]
    fn static_previews_never_imply_a_ready_task_or_live_counters() {
        for scene in [Scene::Main, Scene::Cancel, Scene::Retry, Scene::Complete] {
            let buffer = render(scene);
            let rendered = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("preview · no live data"));
            assert!(!rendered.contains("· Ready"));
            assert!(!rendered.contains("events 0"));
            assert!(rendered.contains("tools   not available"));
        }
    }
}
