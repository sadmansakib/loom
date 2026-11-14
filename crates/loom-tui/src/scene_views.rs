use std::env;
use std::time::Duration;

use loom_core::kernel::{
    context::ContextCensus,
    session::TaskStatus,
    transcript::{Item, ToolFailure, ToolResult, ToolStatus},
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    glyphs::{self, Glyph, GlyphSet},
    picker::{ModelPricing, PickerState},
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
    pub workspace: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub turn_limit: Option<u32>,
    pub thread_scroll_back: usize,
    pub sidebar_scroll_back: usize,
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
        ..
    } = options;
    let area = frame.area();
    let compact = area.width < 90;
    let wide = area.width >= 120;
    let prompt_label = if reducer.task.status == Some(TaskStatus::Running) {
        "Task running. Type to queue a follow-up."
    } else {
        "Describe a task. Use @path to include a workspace file."
    };
    let prompt_display = if input.is_empty() {
        prompt_label
    } else {
        input
    };
    let queue_count = if reducer.task.status == Some(TaskStatus::Running) {
        reducer.task.display_prompts.len().saturating_sub(1)
    } else {
        0
    };
    let input_rows = u16::try_from(input.lines().count().clamp(1, 3)).unwrap_or(3);
    let suggestion_height = if file_suggestions.is_empty() {
        0
    } else {
        u16::try_from(file_suggestions.len().min(5)).unwrap_or(5) + 2
    };
    let header_height = 2;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(5),
            Constraint::Length(input_rows + suggestion_height + 4),
        ])
        .split(area);
    draw_live_header(frame, rows[0], reducer, scene, &options);
    let footer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(input_rows + 2),
            Constraint::Length(
                suggestion_height.min(rows[2].height.saturating_sub(input_rows + 4)),
            ),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(rows[2]);
    let show_sidebar = wide || (area.width >= 90 && sidebar_visible);
    let body = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(65), Constraint::Length(42)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(rows[1])
    };
    draw_reducer_thread(frame, body[0], reducer, compact, thread_scroll_back, scene);
    let (cursor_x, cursor_y, prompt_scroll) = prompt_cursor(input, cursor, footer[0]);
    frame.set_cursor_position((cursor_x.saturating_add(2), cursor_y));
    let prompt_spans = vec![
        Span::styled("› ", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
        Span::styled(
            prompt_display,
            fg(if input.is_empty() {
                theme::DIM
            } else {
                theme::IVORY
            }),
        ),
        Span::raw(" "),
        Span::styled(format!("queued {queue_count}"), fg(theme::DIM)),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(prompt_spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(fg(theme::BORDER)),
            )
            .style(Style::default().bg(theme::BG))
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
    let keys = if scene.is_some() {
        "[Enter] start task   [Tab] @file   [F2] models   [F1] help"
    } else if reducer.task.status == Some(TaskStatus::Running) {
        "[Esc] cancel   [^O] details   [^R] reasoning   [^B] sidebar   [F1] help"
    } else {
        "[Enter] send   [Tab] @file   [F2] models   [F1] help"
    };
    let footer_hint = if scene.is_some() {
        Line::from(keys)
    } else {
        Line::from(vec![
            Span::styled("[Esc]", fg(theme::IVORY).add_modifier(Modifier::BOLD)),
            Span::styled(
                " cancel   [^O] details   [^R] reasoning   [^B] sidebar   [F1] help",
                fg(theme::DIM),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(footer_hint).style(fg(theme::DIM)), footer[2]);
    if scene.is_none() {
        let counters = match dropped {
            Some(dropped) => format!("events {} · dropped {dropped}", reducer.event_count),
            None => "events unavailable".to_owned(),
        };
        frame.render_widget(
            Paragraph::new(counters)
                .style(fg(theme::DIM))
                .alignment(Alignment::Right),
            Rect {
                x: footer[2].x + footer[2].width.saturating_sub(28),
                width: 28,
                ..footer[2]
            },
        );
    }
    let draw_side = |frame: &mut Frame<'_>, sidebar: Rect| match scene {
        None => {
            draw_runtime_sidebar(frame, sidebar, reducer, elapsed, dropped, pricing, &options);
        }
        Some(scene) => {
            draw_scene_sidebar(frame, sidebar, reducer, elapsed, scene, options.turn_limit);
        }
    };
    if wide {
        draw_side(frame, body[1]);
    } else if show_sidebar {
        let overlay = Rect::new(
            area.x + area.width.saturating_sub(42),
            rows[1].y,
            area.width.min(42),
            rows[1].height.min(12),
        );
        draw_side(frame, overlay);
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

fn draw_live_header(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    scene: Option<Scene>,
    options: &LiveViewOptions<'_>,
) {
    let model = if scene.is_some() {
        "model not available in static preview"
    } else {
        reducer
            .observed_model
            .as_deref()
            .or(options.model)
            .unwrap_or("model not reported")
    };
    let state = if scene.is_some() {
        "preview · no live data"
    } else {
        status_label(reducer)
    };
    let workspace = options.workspace.unwrap_or("workspace unavailable");
    let session = options.session_id.unwrap_or("session unavailable");
    let effort = options.effort.unwrap_or("not reported");
    let thinking = options.thinking.unwrap_or("not reported");
    let (left, right) = if scene.is_some() {
        (
            Line::from(vec![
                Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  · {state}"), fg(theme::DIM)),
            ]),
            Line::from(Span::styled(model, fg(theme::BLUE))),
        )
    } else {
        (
            Line::from(vec![
                Span::styled("◆ ", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
                Span::styled("loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("   {workspace}  ·  session {session}"),
                    fg(theme::DIM),
                ),
            ]),
            Line::from(vec![
                Span::styled(model, fg(theme::BLUE)),
                Span::styled("   effort ", fg(theme::DIM)),
                Span::styled(effort, fg(theme::IVORY).add_modifier(Modifier::BOLD)),
                Span::styled("   thinking ", fg(theme::DIM)),
                Span::styled(thinking, fg(theme::IVORY).add_modifier(Modifier::BOLD)),
            ]),
        )
    };
    let content = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(content);
    frame.render_widget(Paragraph::new(left), columns[0]);
    frame.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(fg(theme::BORDER)),
        Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..area
        },
    );
}

#[allow(clippy::too_many_lines, clippy::wildcard_enum_match_arm)] // transcript rendering preserves event and tool ordering; Item is non_exhaustive
fn draw_reducer_thread(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    compact: bool,
    thread_scroll_back: usize,
    scene: Option<Scene>,
) {
    let mut lines = Vec::new();
    if let Some(prompt) = reducer
        .original_instruction
        .as_deref()
        .or_else(|| reducer.task.display_prompts.first().map(String::as_str))
    {
        lines.push(Line::from(Span::styled(
            "TASK · original instruction",
            fg(theme::DIM),
        )));
        lines.push(Line::from(vec![
            Span::styled("› ", fg(theme::AMBER)),
            Span::styled(prompt, fg(theme::IVORY).add_modifier(Modifier::BOLD)),
        ]));
    }
    if let Some(reason) = &reducer.task.failure_reason {
        lines.push(tinted(&format!("Task failed · {reason}"), theme::CORAL));
    }
    let mut rendered_retries = std::collections::BTreeSet::new();
    let transcript_tool_ids = reducer
        .committed_transcript
        .iter()
        .filter_map(|item| {
            if let Item::ToolResult(result) = item {
                Some(result.call_id.to_string())
            } else {
                None
            }
        })
        .collect::<std::collections::BTreeSet<_>>();

    for (turn_number, turn) in &reducer.execution_turns {
        let streaming = reducer.turn == Some(*turn_number) && reducer.has_provisional_output();
        lines.push(turn_heading(*turn_number, area.width, streaming, reducer));
        for input in &turn.user_inputs {
            lines.push(Line::from(vec![
                Span::styled("│  › ", fg(theme::AMBER)),
                Span::styled(
                    input.as_str(),
                    fg(theme::IVORY).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        lines.push(Line::from(Span::styled("│", fg(theme::THREAD))));

        for (retry_index, retry) in reducer
            .retries
            .iter()
            .enumerate()
            .filter(|(_, retry)| retry.turn == Some(*turn_number))
        {
            rendered_retries.insert(retry_index);
            lines.push(Line::from(vec![
                Span::styled("│  ", fg(theme::THREAD)),
                Span::styled(
                    format!(
                        "{} attempt {}",
                        glyphs::glyph(Glyph::Retry, glyph_set()),
                        retry.attempt
                    ),
                    fg(theme::CORAL),
                ),
                Span::styled(
                    format!(
                        " · {} · {} ms · {} · discarded",
                        retry.kind,
                        retry.delay_ms,
                        if retry.response_started {
                            "response started"
                        } else {
                            "no response"
                        }
                    ),
                    fg(theme::DIM),
                ),
            ]));
        }

        // Assistant content is deferred until after tool results so the
        // turn reads chronologically: reasoning → tool executions → tool
        // results → the model's prose (reference ordering).
        let mut assistant_content: Vec<String> = Vec::new();
        for item in &turn.transcript {
            match item {
                Item::Assistant(message) => {
                    if let Some(reasoning) = message
                        .reasoning
                        .as_ref()
                        .filter(|trace| !trace.text.is_empty())
                    {
                        let indicator = if reducer.expanded.reasoning {
                            "▾"
                        } else {
                            "▸"
                        };
                        let size = reasoning_size(&reasoning.text, reducer, *turn_number);
                        lines.push(Line::from(Span::styled(
                            format!("│  {indicator} reasoning · {size}"),
                            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                        )));
                        if reducer.expanded.reasoning
                            && !looks_like_structured_json(&reasoning.text)
                        {
                            lines.push(Line::from(Span::styled(
                                format!("│      {}", reasoning.text),
                                fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                            )));
                        }
                    }
                    if !message.content.trim().is_empty() {
                        assistant_content.push(human_readable_content(&message.content));
                    }
                    for (call_index, call) in message.tool_calls.iter().enumerate() {
                        let call_id = call.id.to_string();
                        let state = reducer.tools.get(&call_id);
                        let (marker, color, label) =
                            state.map_or((Glyph::Spinner, theme::AMBER, "running"), |tool| {
                                match tool.status {
                                    Some(ToolStatus::Success) => {
                                        (Glyph::Check, theme::SAGE, "done")
                                    }
                                    Some(ToolStatus::Error(_)) => {
                                        (Glyph::Cross, theme::CORAL, "failed")
                                    }
                                    Some(ToolStatus::Cancelled { .. }) => {
                                        (Glyph::Warning, theme::AMBER, "cancelled")
                                    }
                                    Some(_) => (Glyph::Warning, theme::DIM, "finished"),
                                    None => (Glyph::Spinner, theme::AMBER, "running"),
                                }
                            });
                        let duration = state.and_then(|tool| {
                            tool.started_at_millis
                                .zip(tool.completed_at_millis)
                                .map(|(start, end)| end.saturating_sub(start))
                        });
                        let timing = duration.map_or_else(String::new, |ms| format!(" · {ms}ms"));
                        let branch = if call_index + 1 == message.tool_calls.len() {
                            "│    └─ "
                        } else {
                            "│    ├─ "
                        };
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
                            Span::styled(
                                format!(
                                    "  {}  · {label}{timing}",
                                    concise_arguments(&call.arguments.to_string())
                                ),
                                fg(theme::DIM),
                            ),
                        ]));
                        if reducer.expanded.tools {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "│       args: {}",
                                    truncate_chars(&call.arguments.to_string(), 160)
                                ),
                                fg(theme::DIM),
                            )));
                        }
                    }
                }
                Item::ToolResult(result) => {
                    lines.extend(tool_result_lines(result, reducer.expanded.tools));
                }
                _ => {}
            }
        }
        for (_id, tool) in reducer.tools.iter().filter(|(id, _)| {
            reducer.tool_call_turn(id) == Some(*turn_number) && !transcript_tool_ids.contains(*id)
        }) {
            let (marker, color, status) = match tool.status {
                Some(ToolStatus::Success) => (Glyph::Check, theme::SAGE, "done"),
                Some(ToolStatus::Error(_)) => (Glyph::Cross, theme::CORAL, "failed"),
                Some(ToolStatus::Cancelled { .. }) => (Glyph::Warning, theme::AMBER, "cancelled"),
                Some(_) => (Glyph::Warning, theme::DIM, "finished"),
                None => (Glyph::Spinner, theme::AMBER, "running"),
            };
            let timing = tool
                .started_at_millis
                .zip(tool.completed_at_millis)
                .map(|(start, end)| end.saturating_sub(start))
                .map_or_else(String::new, |ms| format!(" · {ms}ms"));
            lines.push(Line::from(vec![
                Span::styled("│    └─ ", fg(theme::DIM)),
                Span::styled(
                    format!("{} ", glyphs::glyph(marker, glyph_set())),
                    fg(color),
                ),
                Span::styled(
                    tool.name.as_str(),
                    fg(theme::BLUE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {}  · {status}{timing}",
                        concise_arguments(tool.arguments.as_deref().unwrap_or(""))
                    ),
                    fg(theme::DIM),
                ),
            ]));
            if reducer.expanded.tools
                && let Some(output) = &tool.content_head
            {
                lines.push(Line::from(Span::styled(
                    format!(
                        "│       output preview: {}",
                        truncate_chars(&output.replace('\n', " ↵ "), 160)
                    ),
                    fg(color),
                )));
            }
        }
        // The model's prose lands after every execution in the turn.
        for content in assistant_content {
            lines.push(Line::from(vec![
                Span::styled("│  ", fg(theme::THREAD)),
                Span::styled(content, fg(theme::IVORY)),
            ]));
        }
        if reducer.turn == Some(*turn_number)
            && (reducer.has_provisional_output()
                || (!matches!(
                    reducer.task.status,
                    Some(TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled)
                ) && reducer.task.status.is_some()))
        {
            if !reducer.provisional.reasoning.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "│  {} reasoning · provisional · streaming {}",
                        if reducer.expanded.reasoning {
                            "▾"
                        } else {
                            "▸"
                        },
                        glyphs::glyph(Glyph::Spinner, glyph_set()),
                    ),
                    fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                )));
                if reducer.expanded.reasoning {
                    lines.push(Line::from(vec![
                        Span::styled("│      ", fg(theme::THREAD)),
                        Span::styled(
                            truncate_chars(&reducer.provisional.reasoning, 200),
                            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                        ),
                        Span::styled(
                            streaming_cursor(),
                            fg(theme::VIOLET).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
            }
            if !reducer.provisional.text.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("│  ", fg(theme::THREAD)),
                    Span::styled(
                        human_readable_content(&reducer.provisional.text),
                        fg(theme::IVORY),
                    ),
                    Span::styled(
                        streaming_cursor(),
                        fg(theme::AMBER).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
        }
        lines.push(Line::from(Span::styled("│", fg(theme::THREAD))));
    }

    // A committed transcript may contain a fast-path result without model turns.
    if reducer.execution_turns.is_empty() {
        for item in &reducer.committed_transcript {
            if let Item::Assistant(message) = item {
                lines.push(Line::from(Span::styled(
                    human_readable_content(&message.content),
                    fg(theme::IVORY),
                )));
            }
        }
    }
    for (retry_index, retry) in reducer.retries.iter().enumerate() {
        if !rendered_retries.contains(&retry_index) {
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
    if reducer.task.status == Some(TaskStatus::Running) {
        for prompt in reducer.task.display_prompts.iter().skip(1) {
            lines.push(Line::from(vec![
                Span::styled("  ↳ queued · ", fg(theme::AMBER)),
                Span::styled(prompt.as_str(), fg(theme::IVORY)),
            ]));
        }
    }
    if !compact && reducer.follow_up_queued && reducer.task.display_prompts.len() <= 1 {
        lines.push(tinted("follow-up queued for next turn", theme::AMBER));
    }
    if scene.is_some() {
        render_preview_thread(frame, area, "task thread", lines);
    } else {
        render_thread_scrolled(frame, area, lines, thread_scroll_back);
    }
}

fn turn_heading(turn: u32, width: u16, streaming: bool, reducer: &Reducer) -> Line<'static> {
    let metrics = reducer.turn_metrics.get(&turn);
    let usage = metrics
        .filter(|metrics| metrics.usage_reported)
        .map_or(String::new(), |metrics| {
            format!(
                " · {} in / {} out",
                metrics.usage.input_tokens, metrics.usage.output_tokens
            )
        });
    let cached = metrics
        .filter(|metrics| metrics.usage_reported)
        .and_then(|metrics| metrics.usage.cached_input_tokens)
        .filter(|tokens| *tokens > 0)
        .map_or(String::new(), |tokens| {
            format!(" · {} cached", format_compact_tokens(tokens))
        });
    let duration = metrics
        .and_then(|metrics| {
            Some(
                metrics
                    .completed_at_millis?
                    .saturating_sub(metrics.started_at_millis?),
            )
        })
        .map_or(String::new(), |millis| format!(" · {millis}ms"));
    let state = if streaming { "  ⋮ streaming" } else { "" };
    let metadata = format!("turn {turn}{usage}{cached}{duration}{state}");
    let occupied = metadata.chars().count() + 5;
    let fill = usize::from(width).saturating_sub(occupied);
    Line::from(vec![
        Span::styled("╭─ ", fg(theme::THREAD)),
        Span::styled(
            format!("{metadata} "),
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}", "─".repeat(fill)), fg(theme::THREAD)),
    ])
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

/// Truncates to a character budget with an ellipsis marker.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Block cursor shown while the model streams provisional output.
fn streaming_cursor() -> &'static str {
    if matches!(glyph_set(), GlyphSet::Unicode) {
        "▌"
    } else {
        "|"
    }
}

/// Reasoning size prefers the provider-reported thinking tokens for the turn
/// and falls back to the character count of the streamed trace.
fn reasoning_size(text: &str, reducer: &Reducer, turn: u32) -> String {
    reducer
        .turn_metrics
        .get(&turn)
        .filter(|metrics| metrics.usage_reported)
        .and_then(|metrics| metrics.usage.thinking_tokens)
        .map_or_else(
            || format!("{} chars", text.chars().count()),
            |tokens| format!("{} tok", format_compact_tokens(tokens)),
        )
}

fn failure_category(failure: ToolFailure) -> &'static str {
    match failure {
        ToolFailure::UnknownTool => "unknown tool",
        ToolFailure::NotExposed => "not exposed",
        ToolFailure::InvalidInput => "invalid input",
        ToolFailure::Execution => "execution",
        ToolFailure::Timeout => "timeout",
        ToolFailure::RepeatedCall => "repeated call",
        _ => "error",
    }
}

/// A numbered file window produced by `read_file`: a `path: N lines` header,
/// `line-number<TAB>text` body rows, and an optional `... [` trailer.
struct NumberedRead<'a> {
    header: &'a str,
    body: Vec<(usize, &'a str)>,
    trailer: Option<&'a str>,
}

fn parse_numbered_read(raw: &str) -> Option<NumberedRead<'_>> {
    let mut lines = raw.lines();
    let header = lines.next()?.trim_end();
    let stem = header.strip_suffix(" lines")?;
    stem.rsplit_once(' ')?.1.parse::<usize>().ok()?;
    let mut body = Vec::new();
    let mut trailer = None;
    for line in lines {
        if let Some((number, text)) = line.split_once('\t')
            && let Ok(number) = number.trim().parse::<usize>()
        {
            body.push((number, text));
        } else if line.trim_start().starts_with("... [") {
            trailer = Some(line.trim());
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(NumberedRead {
        header,
        body,
        trailer,
    })
}

fn comment_line(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with('#')
}

/// Renders one committed tool result: a status line with the honest category,
/// then a bounded, tool-aware preview (numbered file windows keep their
/// gutter; everything else summarizes to its first meaningful line).
fn tool_result_lines(result: &ToolResult, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(read) = parse_numbered_read(&result.content) {
        let truncated = if result.truncated {
            " · truncated"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled("│       ↳ ", fg(theme::DIM)),
            Span::styled(
                format!(
                    "{} ok{truncated} · ",
                    glyphs::glyph(Glyph::Check, glyph_set())
                ),
                fg(theme::SAGE),
            ),
            Span::styled(read.header.to_owned(), fg(theme::SAGE)),
        ]));
        let cap = if expanded { 12 } else { 3 };
        for (number, text) in read.body.iter().take(cap) {
            let style = if comment_line(text) {
                theme::DIM
            } else {
                theme::IVORY
            };
            lines.push(Line::from(vec![
                Span::styled(format!("│         {number:>3} │ "), fg(theme::DIM)),
                Span::styled(truncate_chars(text, 80), fg(style)),
            ]));
        }
        if read.body.len() > cap {
            lines.push(Line::from(Span::styled(
                format!("│         … +{} more lines", read.body.len() - cap),
                fg(theme::DIM),
            )));
        }
        if let Some(trailer) = read.trailer {
            lines.push(Line::from(Span::styled(
                format!("│         {trailer}"),
                fg(theme::DIM),
            )));
        }
        return lines;
    }
    let (color, marker, status) = match result.status {
        ToolStatus::Success => (theme::SAGE, Glyph::Check, "ok".to_owned()),
        ToolStatus::Error(failure) => (
            theme::CORAL,
            Glyph::Cross,
            format!("error · {}", failure_category(failure)),
        ),
        ToolStatus::Cancelled { .. } => (theme::AMBER, Glyph::Warning, "cancelled".to_owned()),
        _ => (theme::DIM, Glyph::Warning, "finished".to_owned()),
    };
    let truncated = if result.truncated {
        " · truncated"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled("│       ↳ ", fg(theme::DIM)),
        Span::styled(
            format!(
                "{} {status}{truncated} · ",
                glyphs::glyph(marker, glyph_set())
            ),
            fg(color),
        ),
        Span::styled(tool_result_summary(&result.content), fg(color)),
    ]));
    if expanded {
        let content_lines: Vec<&str> = result
            .content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let cap = 12;
        for line in content_lines.iter().take(cap) {
            lines.push(Line::from(vec![
                Span::styled("│         ", fg(theme::THREAD)),
                Span::styled(truncate_chars(line, 96), fg(theme::DIM)),
            ]));
        }
        if content_lines.len() > cap {
            lines.push(Line::from(Span::styled(
                format!("│         … +{} more lines", content_lines.len() - cap),
                fg(theme::DIM),
            )));
        }
    }
    lines
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
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(fg(theme::THREAD)),
            )
            .style(Style::default().bg(theme::BG))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_preview_thread(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" {title} "))
                    .borders(Borders::ALL)
                    .border_style(fg(theme::BORDER)),
            )
            .style(Style::default().bg(theme::BG))
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

/// Sidebar for the static preview scenes: every live counter is explicitly
/// marked unavailable so a preview can never imply a running task.
fn draw_scene_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    elapsed: Option<Duration>,
    scene: Scene,
    turn_limit: Option<u32>,
) {
    let reported = |value: Option<u64>| {
        if reducer.usage_reported {
            value.map_or_else(|| "not reported".to_owned(), |n| n.to_string())
        } else {
            "not reported".to_owned()
        }
    };
    let cost = "not reported".to_owned();
    let elapsed = elapsed.map_or_else(
        || "not available".to_owned(),
        |value| format!("{:02}:{:02}", value.as_secs() / 60, value.as_secs() % 60),
    );
    let mut lines = vec![
        heading("TASK"),
        muted("no task status in static preview"),
        muted("model not available in static preview"),
        muted("task state not available in static preview"),
        heading("LIVE USAGE · UNAVAILABLE"),
        muted("not available in static preview"),
        muted("not available in static preview"),
        muted("not available in static preview"),
        muted("not available in static preview"),
        muted("elapsed not available in static preview"),
        muted("tools   not available"),
        muted(""),
    ];
    let tools_usage = muted(&format!("tools   {} calls observed", reducer.tools.len()));
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
        tools_usage,
        muted(""),
    ]);
    lines.insert(
        lines.len().saturating_sub(3),
        muted(&format!("cost    {cost}")),
    );
    match scene {
        Scene::Cancel => {
            lines.push(heading("CANCELLATION LEDGER"));
            lines.push(muted("No cancellation event in this preview."));
            lines.push(muted("Call outcomes are not available."));
        }
        Scene::Retry => {
            lines.push(heading("RETRIES"));
            lines.push(muted("No retry events in this preview."));
            lines.push(muted("Retry policy is not exposed by the event stream."));
        }
        Scene::Complete => {
            lines.push(heading("RUN RECORD"));
            lines.push(muted("No completed task in this preview."));
        }
        Scene::Main | Scene::Picker | Scene::Help => {}
    }
    lines.extend([
        heading("PREVIEW NOTES"),
        muted("No cancellation event in this preview."),
        muted("No retry events in this preview."),
        muted("No completed task in this preview."),
    ]);
    if let Some(limit) = turn_limit {
        lines.push(heading("TASK LIMIT"));
        lines.push(muted(&format!("{limit} turns configured")));
    }
    lines.push(muted(""));
    lines.push(heading("TASK BUDGET · REMAINING"));
    lines.push(muted("not available in static preview"));
    lines.push(heading("REQUEST CONTEXT · LATEST COMPILE"));
    lines.push(muted("not available in static preview"));
    lines.push(heading("EVENTS"));
    lines.extend(reducer.recent_events.iter().map(|event| muted(event)));
    lines.push(muted("events unavailable in preview"));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::BG))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[allow(clippy::too_many_lines)] // ordered dashboard sections share the same panel state
fn draw_runtime_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
    pricing: Option<ModelPricing>,
    options: &LiveViewOptions<'_>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(fg(theme::BORDER))
            .style(Style::default().bg(theme::BG)),
        area,
    );
    let content = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(1),
        ..area
    };
    if content.width == 0 {
        return;
    }

    let elapsed_label = elapsed.map_or_else(
        || "not available".to_owned(),
        |value| format!("{:02}:{:02}", value.as_secs() / 60, value.as_secs() % 60),
    );
    let task_id = reducer.task.task_id.map_or_else(
        || "task id unavailable".to_owned(),
        |id| format!("t_{}", &id.to_string()[..8]),
    );
    let status_color = if reducer.task.status == Some(TaskStatus::Running) {
        theme::AMBER
    } else if reducer.task.status == Some(TaskStatus::Completed) {
        theme::SAGE
    } else if reducer.task.status == Some(TaskStatus::Failed) {
        theme::CORAL
    } else {
        theme::DIM
    };
    let task_lines = vec![
        heading("TASK"),
        Line::from(vec![
            Span::styled("◈ ", fg(status_color)),
            Span::styled(
                status_label(reducer),
                fg(status_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {elapsed_label}"), fg(theme::DIM)),
        ]),
        muted(&task_id),
    ];

    let turns = reducer.turn.map(u64::from);
    let turn_limit = options.turn_limit.map(u64::from);
    let mut budget_lines = vec![
        heading("BUDGET"),
        sidebar_budget_width("turns", turns, turn_limit, content.width),
    ];
    if let (Some(used), Some(limit)) = (turns, turn_limit) {
        budget_lines.push(sidebar_limit_bar(used, limit, content.width));
    } else {
        budget_lines.push(sidebar_unbounded_bar(content.width));
    }
    if let Some(progress) = reducer.context_progress {
        let budget = progress.remaining_budget;
        if let Some(remaining) = budget.tool_calls {
            let limit = progress
                .tool_calls_used
                .saturating_add(u64::from(remaining));
            let used = reducer.tool_calls_started;
            budget_lines.push(sidebar_budget_width(
                "tool calls",
                Some(used),
                Some(limit),
                content.width,
            ));
            budget_lines.push(sidebar_limit_bar(used, limit, content.width));
        } else {
            budget_lines.push(sidebar_budget_width(
                "tool calls",
                Some(reducer.tool_calls_started),
                None,
                content.width,
            ));
            budget_lines.push(sidebar_unbounded_bar(content.width));
        }
        if let Some(remaining) = budget.wall_time {
            let used = elapsed.or(progress.elapsed_at_compile);
            if let (Some(used), Some(elapsed_at_compile)) = (used, progress.elapsed_at_compile) {
                let limit = elapsed_at_compile.saturating_add(remaining);
                budget_lines.push(sidebar_budget_text(
                    "wall",
                    &format!("{} / {}", format_duration(used), format_duration(limit)),
                    content.width,
                ));
                budget_lines.push(sidebar_limit_bar(
                    used.as_secs(),
                    limit.as_secs(),
                    content.width,
                ));
            } else {
                budget_lines.push(sidebar_budget_text(
                    "wall",
                    &format!(
                        "{} elapsed · limit available",
                        elapsed.map_or_else(|| "time unavailable".to_owned(), format_duration)
                    ),
                    content.width,
                ));
                budget_lines.push(sidebar_unbounded_bar(content.width));
            }
        } else {
            budget_lines.push(sidebar_budget_text(
                "wall",
                &format!("{elapsed_label} · unbounded"),
                content.width,
            ));
            budget_lines.push(sidebar_unbounded_bar(content.width));
        }
        budget_lines.push(muted(&format!(
            "tokens  in {} · out {} · total {}",
            optional_budget(budget.input_tokens),
            optional_budget(budget.output_tokens),
            optional_budget(budget.total_tokens)
        )));
    } else {
        budget_lines.push(sidebar_budget_text(
            "tool calls",
            &format!(
                "{} observed · limit unavailable",
                reducer.tool_calls_started
            ),
            content.width,
        ));
        budget_lines.push(sidebar_unbounded_bar(content.width));
        budget_lines.push(sidebar_budget_text(
            "wall",
            &format!("{elapsed_label} · limit unavailable"),
            content.width,
        ));
        budget_lines.push(sidebar_unbounded_bar(content.width));
    }

    let mut context_lines = vec![heading("NEXT REQUEST · CONTEXT")];
    if let Some(progress) = reducer.context_progress {
        let census = progress.census;
        context_lines.push(context_bar(census, content.width));
        context_lines.extend(context_segments(census, content.width));
        let capacity = progress.context_budget.max_input_tokens;
        let utilization = if capacity > 0 {
            format!(
                "{:.1}% of {}",
                f64::from(census.total) * 100.0 / f64::from(capacity),
                format_compact_tokens(u64::from(capacity))
            )
        } else {
            "capacity unavailable".to_owned()
        };
        context_lines.push(muted(&format!(
            "total (est.) {} · estimated {}/{} · {utilization}",
            format_compact_tokens(u64::from(census.total)),
            census.total,
            capacity,
        )));
    } else {
        context_lines.push(muted("not reported before request compilation"));
    }

    let cost = format_run_cost(reducer, pricing).unwrap_or_else(|| "not reported".to_owned());
    let usage_value = |value: Option<u64>| {
        if reducer.usage_reported {
            value.map_or_else(|| "not reported".to_owned(), format_compact_tokens)
        } else {
            "not reported".to_owned()
        }
    };
    let cost_lines = vec![
        heading("COST"),
        Line::from(vec![Span::styled(
            cost,
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        )]),
        muted(&format!(
            "in {} · {} cached",
            usage_value(Some(reducer.usage.input_tokens)),
            usage_value(reducer.usage.cached_input_tokens)
        )),
        muted(&format!(
            "out {}",
            usage_value(Some(reducer.usage.output_tokens))
        )),
    ];

    let event_count = match dropped {
        Some(dropped) => tinted(
            &format!("events {} · dropped {dropped}", reducer.event_count),
            if dropped == 0 {
                theme::DIM
            } else {
                theme::CORAL
            },
        ),
        None => muted("event counters not available"),
    };

    if let Some(summary) = &reducer.summary {
        let census = if summary.model_requests > 0 {
            format!(
                "final request census {} tokens",
                summary.last_context_census.total
            )
        } else {
            "final request census not available; no request compiled".to_owned()
        };
        budget_lines.extend([
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
            muted(&census),
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
            muted(&format!(
                "output budget {} · wall {}",
                optional_budget(summary.remaining_budget.output_tokens),
                summary
                    .remaining_budget
                    .wall_time
                    .map_or_else(|| "unbounded".to_owned(), format_duration)
            )),
        ]);
    }

    // Reserve space for every sidebar section first. Compress informational
    // rows before sacrificing the event panel at shorter terminal heights.
    let mut sections_data = vec![
        ("TASK", task_lines),
        ("BUDGET", budget_lines),
        ("NEXT REQUEST · CONTEXT", context_lines),
        ("COST", cost_lines),
    ];
    // Each bordered section consumes a bottom-rule row in addition to its
    // text. Account for that row so natural section heights do not overrun the
    // sidebar and push EVENTS below the viewport.
    let minimums = [4_u16, 6, 5, 3, 3];
    let mut heights = sections_data
        .iter()
        .map(|(_, lines)| {
            u16::try_from(lines.len())
                .unwrap_or(u16::MAX)
                .saturating_add(1)
        })
        .collect::<Vec<_>>();
    let available = content.height;
    while heights.iter().copied().sum::<u16>() + minimums[4] > available {
        let Some((index, _)) = heights
            .iter()
            .enumerate()
            .filter(|(index, height)| **height > minimums[*index])
            .max_by_key(|(_, height)| *height)
        else {
            break;
        };
        heights[index] -= 1;
    }
    if heights.iter().copied().sum::<u16>() >= available {
        // Tiny terminal fallback: keep the event heading visible rather than
        // allowing the last section to disappear entirely.
        heights.fill(0);
    }
    let event_height = available.saturating_sub(heights.iter().copied().sum::<u16>());
    let mut constraints = heights
        .iter()
        .map(|height| Constraint::Length(*height))
        .collect::<Vec<_>>();
    constraints.push(Constraint::Length(event_height));
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(content);
    for (index, ((_, mut lines), area)) in sections_data
        .drain(..)
        .zip(areas.iter().copied())
        .enumerate()
    {
        let line_capacity = usize::from(heights[index].saturating_sub(1));
        if lines.len() > line_capacity {
            lines.truncate(line_capacity);
            if let Some(last) = lines.last_mut() {
                *last = muted("…");
            }
        }
        render_sidebar_section(frame, area, lines, true);
    }

    let Some(&event_area) = areas.last() else {
        return;
    };
    let event_rows = usize::from(event_area.height);
    let mut event_lines = if event_rows == 0 {
        Vec::new()
    } else {
        vec![heading("EVENTS")]
    };
    let event_count_rows = usize::from(event_rows >= 2);
    event_lines.extend(
        reducer
            .recent_events
            .iter()
            .skip(options.sidebar_scroll_back)
            .take(event_rows.saturating_sub(1 + event_count_rows))
            .map(|event| event_line(event)),
    );
    if event_count_rows == 1 && event_lines.len() < event_rows {
        event_lines.push(event_count);
    }
    frame.render_widget(
        Paragraph::new(event_lines).style(Style::default().bg(theme::BG)),
        event_area,
    );
}

fn render_sidebar_section(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    separated: bool,
) {
    let border = if separated {
        Borders::BOTTOM
    } else {
        Borders::empty()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(border)
                    .border_style(fg(theme::BORDER)),
            )
            .style(Style::default().bg(theme::BG))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn sidebar_limit_bar(used: u64, limit: u64, width: u16) -> Line<'static> {
    let inner = usize::from(width);
    let filled = match limit {
        // A zero limit means unbounded: a full bar once anything ran.
        0 => usize::from(used > 0) * inner,
        limit => {
            let cap = u64::try_from(inner).unwrap_or(u64::MAX);
            usize::try_from((used.saturating_mul(cap) / limit).min(cap)).unwrap_or(inner)
        }
    };
    Line::from(vec![
        Span::styled("━".repeat(filled), fg(theme::AMBER)),
        Span::styled("━".repeat(inner.saturating_sub(filled)), fg(theme::BORDER)),
    ])
}

fn sidebar_unbounded_bar(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "━".repeat(usize::from(width)),
        fg(theme::BORDER),
    ))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
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
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" {title} "))
                    .borders(Borders::ALL),
            )
            .style(Style::default().bg(theme::BG))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[allow(dead_code)]
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
                    .title(title)
                    .borders(Borders::LEFT)
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
    area
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

fn sidebar_budget_width(
    label: &str,
    used: Option<u64>,
    limit: Option<u64>,
    width: u16,
) -> Line<'static> {
    let (used, limit) = match (used, limit) {
        (Some(used), Some(limit)) => (used.to_string(), limit.to_string()),
        (Some(used), None) => (used.to_string(), "unbounded".to_owned()),
        _ => ("not reported".to_owned(), "not reported".to_owned()),
    };
    sidebar_budget_text(label, &format!("{used} / {limit}"), width)
}

fn sidebar_budget_text(label: &str, value: &str, width: u16) -> Line<'static> {
    let label_len = label.chars().count();
    let value_len = value.chars().count();
    let gap = usize::from(width)
        .saturating_sub(label_len.saturating_add(value_len))
        .max(2);
    Line::from(vec![
        Span::styled(label.to_owned(), fg(theme::DIM)),
        Span::raw(" ".repeat(gap)),
        Span::styled(value.to_owned(), fg(theme::IVORY)),
    ])
}
fn context_bar(census: ContextCensus, width: u16) -> Line<'static> {
    let segments = context_categories(census);
    let total = segments
        .iter()
        .map(|(_, value, _)| u64::from(*value))
        .sum::<u64>();
    let cells = usize::from(width.saturating_sub(1));
    if total == 0 || cells == 0 {
        return Line::from(Span::styled("░".repeat(cells), fg(theme::BORDER)));
    }
    let mut widths = segments
        .iter()
        .map(|(_, value, _)| {
            let cells = u64::try_from(cells).unwrap_or(u64::MAX);
            usize::try_from(u64::from(*value) * cells / total).unwrap_or(0)
        })
        .collect::<Vec<_>>();
    // Largest-remainder allocation ensures the stack always spans its full width.
    let mut remainders = segments
        .iter()
        .enumerate()
        .map(|(index, (_, value, _))| (index, u64::from(*value) * cells as u64 % total))
        .collect::<Vec<_>>();
    remainders.sort_by_key(|(_, remainder)| std::cmp::Reverse(*remainder));
    for (index, _) in remainders
        .into_iter()
        .take(cells.saturating_sub(widths.iter().sum()))
    {
        widths[index] += 1;
    }
    Line::from(
        segments
            .iter()
            .zip(widths)
            .filter(|(_, count)| *count > 0)
            .map(|((_, _, color), count)| Span::styled("█".repeat(count), fg(*color)))
            .collect::<Vec<_>>(),
    )
}

fn context_categories(census: ContextCensus) -> Vec<(&'static str, u32, Color)> {
    [
        ("system prompt", census.system_prompt, theme::DIM),
        ("repository facts", census.repo_facts, theme::THREAD),
        ("user", census.user_items, theme::BLUE),
        ("reasoning", census.reasoning_replay, theme::VIOLET),
        ("assistant", census.assistant_history, theme::IVORY),
        ("tool results", census.tool_results, theme::SAGE),
        ("tool specs", census.tool_specs, theme::AMBER),
    ]
    .into_iter()
    .filter(|(_, tokens, _)| *tokens > 0)
    .collect()
}

fn context_segments(census: ContextCensus, width: u16) -> Vec<Line<'static>> {
    let categories = context_categories(census);
    if categories.is_empty() {
        return vec![muted("no categorized tokens reported")];
    }
    let inner_width = usize::from(width.saturating_sub(2));
    let max_label = categories
        .iter()
        .map(|(label, _, _)| label.len())
        .max()
        .unwrap_or_default();
    let count_width = categories
        .iter()
        .map(|(_, value, _)| format_compact_tokens(u64::from(*value)).len())
        .max()
        .unwrap_or_default();
    let label_width = max_label.min(inner_width.saturating_sub(count_width + 4));
    categories
        .into_iter()
        .map(|(label, tokens, color)| {
            Line::from(vec![
                Span::styled("■ ", fg(color)),
                Span::styled(format!("{label:<label_width$}"), fg(theme::IVORY)),
                Span::styled(
                    format!("{:>count_width$}", format_compact_tokens(u64::from(tokens))),
                    fg(theme::DIM),
                ),
            ])
        })
        .collect()
}

fn event_line(event: &str) -> Line<'static> {
    let color = if event.contains("ToolCompleted") || event.contains("TaskCompleted") {
        theme::SAGE
    } else if event.contains("Failed") || event.contains("Retry") || event.contains("Cancelled") {
        theme::CORAL
    } else if event.contains("Reasoning") {
        theme::VIOLET
    } else if event.contains("Model") {
        theme::BLUE
    } else {
        theme::DIM
    };
    Line::from(Span::styled(event.to_owned(), fg(color)))
}
#[allow(clippy::cast_precision_loss)] // compact display rounds to one decimal anyway
fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
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
        Some(TaskStatus::Paused) => "Awaiting input",
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
        event::EVENT_SCHEMA_VERSION,
        model::{ModelResponse, StopReason, Usage},
        transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId},
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
    fn live_header_falls_back_to_configured_model_before_reporting_unknown() {
        let reducer = Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        model: Some("configured-model"),
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
        assert!(rendered.contains("configured-model"));
        assert!(!rendered.contains("model not reported"));
    }

    #[test]
    fn live_read_file_result_shows_a_bounded_numbered_preview() {
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect the file");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 0,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::TurnStarted { turn: 1 },
            },
            1,
        );
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 1,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: loom_core::kernel::transcript::ToolCallId::try_from(
                        "read-1".to_owned(),
                    )
                    .unwrap(),
                    arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
                },
            },
            1,
        );
        let read_window = concat!(
            "src/lib.rs: 6 lines\n",
            "1\tfn main() {\n",
            "2\t    // comment explaining the next line\n",
            "3\t    first_visible_call()\n",
            "4\t    fourth_line_hidden_when_collapsed()\n",
            "5\t    fifth_line_hidden_when_collapsed()\n",
            "6\t    sixth_line_hidden_when_collapsed()",
        );
        let call_id =
            loom_core::kernel::transcript::ToolCallId::try_from("read-1".to_owned()).unwrap();
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 2,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::ToolCompleted {
                    name: "read_file".into(),
                    call_id,
                    status: loom_core::kernel::transcript::ToolStatus::Success,
                    content_head: read_window.to_owned(),
                },
            },
            2,
        );
        reducer.committed_transcript.push(Item::ToolResult(
            loom_core::kernel::transcript::ToolResult::new(
                loom_core::kernel::transcript::ToolCallId::try_from("read-1".to_owned()).unwrap(),
                loom_core::kernel::transcript::ToolStatus::Success,
                read_window,
                false,
            ),
        ));
        let mut session = loom_core::Session::new(loom_core::SessionId::new());
        session.transcript.clone_from(&reducer.committed_transcript);
        reducer.reconcile_session(&session);
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

        // The window header summarizes the read and the preview keeps the
        // numbered gutter for the first lines.
        assert!(rendered.contains("src/lib.rs: 6 lines"));
        assert!(rendered.contains("1 │ fn main() {"));
        assert!(rendered.contains("2 │     // comment explaining the next line"));
        assert!(rendered.contains("3 │     first_visible_call()"));
        // Lines past the collapsed cap are elided with an honest count.
        assert!(!rendered.contains("fourth_line_hidden_when_collapsed"));
        assert!(rendered.contains("… +3 more lines"));
    }

    #[test]
    fn tool_errors_keep_their_category_and_message() {
        let mut reducer = Reducer::default();
        reducer.begin_task("run the tool");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 0,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::TurnStarted { turn: 1 },
            },
            1,
        );
        // The model requested the tool before it executed: a real
        // ModelCompleted registers the assistant message (and its tool call)
        // in the turn transcript so the branch line renders.
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: 1,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::ModelCompleted {
                    response: ModelResponse {
                        message: AssistantMessage {
                            content: String::new(),
                            reasoning: None,
                            tool_calls: vec![ToolCall {
                                id: ToolCallId::try_from("cmd-1".to_owned()).unwrap(),
                                name: "run_command".to_owned(),
                                arguments: ToolArguments::try_from(serde_json::json!({
                                    "program": "cargo"
                                }))
                                .unwrap(),
                            }],
                            stop_reason: StopReason::ToolCalls,
                        },
                        usage: Usage::default(),
                    },
                },
            },
            900,
        );
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                sequence: 2,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::ToolStarted {
                    name: "run_command".into(),
                    call_id: ToolCallId::try_from("cmd-1".to_owned()).unwrap(),
                    arguments: r#"{"program":"cargo"}"#.to_owned(),
                },
            },
            1000,
        );
        let call_id =
            loom_core::kernel::transcript::ToolCallId::try_from("cmd-1".to_owned()).unwrap();
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 3,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::ToolCompleted {
                    name: "run_command".into(),
                    call_id,
                    status: loom_core::kernel::transcript::ToolStatus::Error(
                        loom_core::kernel::transcript::ToolFailure::Timeout,
                    ),
                    content_head: "timed out after 30s waiting for cargo test".to_owned(),
                },
            },
            1250,
        );
        reducer.committed_transcript.push(Item::ToolResult(
            loom_core::kernel::transcript::ToolResult::new(
                loom_core::kernel::transcript::ToolCallId::try_from("cmd-1".to_owned()).unwrap(),
                loom_core::kernel::transcript::ToolStatus::Error(
                    loom_core::kernel::transcript::ToolFailure::Timeout,
                ),
                "timed out after 30s waiting for cargo test",
                false,
            ),
        ));
        let mut session = loom_core::Session::new(loom_core::SessionId::new());
        session.transcript.clone_from(&reducer.committed_transcript);
        reducer.reconcile_session(&session);
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_test_live(frame, &reducer, "run the tool"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("error · timeout"));
        assert!(rendered.contains("timed out after 30s waiting for cargo test"));
        // Duration stays visible even though the call failed.
        assert!(rendered.contains("250ms"));
        assert!(!rendered.contains("· done"));
    }

    #[test]
    fn streaming_turn_shows_cursor_and_provisional_reasoning() {
        let mut reducer = Reducer::default();
        reducer.begin_task("streaming task");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                sequence: 0,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::TurnStarted { turn: 1 },
            },
            1,
        );
        reducer.provisional.reasoning = "weighing two approaches".to_owned();
        reducer.provisional.text = "partial answer so far".to_owned();
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_test_live(frame, &reducer, "streaming task"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("reasoning · provisional · streaming"));
        assert!(rendered.contains("partial answer so far"));
        assert!(rendered.contains("▌"));
    }

    #[test]
    fn paused_task_awaits_input() {
        let mut reducer = Reducer::default();
        reducer.begin_task("paused task");
        reducer.task.status = Some(TaskStatus::Paused);
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| draw_test_live(frame, &reducer, "paused task"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Awaiting input"));
        assert!(!rendered.contains("Paused"));
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
        assert!(rendered.contains("› ask @src/lib.rs"));
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
            "output_tokens": null,
            "total_tokens": 1300,
            "tool_calls": 2,
            "wall_time": {"secs": 478, "nanos": 0}
        }))
        .unwrap();
        reducer.context_progress = Some(crate::reducer::ContextProgress {
            turn: 3,
            context_budget,
            census,
            remaining_budget,
            elapsed_at_compile: Some(Duration::from_secs(102)),
            tool_calls_used: 7,
        });
        reducer.tool_calls_started = 7;
        let mut terminal = Terminal::new(TestBackend::new(154, 60)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::from_secs(102)),
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
        assert!(text.contains("7 / 9"));
        assert!(text.contains("01:42 / 09:40"), "{text:?}");
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|cell| cell.symbol() == "━")
                .count()
                >= 40,
            "expected tool-call and wall-time budget bars"
        );
    }

    #[test]
    fn runtime_sidebar_keeps_every_section_and_events_visible() {
        let mut reducer = Reducer::default();
        reducer.task.task_id = Some(loom_core::TaskId::new());
        reducer.task.status = Some(loom_core::kernel::session::TaskStatus::Running);
        reducer.event_count = 1284;
        reducer.recent_events = vec![
            "#1284 ReasoningDelta".into(),
            "#1283 ModelRequestStarted".into(),
            "#1282 TurnStarted".into(),
        ];
        // At screenshot-scale terminal heights the sidebar must still retain
        // its EVENTS heading, at least one event, and the counter.
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        elapsed: Some(Duration::from_secs(102)),
                        dropped: Some(0),
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

        for section in ["TASK", "BUDGET", "NEXT REQUEST · CONTEXT", "COST", "EVENTS"] {
            assert!(
                rendered.contains(section),
                "missing {section} section: {rendered}"
            );
        }
        assert!(rendered.contains("#1284 ReasoningDelta"));
        assert!(rendered.contains("events 1284 · dropped 0"));
    }

    #[test]
    fn context_bar_uses_proportional_census_categories_and_legend_colors() {
        let census: ContextCensus = serde_json::from_value(serde_json::json!({
            "system_prompt": 50,
            "repo_facts": 0,
            "user_items": 25,
            "assistant_history": 0,
            "reasoning_replay": 0,
            "tool_results": 25,
            "tool_specs": 0,
            "total": 120
        }))
        .unwrap();

        let bar = context_bar(census, 21);
        let symbols = bar
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let system_style = bar.spans[0].style;
        let user_style = bar.spans[1].style;
        let tools_style = bar.spans[2].style;
        let segments = context_segments(census, 30);

        assert_eq!(symbols.chars().count(), 20);
        assert_eq!(symbols.chars().filter(|symbol| *symbol == '█').count(), 20);
        assert_eq!(
            bar.spans
                .iter()
                .map(|span| span.content.chars().count())
                .collect::<Vec<_>>(),
            [10, 5, 5]
        );
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].spans[0].style, system_style);
        assert_eq!(segments[1].spans[0].style, user_style);
        assert_eq!(segments[2].spans[0].style, tools_style);
        assert!(segments[0].to_string().contains("system prompt"));
        assert!(segments[1].to_string().contains("user"));
        assert!(segments[2].to_string().contains("tool results"));
    }

    #[test]
    fn empty_context_census_reports_unavailable_categories_without_fake_segments() {
        let census = ContextCensus::default();
        assert!(
            context_bar(census, 12)
                .spans
                .iter()
                .all(|span| !span.content.contains('█'))
        );
        assert_eq!(
            context_segments(census, 30)[0].to_string(),
            "no categorized tokens reported"
        );
    }

    #[test]
    fn sidebar_event_offset_reveals_older_events() {
        let mut reducer = Reducer::default();
        reducer.recent_events = (0..8)
            .rev()
            .map(|sequence| format!("#{sequence} TurnStarted"))
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        sidebar_scroll_back: 5,
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
        assert!(rendered.contains("#2 TurnStarted"));
        assert!(!rendered.contains("#7 TurnStarted"));
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
            elapsed_at_compile: None,
            tool_calls_used: 0,
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
        assert!(rendered.contains("unbounded"), "{rendered}");
        assert!(rendered.contains("estimated 0/20000"));
        assert!(!rendered.contains("0 remaining"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .filter(|cell| cell.symbol() == "━")
                .count()
                >= 40,
            "unbounded tool-call and wall-time limits should still show neutral tracks"
        );
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
