use std::env;
use std::time::Duration;

use loom_core::kernel::{
    session::TaskStatus,
    transcript::{Item, ToolStatus},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    ModelPricing, PickerState,
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
                0,
                LiveViewOptions {
                    scene: Some(scene),
                    ..LiveViewOptions::default()
                },
            );
        }
        Scene::Help => draw_help(frame),
        Scene::Picker => draw_empty_picker(frame),
    }
}

#[derive(Default, Clone, Copy)]
pub struct LiveViewOptions<'a> {
    pub sidebar_visible: bool,
    pub elapsed: Option<Duration>,
    pub dropped: Option<u64>,
    pub thread_scroll_back: usize,
    pub file_suggestions: &'a [String],
    pub pricing: Option<ModelPricing>,
    pub scene: Option<Scene>,
}

pub fn draw_live(
    frame: &mut Frame<'_>,
    reducer: &Reducer,
    input: &str,
    cursor: usize,
    options: LiveViewOptions<'_>,
) {
    draw_live_scrolled(frame, reducer, input, cursor, options);
}

#[allow(clippy::too_many_lines)] // ordered layout composition for the live screen
pub fn draw_live_scrolled(
    frame: &mut Frame<'_>,
    reducer: &Reducer,
    input: &str,
    cursor: usize,
    options: LiveViewOptions<'_>,
) {
    let LiveViewOptions {
        sidebar_visible,
        elapsed,
        dropped,
        thread_scroll_back,
        file_suggestions,
        pricing,
        scene,
    } = options;
    let area = design_area(frame.area());
    let compact = area.width < 90;
    let wide = area.width >= 120;
    let input_rows = u16::try_from(input.lines().count().clamp(2, 4)).unwrap_or(4);
    let suggestion_height = if file_suggestions.is_empty() {
        0
    } else {
        u16::try_from(file_suggestions.len().min(5)).unwrap_or(5) + 2
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(input_rows + suggestion_height + 4),
        ])
        .split(area);
    draw_live_header(frame, rows[0], reducer, scene);
    let footer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(input_rows + 2),
            Constraint::Length(suggestion_height),
            Constraint::Length(2),
        ])
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
    draw_reducer_thread(frame, body[0], reducer, compact, thread_scroll_back, scene);
    let (cursor_x, cursor_y, prompt_scroll) = prompt_cursor(input, cursor, footer[0]);
    frame.set_cursor_position((cursor_x, cursor_y));
    let placeholder = match reducer.task.status {
        Some(TaskStatus::Running) => "Task running. Type to queue a follow-up.",
        _ => "Describe a task. Use @path to include a workspace file.",
    };
    let prompt_text = if input.is_empty() { placeholder } else { input };
    frame.render_widget(
        Paragraph::new(prompt_text)
            .block(
                Block::default()
                    .title(" MESSAGE · @path to mention a file ")
                    .borders(Borders::ALL)
                    .border_style(fg(theme::AMBER)),
            )
            .style(
                Style::default()
                    .fg(if input.is_empty() {
                        theme::DIM
                    } else {
                        theme::IVORY
                    })
                    .bg(theme::BG),
            )
            .scroll((prompt_scroll, 0))
            .wrap(Wrap { trim: false }),
        footer[0],
    );
    if !file_suggestions.is_empty() && footer[1].height > 0 {
        let suggestions = file_suggestions
            .iter()
            .map(|path| Line::from(Span::styled(format!("  @{path}"), fg(theme::BLUE))))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(suggestions)
                .block(
                    Block::default()
                        .title(" FILES · Tab completes ")
                        .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP | Borders::BOTTOM)
                        .border_style(fg(theme::BORDER)),
                )
                .style(Style::default().bg(theme::BG)),
            footer[1],
        );
    }
    let keys = if reducer.task.status == Some(TaskStatus::Running) {
        "Enter send / queue  ·  Shift+Enter newline  ·  Tab @file  ·  PgUp/PgDn thread  ·  Ctrl+End latest  ·  Esc cancel"
    } else {
        "Enter send  ·  Shift+Enter newline  ·  Tab @file  ·  PgUp/PgDn thread  ·  Ctrl+End latest  ·  F2 models  ·  F1 help"
    };
    frame.render_widget(
        Paragraph::new(keys)
            .style(fg(theme::DIM))
            .wrap(Wrap { trim: true }),
        footer[2],
    );
    if wide {
        draw_live_sidebar(frame, body[1], reducer, elapsed, dropped, pricing, scene);
    } else if show_sidebar {
        let overlay = Rect::new(
            area.x + area.width.saturating_sub(42),
            rows[1].y,
            area.width.min(42),
            rows[1].height.min(12),
        );
        draw_live_sidebar(frame, overlay, reducer, elapsed, dropped, pricing, scene);
    }
}

fn prompt_cursor(input: &str, cursor: usize, area: Rect) -> (u16, u16, u16) {
    let inner_x = area.x.saturating_add(1);
    let inner_y = area.y.saturating_add(1);
    let width = area.width.saturating_sub(2).max(1) as usize;
    let before = &input[..cursor.min(input.len())];
    let (mut row, mut column) = (0_usize, 0_usize);
    for character in before.chars() {
        if character == '\n' {
            row += 1;
            column = 0;
        } else {
            if column == width {
                row += 1;
                column = 0;
            }
            column += 1;
        }
    }
    if column == width {
        row += 1;
        column = 0;
    }
    let visible_rows = usize::from(area.height.saturating_sub(2).max(1));
    let scroll = row.saturating_sub(visible_rows.saturating_sub(1));
    (
        inner_x
            .saturating_add(u16::try_from(column.min(width.saturating_sub(1))).unwrap_or(u16::MAX)),
        inner_y.saturating_add(u16::try_from(row.saturating_sub(scroll)).unwrap_or(u16::MAX)),
        u16::try_from(scroll.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
    )
}

#[allow(clippy::too_many_lines)] // assembles picker rows and responsive detail pane together
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
    lines.push(Line::from(vec![
        Span::styled("SEARCH / ", fg(theme::DIM)),
        Span::styled(
            if picker.searching {
                format!("{}▏", picker.search_query)
            } else if picker.search_query.is_empty() {
                "type / to filter provider or model".to_owned()
            } else {
                picker.search_query.clone()
            },
            fg(theme::IVORY),
        ),
    ]));
    let filtered = picker.filtered_indices();
    for index in filtered {
        let choice = &picker.choices[index];
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
    if picker.filtered_indices().is_empty() {
        lines.push(muted(if picker.choices.is_empty() {
            picker.notice.as_deref().unwrap_or("No models discovered.")
        } else {
            "No models match this search. Press Esc to clear it."
        }));
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
            "↑/↓ choose · / search · Tab effort · Enter apply · Esc clear/back · r refresh",
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
        help_key("Shift+Enter", "insert newline"),
        help_key("Tab @path", "complete a workspace file mention"),
        help_key("Ctrl+B", "toggle sidebar"),
        help_key("F1", "close help"),
        heading("WHEN IDLE"),
        help_key("Enter", "send task / start next task"),
        help_key("Shift+Enter", "insert newline"),
        help_key("F2", "open model picker"),
        help_key("F1", "open help"),
        help_key("@path + Tab", "complete workspace file mention"),
        help_key("Ctrl+C", "quit"),
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

#[allow(clippy::too_many_lines)] // transcript rendering preserves event and tool ordering
fn draw_reducer_thread(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    compact: bool,
    thread_scroll_back: usize,
    scene: Option<Scene>,
) {
    let mut lines = Vec::new();
    let latest_assistant = reducer
        .committed_transcript
        .iter()
        .rposition(|item| matches!(item, Item::Assistant(_)));
    if let Some(reason) = &reducer.task.failure_reason {
        lines.push(tinted(&format!("Task failed · {reason}"), theme::CORAL));
    }
    let mut turn = 0_u32;
    for (index, item) in reducer.committed_transcript.iter().enumerate() {
        match item {
            Item::User { text } => {
                turn = turn.saturating_add(1);
                lines.push(Line::from(vec![
                    Span::styled("› ", fg(theme::AMBER)),
                    Span::styled(text.as_str(), fg(theme::IVORY).add_modifier(Modifier::BOLD)),
                ]));
                for attachment in &reducer.task.attachments {
                    lines.push(Line::from(vec![
                        Span::styled("  ↳ attached ", fg(theme::DIM)),
                        Span::styled(attachment.path.as_str(), fg(theme::BLUE)),
                        Span::styled(
                            format!(" · {} · included directly", format_bytes(attachment.bytes)),
                            fg(theme::DIM),
                        ),
                    ]));
                }
                lines.push(turn_heading(turn, area.width, false));
            }
            Item::Assistant(message) => {
                if turn == 0 {
                    turn = 1;
                    lines.push(turn_heading(turn, area.width, false));
                }
                let is_final = Some(index) == latest_assistant
                    && reducer.task.status == Some(TaskStatus::Completed)
                    && message.tool_calls.is_empty();
                if is_final {
                    continue;
                }
                if let Some(reasoning) = message
                    .reasoning
                    .as_ref()
                    .filter(|trace| !trace.text.is_empty())
                {
                    lines.push(Line::from(Span::styled(
                        if looks_like_structured_json(&reasoning.text) {
                            "  ▸ reasoning · structured trace".to_owned()
                        } else {
                            format!("  ▸ reasoning · {} tok", reasoning.text.chars().count())
                        },
                        fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                    )));
                    if !looks_like_structured_json(&reasoning.text) {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", reasoning.text),
                            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
                if !message.content.trim().is_empty() {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(human_readable_content(&message.content), fg(theme::IVORY)),
                    ]));
                }
                if !message.tool_calls.is_empty() {
                    lines.push(muted(&format!(
                        "  {} tool call{}",
                        message.tool_calls.len(),
                        if message.tool_calls.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    )));
                    for (call_index, call) in message.tool_calls.iter().enumerate() {
                        let state = reducer.tools.get(&call.id.to_string());
                        let (marker, color, label) =
                            state.map_or((Glyph::Spinner, theme::AMBER, "running"), |tool| {
                                match tool.status {
                                    Some(loom_core::kernel::transcript::ToolStatus::Success) => {
                                        (Glyph::Check, theme::SAGE, "done")
                                    }
                                    Some(loom_core::kernel::transcript::ToolStatus::Error(_)) => {
                                        (Glyph::Cross, theme::CORAL, "failed")
                                    }
                                    Some(
                                        loom_core::kernel::transcript::ToolStatus::Cancelled {
                                            ..
                                        },
                                    ) => (Glyph::Warning, theme::AMBER, "cancelled"),
                                    Some(_) => (Glyph::Warning, theme::DIM, "finished"),
                                    None => (Glyph::Spinner, theme::AMBER, "running"),
                                }
                            });
                        let branch = if call_index + 1 == message.tool_calls.len() {
                            "  └─ "
                        } else {
                            "  ├─ "
                        };
                        let arguments = concise_arguments(&call.arguments.to_string());
                        lines.push(Line::from(vec![
                            Span::styled(branch, fg(theme::DIM)),
                            Span::styled(
                                format!("{} ", glyphs::glyph(marker, glyph_set())),
                                fg(color),
                            ),
                            Span::styled(
                                call.name.as_str(),
                                fg(theme::BLUE).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!("  {arguments}  · {label}"), fg(theme::DIM)),
                        ]));
                    }
                }
            }
            Item::ToolResult(result) => {
                let color = match result.status {
                    ToolStatus::Error(_) => theme::CORAL,
                    ToolStatus::Cancelled { .. } => theme::AMBER,
                    ToolStatus::Success | _ => theme::DIM,
                };
                lines.push(Line::from(vec![
                    Span::styled("      ↳ ", fg(theme::DIM)),
                    Span::styled(tool_result_summary(&result.content), fg(color)),
                ]));
            }
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
    if reducer.turn.is_some() && turn == 0 {
        turn = reducer.turn.unwrap_or_default().saturating_sub(1);
    }
    if reducer.has_provisional_output() {
        lines.push(turn_heading(
            reducer.turn.unwrap_or(turn.saturating_add(1)),
            area.width,
            true,
        ));
    }
    if !reducer.provisional.reasoning.is_empty() {
        lines.push(Line::from(Span::styled(
            if looks_like_structured_json(&reducer.provisional.reasoning) {
                "  ▸ reasoning · structured trace".to_owned()
            } else {
                format!(
                    "  ▸ reasoning · {} tok",
                    reducer.provisional.reasoning.chars().count()
                )
            },
            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
        )));
        if !looks_like_structured_json(&reducer.provisional.reasoning) {
            lines.push(Line::from(Span::styled(
                format!("  {}", reducer.provisional.reasoning),
                fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    if !reducer.provisional.text.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                human_readable_content(&reducer.provisional.text),
                fg(theme::IVORY),
            ),
        ]));
    }
    let mut transcript_tool_ids = std::collections::BTreeSet::new();
    for item in &reducer.committed_transcript {
        if let Item::ToolResult(result) = item {
            transcript_tool_ids.insert(result.call_id.to_string());
        }
    }
    let live_tools = reducer
        .tools
        .iter()
        .filter(|(id, _)| !transcript_tool_ids.contains(*id))
        .collect::<Vec<_>>();
    if !live_tools.is_empty() {
        lines.push(muted(&format!(
            "  {} read-only call{} · concurrent",
            live_tools.len(),
            if live_tools.len() == 1 { "" } else { "s" }
        )));
        for (index, (_, tool)) in live_tools.iter().enumerate() {
            let (marker, color, state) = match tool.status {
                Some(loom_core::kernel::transcript::ToolStatus::Success) => {
                    (Glyph::Check, theme::SAGE, "done")
                }
                Some(loom_core::kernel::transcript::ToolStatus::Error(_)) => {
                    (Glyph::Cross, theme::CORAL, "failed")
                }
                Some(loom_core::kernel::transcript::ToolStatus::Cancelled { .. }) => {
                    (Glyph::Warning, theme::AMBER, "cancelled")
                }
                Some(_) => (Glyph::Warning, theme::DIM, "finished"),
                None => (Glyph::Spinner, theme::AMBER, "running"),
            };
            let branch = if index + 1 == live_tools.len() {
                "  └─ "
            } else {
                "  ├─ "
            };
            let arguments = concise_arguments(tool.arguments.as_deref().unwrap_or(""));
            let summary = tool
                .content_head
                .as_deref()
                .map_or_else(String::new, |head| {
                    format!("  {}", tool_result_summary(head))
                });
            lines.push(Line::from(vec![
                Span::styled(branch, fg(theme::DIM)),
                Span::styled(
                    format!("{} ", glyphs::glyph(marker, glyph_set())),
                    fg(color),
                ),
                Span::styled(
                    tool.name.as_str(),
                    fg(theme::BLUE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {arguments}  · {state}{summary}"), fg(theme::DIM)),
            ]));
        }
    }
    if let Some(Item::Assistant(message)) =
        latest_assistant.and_then(|index| reducer.committed_transcript.get(index))
        && reducer.task.status == Some(TaskStatus::Completed)
        && message.tool_calls.is_empty()
    {
        lines.push(Line::from(Span::styled(
            "▸ final answer",
            fg(theme::SAGE).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(message.content.as_str(), fg(theme::IVORY)),
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
    render_thread_scrolled(frame, area, lines, thread_scroll_back);
}

fn turn_heading(turn: u32, width: u16, streaming: bool) -> Line<'static> {
    let occupied = if streaming { 19 } else { 10 };
    let fill = usize::from(width).saturating_sub(occupied);
    Line::from(vec![
        Span::styled("● ", fg(theme::AMBER)),
        Span::styled(format!("turn {turn}"), fg(theme::DIM)),
        if streaming {
            Span::styled("  ⋮ streaming", fg(theme::AMBER))
        } else {
            Span::raw("")
        },
        Span::styled(format!(" {}", "─".repeat(fill)), fg(theme::BORDER)),
    ])
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        let tenths = u128::from(bytes) * 10 / 1024;
        format!("{}.{:01} KB", tenths / 10, tenths % 10)
    }
}

fn concise_arguments(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.chars().take(96).collect();
    };
    let Some(object) = value.as_object() else {
        return String::new();
    };
    object
        .iter()
        .map(|(key, value)| {
            let scalar = match value {
                serde_json::Value::String(value) => value.as_str().to_owned(),
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                serde_json::Value::Null => "null".to_owned(),
                serde_json::Value::Array(_) => "[…]".to_owned(),
                serde_json::Value::Object(_) => "{…}".to_owned(),
            };
            format!("{key}={}", scalar.chars().take(48).collect::<String>())
        })
        .take(3)
        .collect::<Vec<_>>()
        .join(" · ")
}

fn tool_result_summary(raw: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
        && (value.is_object() || value.is_array())
    {
        return format!("result · {}", raw.chars().count());
    }
    let first_line = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    first_line.chars().take(120).collect()
}

fn looks_like_structured_json(raw: &str) -> bool {
    let trimmed = raw.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
}

fn human_readable_content(raw: &str) -> String {
    let trimmed = raw.trim();
    if looks_like_structured_json(trimmed) {
        return "(structured output hidden)".to_owned();
    }
    raw.to_owned()
}

fn render_thread_scrolled(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'_>>,
    thread_scroll_back: usize,
) {
    let width = usize::from(area.width.saturating_sub(1).max(1));
    let inner_height = usize::from(area.height);
    let visual_lines = lines
        .iter()
        .map(|line| {
            let chars = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            chars.max(1).div_ceil(width)
        })
        .sum::<usize>();
    let max_scroll = visual_lines.saturating_sub(inner_height);
    let scroll = u16::try_from(
        max_scroll
            .saturating_sub(thread_scroll_back)
            .min(usize::from(u16::MAX)),
    )
    .unwrap_or(u16::MAX);
    let is_lagging = thread_scroll_back > 0;
    let title = if is_lagging {
        format!(" THREAD  ↑ {thread_scroll_back} lines · PgDn resume follow ")
    } else {
        " THREAD  ↓ following latest ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .title(title)
                    .border_style(fg(theme::THREAD)),
            )
            .style(Style::default().bg(theme::BG))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
fn draw_test_live(frame: &mut Frame<'_>, reducer: &Reducer, input: &str) {
    draw_live(
        frame,
        reducer,
        input,
        input.len(),
        LiveViewOptions {
            sidebar_visible: true,
            ..LiveViewOptions::default()
        },
    );
}

#[allow(clippy::too_many_lines)] // ordered dashboard sections share the same panel state
fn draw_live_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
    pricing: Option<ModelPricing>,
    scene: Option<Scene>,
) {
    let reported = |value: Option<u64>| {
        if reducer.usage_reported {
            value.map_or_else(|| "not reported".to_owned(), |n| n.to_string())
        } else {
            "not reported".to_owned()
        }
    };
    let cost = if scene.is_some() {
        Some("not reported".to_owned())
    } else {
        format_run_cost(reducer, pricing)
    };
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
    if let Some(cost) = cost {
        lines.insert(
            lines.len().saturating_sub(3),
            muted(&format!("cost    {cost}")),
        );
    }
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

fn format_run_cost(reducer: &Reducer, pricing: Option<ModelPricing>) -> Option<String> {
    if let Some(reported) = reducer.usage.cost_usd
        && reported > 0.0
    {
        return Some(format!("${reported:.4}"));
    }
    if let Some(estimated) = pricing.and_then(|pricing| {
        pricing.estimate(reducer.usage.input_tokens, reducer.usage.output_tokens)
    }) {
        if estimated > 0.0 {
            return Some(format!("~${estimated:.4} est."));
        }
        if reducer.usage.input_tokens > 0 || reducer.usage.output_tokens > 0 {
            return Some("$0.0000 (free)".to_owned());
        }
    }
    reducer.usage.cost_usd.map(|value| format!("${value:.4}"))
}

fn render_panel(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    render_panel_scrolled(frame, area, title, lines, false);
}

fn render_panel_scrolled(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    follow_tail: bool,
) {
    let inner_width = usize::from(area.width.saturating_sub(2).max(1));
    let inner_height = usize::from(area.height.saturating_sub(2));
    let visual_lines = lines
        .iter()
        .map(|line| {
            let width = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            width.max(1).div_ceil(inner_width)
        })
        .sum::<usize>();
    let vertical_scroll = if follow_tail {
        u16::try_from(
            visual_lines
                .saturating_sub(inner_height)
                .min(usize::from(u16::MAX)),
        )
        .unwrap_or(u16::MAX)
    } else {
        0
    };
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
            .scroll((vertical_scroll, 0))
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
            while let Some((text, _)) = row.last_mut() {
                let trimmed = text.trim_end();
                if trimmed.len() == text.len() {
                    break;
                }
                text.truncate(trimmed.len());
                if text.is_empty() {
                    row.pop();
                }
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
                .map(ratatui::buffer::Cell::symbol)
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
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::ZERO),
                        ..LiveViewOptions::default()
                    },
                );
            })
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
        assert!(!rendered.contains("cost    "));
    }

    #[test]
    fn live_read_file_result_shows_only_a_one_line_summary() {
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect the file");
        reducer.tools.insert(
            "read-1".to_owned(),
            crate::reducer::ToolState {
                name: "read_file".to_owned(),
                arguments: Some(r#"{"path":"src/lib.rs"}"#.to_owned()),
                status: Some(loom_core::kernel::transcript::ToolStatus::Success),
                content_head: Some(
                    "src/lib.rs: 42 lines\n1\tfn main() {\n2\t    secret_file_content()".to_owned(),
                ),
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_test_live(frame, &reducer, "inspect the file"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("src/lib.rs: 42 lines"));
        assert!(!rendered.contains("fn main()"));
        assert!(!rendered.contains("secret_file_content"));
    }

    #[test]
    fn run_cost_uses_catalog_per_million_prices_when_provider_cost_is_missing() {
        let mut reducer = Reducer::default();
        reducer.usage.input_tokens = 2_000;
        reducer.usage.output_tokens = 1_000;
        let pricing = ModelPricing {
            input_per_million: Some(3.0),
            output_per_million: Some(15.0),
        };

        assert_eq!(
            format_run_cost(&reducer, Some(pricing)),
            Some("~$0.0210 est.".to_owned())
        );
    }

    #[test]
    fn reported_run_cost_takes_precedence_over_catalog_estimate() {
        let mut reducer = Reducer::default();
        reducer.usage.input_tokens = 2_000;
        reducer.usage.cost_usd = Some(0.0042);
        let pricing = ModelPricing {
            input_per_million: Some(3.0),
            output_per_million: Some(15.0),
        };

        assert_eq!(
            format_run_cost(&reducer, Some(pricing)),
            Some("$0.0042".to_owned())
        );
    }

    #[test]
    fn live_prompt_is_bordered_cursorable_and_final_answer_stays_visible() {
        let mut reducer = Reducer::default();
        reducer.begin_task("explain the result");
        reducer.committed_transcript.push(Item::Assistant(
            loom_core::kernel::transcript::AssistantMessage {
                content: "The final answer is visible.".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: loom_core::kernel::model::StopReason::EndTurn,
            },
        ));
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_test_live(frame, &reducer, "ask @src/lib.rs"))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("MESSAGE · @path to mention a file"));
        assert!(rendered.contains("final answer"));
        assert!(rendered.contains("The final answer is visible."));
        assert!(rendered.contains("ask @src/lib.rs"));
        assert!(terminal.backend().cursor_visible());
    }

    #[test]
    fn file_suggestions_are_visible_below_the_prompt() {
        let reducer = Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| {
                let suggestions = ["README.md".to_owned()];
                draw_live_scrolled(
                    frame,
                    &reducer,
                    "inspect @REA",
                    12,
                    LiveViewOptions {
                        file_suggestions: &suggestions,
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("FILES · Tab completes"));
        assert!(text.contains("@README.md"));
    }

    #[test]
    fn structured_payloads_are_not_dumped_into_the_thread() {
        assert_eq!(
            human_readable_content(r#"{"type":"thinking","secret":"x"}"#),
            "(structured output hidden)"
        );
        assert_eq!(
            concise_arguments(r#"{"path":"src/lib.rs","recursive":true}"#),
            "path=src/lib.rs · recursive=true"
        );
        assert_eq!(tool_result_summary(r#"{"files":["a","b"]}"#), "result · 19");
        assert_eq!(
            tool_result_summary("Found README.md\nextra output"),
            "Found README.md"
        );
        assert_eq!(
            tool_result_summary("src/a.rs: 42 lines\n1\tsecret_file_content"),
            "src/a.rs: 42 lines"
        );
    }

    #[test]
    fn prompt_cursor_scrolls_with_wrapped_input() {
        let area = Rect::new(4, 7, 12, 3);
        let input = "abcdefghijklmno";
        assert_eq!(prompt_cursor(input, input.len(), area), (10, 8, 1));
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
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::ZERO),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
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
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::ZERO),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
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
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::from_secs(2)),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
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
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("preview · no live data"));
            assert!(!rendered.contains("· Ready"));
            assert!(!rendered.contains("events 0"));
            assert!(rendered.contains("tools   not available"));
        }
    }
}
