//! Rendering for the interactive terminal frontend.

use super::{
    AMBER, App, BG, BLUE, BORDER, CORAL, DIM, Duration, IVORY, PANEL, RetryRecord, SAGE, TEAL,
    ToolRecordState, VIOLET,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
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
    let show_inline_sidebar = app.sidebar_visible && area.width >= 120;
    let show_sidebar_overlay = app.sidebar_visible && (90..120).contains(&area.width);
    let (timeline_area, sidebar_area) = if show_inline_sidebar {
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
    if show_sidebar_overlay {
        let overlay = centered_rect(42, rows[1].height.saturating_sub(2), rows[1]);
        frame.render_widget(Clear, overlay);
        draw_sidebar(frame, overlay, app);
    }
    draw_composer(frame, rows[2], app);
    draw_help(frame, rows[3], app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let session = app
        .session
        .map_or_else(|| "opening session".into(), |id| id.to_string());
    let workspace = std::env::current_dir().ok().map_or_else(
        || "workspace".into(),
        |path| {
            let rendered = path.to_string_lossy().into_owned();
            std::env::var("HOME")
                .map(|home| rendered.replacen(&home, "~", 1))
                .unwrap_or(rendered)
        },
    );
    let left = Line::from(vec![
        Span::styled(
            " ◆ Loom ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{workspace} · {}", short_id(&session)),
            Style::default().fg(DIM),
        ),
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
    let right_width = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .saturating_add(1)
        .min(area.width.saturating_sub(16));
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

#[allow(
    clippy::too_many_lines,
    reason = "timeline rows are rendered in one pass to preserve ordering and scroll behavior"
)]
fn draw_timeline(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if app.timeline.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled("Loom is ready", Style::default().fg(IVORY))),
            Line::from(Span::styled(
                "task events will appear here",
                Style::default().fg(DIM),
            )),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(BG)),
        );
        frame.render_widget(empty, area);
        return;
    }
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
    for (index, item) in app.timeline.iter().enumerate() {
        if item.marker == "●" {
            let elapsed = format_duration(app.elapsed);
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(AMBER)),
                Span::styled(item.text.as_str(), Style::default().fg(DIM)),
                Span::styled(" ───────────────────────── ", Style::default().fg(BORDER)),
                Span::styled(
                    format!(
                        "{} · {elapsed}",
                        item.detail.as_deref().unwrap_or("in progress")
                    ),
                    Style::default().fg(DIM),
                ),
            ]));
            continue;
        }
        let marker_style = Style::default().fg(item.color).add_modifier(Modifier::BOLD);
        let text_style = if item.provisional || item.italic {
            Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(IVORY)
        };
        let selected = app.selected_item == Some(index);
        let mut spans = vec![
            Span::styled(
                if item.provisional { "┆ " } else { "│ " },
                Style::default().fg(if item.provisional { DIM } else { BORDER }),
            ),
            Span::styled(format!("{} ", item.marker), marker_style),
            Span::styled(item.text.as_str(), text_style),
        ];
        if let Some(detail) = &item.detail
            && item.expanded
            && item.marker != "●"
        {
            spans.push(Span::styled(
                format!("  {detail}"),
                Style::default().fg(DIM),
            ));
        }
        lines.push(Line::from(spans).style(if selected {
            Style::default().bg(PANEL)
        } else {
            Style::default()
        }));
        if item.italic && !app.show_reasoning {
            lines.pop();
        }
    }
    if app.reasoning_chars > 0 && !app.finished {
        lines.push(Line::from(Span::styled(
            format!("  ▸ reasoning · {} chars", app.reasoning_chars),
            Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC),
        )));
    }
    let visible = usize::from(area.height.saturating_sub(1));
    let scroll = u16::try_from(
        lines
            .len()
            .saturating_sub(visible)
            .min(usize::from(u16::MAX)),
    )
    .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "sidebar sections share one vertical layout and are rendered as a single widget tree"
)]
fn draw_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let blocks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Min(5),
    ])
    .split(area);
    let session = app
        .session
        .map_or_else(|| "waiting".into(), |id| id.to_string());
    panel(
        frame,
        blocks[0],
        if app.finished { "RUN RECORD" } else { "TASK" },
        vec![
            styled_line(
                &format!("{} ", status_glyph(&app.status)),
                &app.status,
                app.status_color,
            ),
            styled_line("id ", &session, DIM),
            styled_line("run ", &format!("{:02}", app.run_id.max(1)), AMBER),
            styled_line(
                "wall ",
                &format_duration(if app.finished {
                    app.wall_time
                } else {
                    app.elapsed
                }),
                DIM,
            ),
        ],
    );
    panel(
        frame,
        blocks[1],
        if app.status.starts_with("Retrying") || !app.retries_log.is_empty() {
            "RETRY HISTORY"
        } else {
            "RUN SO FAR"
        },
        vec![
            metric("turns", app.turns.to_string(), AMBER),
            metric("requests", app.requests.to_string(), BLUE),
            metric("retries", app.retries.to_string(), AMBER),
            metric(
                "wall",
                format_duration(if app.finished {
                    app.wall_time
                } else {
                    app.elapsed
                }),
                DIM,
            ),
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
            metric("cache %", app.last_cache_pct.clone(), TEAL),
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
    if app.status.starts_with("Retrying") || !app.retries_log.is_empty() {
        let retry_lines = app
            .retries_log
            .iter()
            .rev()
            .take(4)
            .map(|record| {
                Line::from(vec![
                    Span::styled(
                        format!("↻ {} · ", record.attempt),
                        Style::default().fg(CORAL),
                    ),
                    Span::styled(record.kind.clone(), Style::default().fg(IVORY)),
                    Span::styled(
                        format!(
                            " {} ms · {}",
                            record.delay_ms,
                            if record.response_started {
                                "response started"
                            } else {
                                "no response"
                            }
                        ),
                        Style::default().fg(AMBER),
                    ),
                ])
            })
            .collect();
        let (attempt, maximum) = retry_progress(&app.retries_log, app.max_attempts.max(1));
        panel(
            frame,
            blocks[3],
            &format!("ATTEMPTS · {attempt} / {maximum}"),
            retry_lines,
        );
    } else if app.status == "Cancelled" || app.status.starts_with("Cancelled by you") {
        let completed = app
            .tools
            .iter()
            .filter(|tool| {
                matches!(
                    tool.state,
                    ToolRecordState::Success | ToolRecordState::Failed
                )
            })
            .count();
        let interrupted = app
            .tools
            .iter()
            .filter(|tool| tool.state == ToolRecordState::Interrupted)
            .count();
        let not_run = app
            .tools
            .iter()
            .filter(|tool| tool.state == ToolRecordState::NotRun)
            .count();
        panel(
            frame,
            blocks[3],
            &format!("CANCELLATION LEDGER · {} CALLS", app.tools.len()),
            vec![
                metric("completed", completed.to_string(), SAGE),
                metric("interrupted", interrupted.to_string(), AMBER),
                metric("not run", not_run.to_string(), DIM),
                Line::from(Span::styled(
                    "Interrupted calls may have side effects",
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    "Previous turn results remain in the session",
                    Style::default().fg(DIM),
                )),
            ],
        );
    } else {
        let tool_lines: Vec<_> = app
            .tools
            .iter()
            .rev()
            .take(4)
            .map(|tool| {
                let (glyph, color) = match tool.state {
                    ToolRecordState::Running => ("◌", AMBER),
                    ToolRecordState::Success => ("✓", SAGE),
                    ToolRecordState::Failed => ("×", CORAL),
                    ToolRecordState::Interrupted => ("△", AMBER),
                    ToolRecordState::NotRun => ("○", DIM),
                };
                Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(tool.name.clone(), Style::default().fg(BLUE)),
                    Span::styled(format!(" · {}", tool.detail), Style::default().fg(DIM)),
                ])
            })
            .collect();
        panel(
            frame,
            blocks[3],
            if tool_lines.is_empty() {
                if app.finished {
                    "FINAL EVENTS"
                } else {
                    "EVENTS"
                }
            } else {
                "RECENT CALLS"
            },
            if tool_lines.is_empty() {
                recent
            } else {
                tool_lines
            },
        );
    }
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

pub(super) fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!(
            "{}.{:01}M",
            rounded_tenths(value, 100_000) / 10,
            rounded_tenths(value, 100_000) % 10
        )
    } else if value >= 1_000 {
        format!(
            "{}.{:01}K",
            rounded_tenths(value, 100) / 10,
            rounded_tenths(value, 100) % 10
        )
    } else {
        value.to_string()
    }
}

fn rounded_tenths(value: u64, divisor: u64) -> u64 {
    let quotient = value / divisor;
    quotient + u64::from(value % divisor >= divisor / 2)
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let (title, text, color) = if app.editing {
        (
            "STEER · ENTER SENDS · ESC CANCELS",
            app.input.as_str(),
            AMBER,
        )
    } else if app.finished {
        (
            "NEXT TASK IN THIS SESSION",
            "Session keeps completed tasks in history · i starts another",
            app.status_color,
        )
    } else {
        ("INPUT", "Task running. Press i to queue a follow-up.", DIM)
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
    if app.show_help {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("ONE THREAD PER TASK · SETTLED DETAILS STAY QUIET"),
                Line::from("i steer / next task · ↑ edit last task · Enter sends · p pause at turn boundary"),
                Line::from("Esc cancel · q cancel + exit · Ctrl-C cancel / twice exit"),
                Line::from("↑/↓ select · Ctrl-O expand · Ctrl-R reasoning · Tab sidebar · ? help"),
            ])
            .block(
                Block::default()
                    .title("HELP")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(AMBER))
                    .style(Style::default().bg(PANEL)),
            )
            .style(Style::default().fg(IVORY)),
            centered_rect(60, 9, frame.area()),
        );
    }
    let hint = if app.finished {
        " Task finished   ·   i new task   ·   ↑ edit last task   ·   Enter send   ·   q/Esc exit   ·   ? help"
    } else {
        " i steer   ·   Esc cancel   ·   p pause   ·   q cancel + exit   ·   ↑/↓ select   ·   ^O expand   ·   ^R reasoning   ·   Tab sidebar   ·   ? help"
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn status_glyph(status: &str) -> &'static str {
    if std::env::var_os("LOOM_ASCII").is_some() {
        return if status.starts_with("Completed") {
            "+"
        } else if status.starts_with("Failed") {
            "x"
        } else if status.starts_with("Cancelled") || status.starts_with("Cancelling") {
            "/"
        } else if status.starts_with("Retrying") {
            "~"
        } else if status.starts_with("Paused") {
            "||"
        } else {
            "*"
        };
    }
    if status.starts_with("Completed") {
        "✓"
    } else if status.starts_with("Failed") {
        "×"
    } else if status.starts_with("Cancelled") || status.starts_with("Cancelling") {
        "⊘"
    } else if status.starts_with("Retrying") {
        "↻"
    } else if status.starts_with("Paused") {
        "Ⅱ"
    } else {
        "●"
    }
}

pub(super) fn retry_progress(retries: &[RetryRecord], maximum: u32) -> (u32, u32) {
    let used = retries.last().map_or(0, |retry| retry.attempt).min(maximum);
    (used, maximum)
}

pub(super) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub(super) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
