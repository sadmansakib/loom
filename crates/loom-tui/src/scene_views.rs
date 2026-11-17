use std::env;
use std::time::Duration;

use loom_core::kernel::session::TaskStatus;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

mod failure_cards;
pub(crate) mod sidebar;
mod thread;
mod thread_live;

use sidebar::{draw_runtime_sidebar, draw_scene_sidebar};
use thread::draw_reducer_thread;

/// Sidebar width in columns for the wide (>=120) and overlay layouts.
pub(crate) const SIDEBAR_WIDTH: u16 = 38;

use crate::{
    glyphs::{self, Glyph, GlyphSet},
    picker::{ModelPricing, PickerState},
    recent::RecentSession,
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
    /// Design spec: palette, glyphs, grid and recipe.
    Spec,
    /// Narrow (under 90 columns) layout.
    Narrow,
    /// `/compact` notice hidden under the `/context` modal.
    CompactA,
    /// The compact notice revealed in the status bar after Esc.
    CompactB,
}

/// Establish the application's ground colour across the whole frame before
/// any widget runs. Ratatui patches styles cell by cell, so a region that no
/// widget touches — or a widget with empty lines / unstyled spans — keeps the
/// terminal's own default background. Ghostty's default differs from our
/// near-black ground, which shows up as grey rectangles. Painting the base
/// first makes every region own its background, in every terminal.
pub(crate) fn paint_base(frame: &mut Frame<'_>) {
    frame.render_widget(
        Block::default().style(Style::default().bg(adapt_color(theme::BG))),
        frame.area(),
    );
}

/// Render a `--scene` route without inventing task history or runtime metrics.
pub fn draw(frame: &mut Frame<'_>, scene: Scene) {
    paint_base(frame);
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
        Scene::Spec => crate::wireframe_scenes::draw_spec(frame),
        Scene::Narrow => crate::wireframe_scenes::draw_narrow(frame),
        Scene::CompactA => crate::wireframe_scenes::draw_compact_a(frame),
        Scene::CompactB => crate::wireframe_scenes::draw_compact_b(frame),
    }
}

/// Defaults and permissions shown on the idle sidebar. The TUI does not
/// invent these: the app loop fills what the engine config exposes, and the
/// rest comes from the resolved policy.
///
/// TODO(loom-cli): fill the tool permission modes from the resolved trust /
/// approval policy once `EngineConfig` exposes it. Until then they default to
/// the effective TUI behaviour: reads allowed, writes and shell ask.
#[derive(Debug, Clone, Copy)]
pub struct IdleFacts {
    pub tool_call_budget: Option<u32>,
    pub wall_budget: Option<Duration>,
    pub read_permission: &'static str,
    pub write_permission: &'static str,
    pub shell_permission: &'static str,
}

impl Default for IdleFacts {
    fn default() -> Self {
        Self {
            tool_call_budget: None,
            wall_budget: None,
            read_permission: "allowed",
            write_permission: "ask",
            shell_permission: "ask",
        }
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
    /// Task mode shown under TASK, e.g. `read-only`.
    pub task_mode: Option<&'a str>,
    /// Human label shown under TASK, e.g. `benchmark B1`.
    pub task_label: Option<&'a str>,
    /// Billing period label, `off-peak` or `peak`.
    pub billing_period: Option<&'a str>,
    pub turn_limit: Option<u32>,
    pub thread_scroll_back: usize,
    pub sidebar_scroll_back: usize,
    pub file_suggestions: &'a [String],
    pub recent: &'a [RecentSession],
    /// Write awaiting the operator; drawn as an amber box over the thread.
    pub approval: Option<&'a crate::approval::Request>,
    /// One-line status message (`/export` result, `/compact` note).
    pub notice: Option<&'a str>,
    /// Highlighted row while the `/` command palette is open.
    pub palette_selected: usize,
    pub pricing: Option<ModelPricing>,
    pub scene: Option<Scene>,
    /// Idle budget and permission summary; defaults are the TUI policy.
    pub idle: IdleFacts,
    /// Test/static override. `None` reads `LOOM_ASCII` at draw time.
    pub glyph_set: Option<GlyphSet>,
    /// Test/static override. `None` renders truecolor.
    pub depth: Option<theme::Depth>,
    /// Skip the composer cursor when an overlay modal owns the frame.
    pub hide_cursor: bool,
}

#[allow(clippy::large_types_passed_by_value)] // one options value per frame
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
#[allow(clippy::large_types_passed_by_value)]
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
        recent,
        approval,
        ..
    } = options;
    set_depth(options.depth.unwrap_or(theme::Depth::TrueColor));
    let set = options.glyph_set.unwrap_or_else(glyph_set);
    paint_base(frame);
    let area = frame.area();
    let compact = area.width < 90;
    let wide = area.width >= 120;
    let prompt_label = if approval.is_some() {
        "Task paused. Answer the prompt above, or type to queue a follow-up."
    } else if reducer.task.status == Some(TaskStatus::Running) {
        "Task running. Type to queue a follow-up."
    } else {
        "Describe a task…"
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
    // The `/` palette replaces the @file suggestion slot while open.
    let palette_open = crate::palette::is_open(input);
    let suggestion_height = if palette_open {
        u16::try_from(crate::palette::matching(input).len().min(9) + 3).unwrap_or(12)
    } else if file_suggestions.is_empty() {
        0
    } else {
        u16::try_from(file_suggestions.len().min(5)).unwrap_or(5) + 2
    };
    let header_height = 1;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(5),
            Constraint::Length(
                input_rows + suggestion_height + 4 + u16::from(options.notice.is_some()),
            ),
        ])
        .split(area);
    draw_live_header(frame, rows[0], reducer, scene, &options, set);
    let footer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(
                suggestion_height.min(rows[2].height.saturating_sub(input_rows + 4)),
            ),
            Constraint::Length(input_rows + 2),
            Constraint::Length(1),
            Constraint::Length(u16::from(options.notice.is_some())),
            Constraint::Min(0),
        ])
        .split(rows[2]);
    let show_sidebar = wide || (area.width >= 90 && sidebar_visible);
    let body = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(65), Constraint::Length(SIDEBAR_WIDTH)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(rows[1])
    };
    // The wireframes inset the thread, composer and footer by one text
    // margin; full-bleed chrome is the one thing that reads as unfinished.
    let gutter = |area: Rect| Rect {
        x: area.x.saturating_add(2),
        width: area.width.saturating_sub(4),
        ..area
    };
    let panel_height = if reducer.is_retrying() && body[0].height >= 9 {
        4
    } else {
        0
    };
    let failure_height = if reducer.failure.is_some() && body[0].height >= 14 {
        if body[0].width >= 120 { 7 } else { 9 }
    } else {
        0
    };
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(panel_height),
            Constraint::Length(failure_height),
        ])
        .split(body[0]);
    let thread_area = gutter(main[0]);
    if reducer.execution_turns.is_empty()
        && reducer.original_instruction.is_none()
        && reducer.task.display_prompts.is_empty()
        && !reducer.restored
        && scene.is_none()
    {
        draw_idle_home(frame, thread_area, recent, set);
    } else {
        draw_reducer_thread(
            frame,
            thread_area,
            reducer,
            compact,
            thread_scroll_back,
            scene,
            options.approval.map(|request| request.path.as_str()),
        );
    }
    if panel_height > 0 {
        draw_retry_panel(frame, gutter(main[1]));
    }
    if failure_height > 0
        && let Some(failure) = &reducer.failure
    {
        failure_cards::draw(frame, gutter(main[2]), failure);
    }
    let composer = gutter(footer[1]);
    let (cursor_x, cursor_y, prompt_scroll) = prompt_cursor(input, cursor, composer);
    if let Some(request) = options.approval {
        draw_approval_box(frame, thread_area, request);
    }
    if !options.hide_cursor {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    let mut prompt_spans = vec![
        Span::styled(
            format!("{} ", glyphs::glyph(Glyph::Prompt, set)),
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt_display,
            if input.is_empty() {
                fg(theme::DIM)
            } else {
                fg(theme::IVORY).add_modifier(Modifier::BOLD)
            },
        ),
    ];
    let show_queue = reducer.task.status == Some(TaskStatus::Running);
    if show_queue {
        // Right-align the queue count on the composer's first line.
        let queued = format!("queued {queue_count}");
        let first_line = prompt_display.lines().next().unwrap_or("").chars().count();
        let inner = usize::from(composer.width.saturating_sub(2));
        let pad = inner.saturating_sub(first_line + 2 + queued.chars().count());
        prompt_spans.push(Span::raw(" ".repeat(pad)));
        prompt_spans.push(Span::styled(queued, fg(theme::DIM)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(prompt_spans))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(fg(theme::AMBER)),
            )
            .style(Style::default().bg(theme::BG))
            .scroll((prompt_scroll, 0))
            .wrap(Wrap { trim: false }),
        composer,
    );
    if palette_open && footer[0].height > 0 {
        draw_palette(frame, footer[0], input, options.palette_selected, set);
    } else if !file_suggestions.is_empty() && footer[0].height > 0 {
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
            footer[0],
        );
    }
    let footer_hint = if scene.is_some() {
        key_hint_line(&[
            ("Enter", "start task"),
            ("Tab", "@file"),
            ("F2", "models"),
            ("F1", "help"),
        ])
    } else if compact {
        // Narrow keeps one hint only; the counters share the status row.
        key_hint_line(&[("/help", "help")])
    } else if reducer.is_retrying() {
        key_hint_line(&[
            ("Esc", "cancel"),
            ("/retry", "retry now"),
            ("^O", "expand"),
            ("/help", "help"),
        ])
    } else if reducer.failure.is_some() {
        key_hint_line(&[("/retry", "retry"), ("^O", "expand"), ("/help", "help")])
    } else if reducer.task.status == Some(TaskStatus::Running) {
        let mut hints = vec![
            ("Esc", "cancel"),
            ("^O", "expand"),
            ("/reasoning", "reasoning"),
            ("/sidebar", "sidebar"),
        ];
        if thread_scroll_back > 0 || reducer.selected.is_some() {
            hints.push(("^End", "latest"));
        }
        hints.push(("/help", "help"));
        key_hint_line(&hints)
    } else {
        key_hint_line(&[
            ("Enter", "send"),
            ("/", "commands"),
            ("/help", "help"),
            ("/new", "new session"),
        ])
    };
    let footer_area = gutter(footer[2]);
    let (counters, counter_color) = if scene.is_some() {
        (String::new(), theme::DIM)
    } else {
        match dropped {
            Some(dropped) if dropped > 0 => (
                format!(
                    "events {} · dropped {dropped} · trace incomplete",
                    reducer.event_count
                ),
                theme::CORAL,
            ),
            Some(dropped) => (
                format!("events {} · dropped {dropped}", reducer.event_count),
                theme::DIM,
            ),
            // Idle has no sink yet; zero events is a fact, not missing data.
            None if reducer.task.status.is_none() => (
                format!("events {} · dropped 0", reducer.event_count),
                theme::DIM,
            ),
            None => ("events unavailable".to_owned(), theme::DIM),
        }
    };
    let counter_width = u16::try_from(counters.chars().count()).unwrap_or(u16::MAX);
    let footer_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(counter_width.min(footer_area.width)),
        ])
        .split(footer_area);
    frame.render_widget(Paragraph::new(footer_hint), footer_columns[0]);
    if !counters.is_empty() {
        frame.render_widget(
            Paragraph::new(counters)
                .style(fg(counter_color))
                .alignment(Alignment::Right),
            footer_columns[1],
        );
    }
    if let Some(notice) = options.notice {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("· ", fg(theme::AMBER)),
                Span::styled(notice, fg(theme::IVORY)),
            ])),
            gutter(footer[3]),
        );
    }
    let draw_side = |frame: &mut Frame<'_>, sidebar: Rect| match scene {
        None => {
            draw_runtime_sidebar(frame, sidebar, reducer, elapsed, pricing, &options);
        }
        Some(scene) => {
            draw_scene_sidebar(frame, sidebar, reducer, elapsed, scene, options.turn_limit);
        }
    };
    if wide {
        draw_side(frame, body[1]);
    } else if show_sidebar {
        let overlay = Rect::new(
            area.x + area.width.saturating_sub(SIDEBAR_WIDTH),
            rows[1].y,
            area.width.min(SIDEBAR_WIDTH),
            rows[1].height,
        );
        // The medium-width sidebar floats over the thread: clear the
        // cells first or the text underneath shows through the gaps.
        frame.render_widget(ratatui::widgets::Clear, overlay);
        frame.render_widget(
            ratatui::widgets::Block::default().style(Style::default().bg(theme::BG)),
            overlay,
        );
        draw_side(frame, overlay);
    }
}

fn prompt_cursor(input: &str, cursor: usize, area: Rect) -> (u16, u16, u16) {
    let inner_x = area.x.saturating_add(1);
    let inner_y = area.y.saturating_add(1);
    let width = area.width.saturating_sub(2).max(1) as usize;
    let before = &input[..cursor.min(input.len())];
    // The prompt glyph and its trailing space occupy the first two cells.
    let (mut row, mut column) = (0_usize, 2_usize.min(width));
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
    paint_base(frame);
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
            .constraints([Constraint::Min(65), Constraint::Length(SIDEBAR_WIDTH)])
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
                    body(&format!(
                        "effort    {}",
                        if choice.efforts.is_empty() {
                            "not reported"
                        } else {
                            picker.effort.as_str()
                        }
                    )),
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
    draw_header(frame, rows[0], "keys · commands · events · crate seam");
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(54), Constraint::Min(1)])
        .split(rows[1]);
    let mut left = Vec::new();
    for section in crate::keys::KEYMAP {
        left.push(heading(section.title));
        left.extend(
            section
                .rows
                .iter()
                .map(|row| help_key(row.keys, row.action)),
        );
    }
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
                format!("{event:<36}"),
                fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(meaning, fg(theme::IVORY)),
        ])
    }));
    right.push(muted(""));
    right.push(heading("COMMANDS"));
    right.push(muted("Type / to filter; Enter runs the selected command."));
    right.extend(command_rows(columns[1].width));
    right.push(muted(""));
    right.push(heading("DESIGN REFERENCE"));
    right.push(muted(
        "Static --scene views show no task data; live values come from events and session.",
    ));
    right.push(muted(""));
    right.push(heading("CRATE SEAM"));
    right.push(body(
        "loom-cli → Engine::run_task → EventSink → channel → reducer → Ratatui",
    ));
    right.push(muted(
        "Session.transcript is authoritative; events are observations.",
    ));
    render_panel(frame, columns[1], "events · commands", right);
}

fn draw_idle_home(frame: &mut Frame<'_>, area: Rect, recent: &[RecentSession], set: GlyphSet) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let brand = format!("{} loom", glyphs::glyph(Glyph::Brand, set));
    let brand_rows = area.height.min(8);
    let brand_area = Rect {
        height: brand_rows,
        ..area
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                brand,
                fg(theme::AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Describe a task. It runs as one thread.",
                fg(theme::DIM),
            )),
        ])
        .alignment(Alignment::Center),
        brand_area,
    );
    let list_y = area.y.saturating_add(6);
    if list_y >= area.y.saturating_add(area.height) {
        return;
    }
    let list_area = Rect {
        y: list_y,
        height: area.height.saturating_sub(6),
        ..area
    };
    let width = usize::from(list_area.width);
    // Section labels are uppercase and dim. A terminal cell cannot
    // letter-space, so uppercase plus dim carries the label instead.
    let mut lines = vec![Line::from(Span::styled("RECENT", fg(theme::DIM)))];
    if recent.is_empty() {
        lines.push(Line::from(Span::styled(
            "no past sessions yet",
            fg(theme::DIM),
        )));
    }
    let prompt = glyphs::glyph(Glyph::Prompt, set);
    for session in recent.iter().take(3) {
        let status = match session.status {
            Some(TaskStatus::Completed) => glyphs::glyph(Glyph::Check, set),
            Some(TaskStatus::Failed) => glyphs::glyph(Glyph::Cross, set),
            Some(TaskStatus::Cancelled) => glyphs::glyph(Glyph::Cancel, set),
            _ => glyphs::glyph(Glyph::Pending, set),
        };
        // Outcome is metadata: glyph + word, dim, never colour alone.
        let tail = format!("{status} {} turns", session.turns);
        let room = width.saturating_sub(tail.chars().count() + 4).max(8);
        let title = thread::truncate_chars(&session.title, room);
        let gap = width
            .saturating_sub(2 + title.chars().count() + tail.chars().count())
            .max(1);
        lines.push(Line::from(vec![
            Span::styled(format!("{prompt} "), fg(theme::AMBER)),
            Span::styled(title, fg(theme::IVORY)),
            Span::raw(" ".repeat(gap)),
            Span::styled(tail, fg(theme::DIM)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("TRY", fg(theme::DIM))));
    lines.push(Line::from(Span::styled(
        "Explain this crate · Find where X is defined · Add a test for Y",
        fg(theme::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Amber box for the backoff wait; the countdown itself lives in the thread.
fn draw_retry_panel(frame: &mut Frame<'_>, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let hints = Line::from(vec![
        Span::styled("[Esc]", fg(theme::IVORY)),
        Span::styled(" cancel now  ·  ", fg(theme::DIM)),
        Span::styled("[/retry]", fg(theme::IVORY)),
        Span::styled(" retry immediately", fg(theme::DIM)),
    ]);
    let mut lines = vec![Line::from(Span::styled(
        "Waiting to retry. Your task is still running.",
        fg(theme::AMBER).add_modifier(Modifier::BOLD),
    ))];
    if area.height >= 5 {
        lines.push(Line::from(Span::styled(
            "The failed attempt was discarded; nothing from it can be committed.",
            fg(theme::DIM),
        )));
    }
    lines.push(hints);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(fg(theme::AMBER)),
            )
            .style(Style::default().bg(theme::BG))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_approval_box(frame: &mut Frame<'_>, area: Rect, request: &crate::approval::Request) {
    // Inline at the bottom of the thread, like the wireframe: the paused
    // task, its prose and the pending tool row stay readable above it.
    let body: Vec<&str> = request.content.lines().collect();
    let mut lines = vec![
        Line::from(Span::styled(
            "◇ Allow this write? The task is paused, not failed.",
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("  ", fg(theme::DIM)),
            Span::styled(request.path.clone(), fg(theme::BLUE)),
            Span::styled(format!("   +{} −0", body.len()), fg(theme::SAGE)),
        ]),
    ];
    if !body.is_empty() {
        // A proposal is a whole-file insert: every line is an addition.
        let name = request
            .path
            .rsplit('/')
            .next()
            .unwrap_or(request.path.as_str());
        lines.push(Line::from(Span::styled(
            format!("  @@ {name} · {} lines", body.len()),
            fg(theme::DIM),
        )));
        let shown: Vec<&str> = body
            .iter()
            .filter(|line| !line.trim().is_empty())
            .take(2)
            .copied()
            .collect();
        for line in &shown {
            lines.push(Line::from(vec![
                Span::styled("  + ", fg(theme::SAGE)),
                Span::styled(
                    thread::truncate_chars(line, usize::from(area.width).saturating_sub(8)),
                    fg(theme::SAGE),
                ),
            ]));
        }
        let non_empty = body.iter().filter(|line| !line.trim().is_empty()).count();
        if non_empty > shown.len() {
            lines.push(Line::from(Span::styled(
                format!("  … {} more lines · ^O expand", non_empty - shown.len()),
                fg(theme::DIM),
            )));
        }
    }
    lines.push(Line::from(Span::styled(
        "  Writes touch your working tree. Reads never ask.",
        fg(theme::DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  [y] once   [a] session   [n] deny   [e] edit first",
        fg(theme::IVORY),
    )));
    let available = usize::from(area.height.saturating_sub(2)).max(1);
    // When the thread pane is short, drop the diff before the answer keys:
    // the operator must always see how to answer.
    while lines.len() > available && lines.len() > 3 {
        lines.remove(2);
    }
    if lines.len() > available {
        lines.truncate(available.max(2));
        let keys = Line::from(Span::styled(
            "  [y] allow once   [a] allow for session   [n] deny   [e] edit",
            fg(theme::IVORY),
        ));
        if available > 1 {
            let last = lines.len().saturating_sub(1);
            lines[last] = keys;
        }
    }
    let height = area
        .height
        .min(u16::try_from(lines.len() + 2).unwrap_or(area.height));
    let box_area = Rect {
        y: area.y + area.height.saturating_sub(height),
        height,
        ..area
    };
    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(fg(theme::AMBER)),
            )
            .style(Style::default().bg(theme::BG)),
        box_area,
    );
}

/// Filtered `/` command list anchored over the composer. Selection is
/// highlighted in amber; meaning is carried by the `❯` marker as well.
fn draw_palette(frame: &mut Frame<'_>, area: Rect, input: &str, selected: usize, set: GlyphSet) {
    let matches = crate::palette::matching(input);
    let prompt = glyphs::glyph(Glyph::Prompt, set);
    // Keep the selected command on screen when the list is taller than its slot.
    let rows = usize::from(area.height.saturating_sub(3)).max(1);
    let start = if matches.len() > rows {
        selected.saturating_sub(rows - 1).min(matches.len() - rows)
    } else {
        0
    };
    let mut lines: Vec<Line<'_>> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(index, command)| {
            let selected_row = index == selected;
            let marker = if selected_row {
                format!("{prompt} ")
            } else {
                "  ".to_owned()
            };
            Line::from(vec![
                Span::styled(
                    format!("{marker}{}", command.name),
                    fg(if selected_row {
                        theme::AMBER
                    } else {
                        theme::IVORY
                    })
                    .add_modifier(if selected_row {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(format!("   {}", command.blurb), fg(theme::DIM)),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no matching command",
            fg(theme::DIM),
        )));
    }
    lines.push(Line::from(Span::styled(
        " ↑↓ move · Enter run · Esc close",
        fg(theme::DIM),
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::BG))
            .block(
                Block::default()
                    .title(" commands ")
                    .borders(Borders::ALL)
                    .border_style(fg(theme::AMBER)),
            ),
        area,
    );
}

#[allow(clippy::too_many_lines)] // one header composition for wide and narrow
fn draw_live_header(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    scene: Option<Scene>,
    options: &LiveViewOptions<'_>,
    set: GlyphSet,
) {
    // One-row header bar. Paint the bar first so every cell owns the shade.
    frame.render_widget(
        Block::default().style(Style::default().bg(adapt_color(theme::HEADER))),
        area,
    );
    if area.width < 4 {
        return;
    }
    let content = Rect {
        x: area.x.saturating_add(2),
        width: area.width.saturating_sub(4),
        height: 1,
        ..area
    };
    let brand = format!("{} loom", glyphs::glyph(Glyph::Brand, set));
    let model = if scene.is_some() {
        "model not available in static preview"
    } else {
        reducer
            .observed_model
            .as_deref()
            .or(options.model)
            .unwrap_or("model not reported")
    };
    let workspace = options.workspace.unwrap_or("workspace unavailable");
    let session = options.session_id.map_or_else(
        || "new".to_owned(),
        |id| id.chars().take(8).collect::<String>(),
    );

    // Narrow: one status line carries state, turn and cost instead of the
    // full header. The brand stays visible for orientation.
    if scene.is_none() && area.width < 90 {
        let (_, label, color) = live_header_state(reducer, options.approval.is_some(), set)
            .unwrap_or_else(|| {
                (
                    glyphs::glyph(Glyph::Spinner, set).to_owned(),
                    "running".to_owned(),
                    theme::AMBER,
                )
            });
        let turn = reducer.turn.unwrap_or(0);
        let cost = if reducer.usage_reported {
            pricing_label(pricing_value(options.pricing, reducer))
        } else {
            "—".to_owned()
        };
        let status = format!("{label} · turn {turn} · cost {cost}");
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(9), Constraint::Min(0)])
            .split(content);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{} ", glyphs::glyph(Glyph::Brand, set)),
                    header_fg(theme::AMBER).add_modifier(Modifier::BOLD),
                ),
                Span::styled("loom", header_fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            ]))
            .style(Style::default().bg(adapt_color(theme::HEADER))),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(status)
                .style(header_fg(color))
                .alignment(Alignment::Right),
            columns[1],
        );
        return;
    }

    let state = if scene.is_some() {
        None
    } else {
        live_header_state(reducer, options.approval.is_some(), set)
    };
    let state_text = state.as_ref().map(|(glyph, label, _)| {
        if glyph.is_empty() {
            label.to_owned()
        } else {
            format!("{glyph} {label}")
        }
    });
    let effort = options.effort.unwrap_or("not reported");
    let thinking = options.thinking.unwrap_or("not reported");
    let model = thread::truncate_chars(model, 24);

    let model_len = model.chars().count();
    let state_len = state_text.as_ref().map_or(0, |text| text.chars().count());
    let effort_len = 7 + effort.chars().count(); // "effort "
    let thinking_len = 9 + thinking.chars().count(); // "thinking "
    let mut right_len = state_len + 3 + model_len;
    let room_for_right = usize::from(content.width).saturating_sub(20);
    let show_effort = right_len + 3 + effort_len <= room_for_right;
    if show_effort {
        right_len += 3 + effort_len;
    }
    let show_thinking = show_effort && right_len + 3 + thinking_len <= room_for_right;
    if show_thinking {
        right_len += 3 + thinking_len;
    }
    let right_width = u16::try_from(right_len)
        .unwrap_or(u16::MAX)
        .min(content.width);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_width)])
        .split(content);
    let left_width = usize::from(columns[0].width);

    let left = if scene.is_some() {
        Line::from(vec![
            Span::styled(brand, header_fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("  · preview · no live data", header_fg(theme::DIM)),
        ])
    } else {
        let mode = options
            .task_mode
            .map_or_default(|mode| format!(" · {mode}"));
        let meta = format!("{workspace} · session {session}{mode}");
        let room = left_width.saturating_sub(brand.chars().count() + 3);
        let meta = thread::truncate_chars(&meta, room.max(8));
        Line::from(vec![
            Span::styled(brand, header_fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled(format!("   {meta}"), header_fg(theme::DIM)),
        ])
    };

    let mut right_spans = Vec::new();
    if let Some((_, _, color)) = &state
        && let Some(state_text) = state_text
    {
        right_spans.push(Span::styled(
            state_text,
            header_fg(*color).add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::raw("   "));
    }
    right_spans.push(Span::styled(model, header_fg(theme::BLUE)));
    if show_effort {
        right_spans.push(Span::styled("   effort ", header_fg(theme::DIM)));
        right_spans.push(Span::styled(
            effort,
            header_fg(theme::IVORY).add_modifier(Modifier::BOLD),
        ));
    }
    if show_thinking {
        right_spans.push(Span::styled("   thinking ", header_fg(theme::DIM)));
        right_spans.push(Span::styled(
            thinking,
            header_fg(theme::IVORY).add_modifier(Modifier::BOLD),
        ));
    }
    let right = Line::from(right_spans);

    frame.render_widget(Paragraph::new(left), columns[0]);
    frame.render_widget(
        Paragraph::new(right)
            .style(Style::default().bg(adapt_color(theme::HEADER)))
            .alignment(Alignment::Right),
        columns[1],
    );
}

/// `$0.0000` when both usage and pricing are known, otherwise `—`.
fn pricing_value(pricing: Option<ModelPricing>, reducer: &Reducer) -> Option<f64> {
    pricing.and_then(|pricing| {
        pricing.estimate(
            reducer
                .usage
                .input_tokens
                .saturating_add(reducer.usage.cached_input_tokens.unwrap_or_default()),
            reducer.usage.output_tokens,
        )
    })
}

fn pricing_label(cost: Option<f64>) -> String {
    cost.map_or_else(|| "—".to_owned(), |cost| format!("${cost:.4}"))
}

/// Glyph + word + color: the design rule that state is never color alone.
fn header_state(
    reducer: &Reducer,
    approval_pending: bool,
    set: GlyphSet,
) -> (&'static str, &'static str, Color) {
    if approval_pending || reducer.task.status == Some(TaskStatus::Paused) {
        return (glyphs::glyph(Glyph::Needs, set), "needs you", theme::AMBER);
    }
    match reducer.task.status {
        Some(TaskStatus::Running) => (glyphs::glyph(Glyph::Spinner, set), "running", theme::AMBER),
        Some(TaskStatus::Completed) => (glyphs::glyph(Glyph::Check, set), "done", theme::SAGE),
        Some(TaskStatus::Failed) => (glyphs::glyph(Glyph::Cross, set), "failed", theme::CORAL),
        Some(TaskStatus::Cancelled) => (glyphs::glyph(Glyph::Cancel, set), "cancelled", theme::DIM),
        // Idle is quiet: the word carries the meaning, no glyph needed.
        _ => ("", "idle", theme::DIM),
    }
}

/// The header chip is reserved for anything not plainly running; a live
/// model request borrows it to show the attempt (mapping in the design notes).
fn live_header_state(
    reducer: &Reducer,
    approval_pending: bool,
    set: GlyphSet,
) -> Option<(String, String, Color)> {
    if approval_pending
        || reducer.task.status == Some(TaskStatus::Paused)
        || reducer.task.status != Some(TaskStatus::Running)
    {
        let (glyph, label, color) = header_state(reducer, approval_pending, set);
        return Some((glyph.to_owned(), label.to_owned(), color));
    }
    if let Some(wait) = &reducer.retry_wait {
        return Some((
            glyphs::glyph(Glyph::Retry, set).to_owned(),
            format!(
                "retrying {} / {}",
                wait.next_attempt, reducer.retry_policy.max_attempts
            ),
            theme::AMBER,
        ));
    }
    reducer.provisional.attempt.map(|attempt| {
        (
            glyphs::glyph(Glyph::Spinner, set).to_owned(),
            format!("streaming · attempt {attempt}"),
            theme::AMBER,
        )
    })
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

fn command_rows(width: u16) -> Vec<Line<'static>> {
    const COLUMN_WIDTH: usize = 18;
    let inner = usize::from(width.saturating_sub(2));
    let columns = (inner / COLUMN_WIDTH).clamp(2, 3);
    crate::palette::COMMANDS
        .chunks(columns)
        .map(|group| {
            Line::from(
                (0..columns)
                    .map(|index| {
                        let name = group.get(index).map_or("", |command| command.name);
                        Span::styled(format!("{name:<COLUMN_WIDTH$}"), fg(theme::AMBER))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn design_area(area: Rect) -> Rect {
    area
}

/// `[key] label` pairs in the wireframes' footer style: key ivory, label dim.
fn key_hint_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            fg(theme::IVORY)
                .bg(adapt_color(theme::PANEL))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), fg(theme::DIM)));
    }
    Line::from(spans)
}

fn heading(text: &str) -> Line<'static> {
    // Section labels are quiet chrome; amber is reserved for live state.
    Line::from(Span::styled(text.to_owned(), fg(theme::DIM)))
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

#[allow(clippy::cast_precision_loss)] // compact display rounds to one decimal anyway
fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
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

/// State glyph alone, for panels that print the word beside it.
fn status_glyph(reducer: &Reducer) -> &'static str {
    let set = glyph_set();
    match reducer.task.status {
        Some(TaskStatus::Running) => glyphs::glyph(Glyph::Spinner, set),
        Some(TaskStatus::Completed) => glyphs::glyph(Glyph::Check, set),
        Some(TaskStatus::Cancelled) => glyphs::glyph(Glyph::Cancel, set),
        Some(TaskStatus::Failed) => glyphs::glyph(Glyph::Cross, set),
        Some(TaskStatus::Paused) => glyphs::glyph(Glyph::Needs, set),
        _ => glyphs::glyph(Glyph::Pending, set),
    }
}
fn fg(color: Color) -> Style {
    Style::default()
        .fg(adapt_color(color))
        .bg(adapt_color(theme::BG))
}
/// Header spans sit on the header bar, not the ground.
fn header_fg(color: Color) -> Style {
    Style::default()
        .fg(adapt_color(color))
        .bg(adapt_color(theme::HEADER))
}
pub(crate) fn glyph_set() -> GlyphSet {
    if env::var_os("LOOM_ASCII").is_some_and(|value| value == "1") {
        GlyphSet::Ascii
    } else {
        GlyphSet::Unicode
    }
}
thread_local! {
    /// Set once per draw from [`LiveViewOptions::depth`]. Rendering stays a
    /// pure function of state plus this capability value.
    static DEPTH: std::cell::Cell<theme::Depth> =
        const { std::cell::Cell::new(theme::Depth::TrueColor) };
}
pub(crate) fn set_depth(depth: theme::Depth) {
    DEPTH.with(|cell| cell.set(depth));
}
fn adapt_color(color: Color) -> Color {
    DEPTH.with(|cell| theme::adapt(color, cell.get()))
}

#[cfg(test)]
mod tests {
    use super::sidebar::*;
    use super::thread::*;
    use super::*;
    use loom_core::EventKind;
    use loom_core::kernel::transcript::{Item, ToolStatus};
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

    /// The live view must own its background on every cell. If any region is
    /// left with the default background, terminals whose own background
    /// differs from ours (notably Ghostty) paint it grey — the reported
    /// "grey rectangle" corruption. This guards that root cause directly.
    #[test]
    fn live_frame_owns_its_background_everywhere() {
        let reducer = crate::reducer::Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(frame, &reducer, "", 0, LiveViewOptions::default());
            })
            .unwrap();
        for cell in terminal.backend().buffer().content() {
            assert_ne!(
                cell.bg,
                Color::Reset,
                "a cell fell back to the terminal default background"
            );
        }
    }

    #[test]
    fn live_sidebar_keeps_a_two_cell_right_margin() {
        let reducer = crate::reducer::Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(frame, &reducer, "", 0, LiveViewOptions::default());
            })
            .unwrap();
        for y in 0..40 {
            for x in [118, 119] {
                assert_eq!(
                    terminal.backend().buffer().cell((x, y)).unwrap().symbol(),
                    " ",
                    "column {x} row {y} still reaches the terminal edge"
                );
            }
        }
    }

    #[test]
    fn restored_history_renders_run_boundaries_and_exact_turns() {
        use crate::reducer::{ExecutionTurn, RestoredRun};

        let mut reducer = Reducer::default();
        reducer.restored = true;
        reducer.task.status = Some(TaskStatus::Completed);
        for (key, run, prompt) in [(1, 1, "first task"), (2, 2, "second task")] {
            reducer.execution_turns.insert(
                key,
                ExecutionTurn {
                    number: key,
                    engine_turn: Some(1),
                    run: Some(run),
                    user_inputs: vec![prompt.into()],
                    transcript: Vec::new(),
                },
            );
            reducer.restored_runs.insert(
                run,
                RestoredRun {
                    status: Some(TaskStatus::Completed),
                    turns: 1,
                    ..RestoredRun::default()
                },
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(frame, &reducer, "", 0, LiveViewOptions::default());
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("run 1 · completed"));
        assert!(rendered.contains("run 2 · completed"));
        assert_eq!(rendered.matches("turn 1").count(), 2);
    }

    #[test]
    fn static_scenes_snapshot_text_and_styles_without_runtime_data() {
        use std::{fmt::Write as _, fs, path::PathBuf};

        let scenes = [
            ("main", Scene::Main),
            ("cancel", Scene::Cancel),
            ("retry", Scene::Retry),
            ("complete", Scene::Complete),
            ("picker", Scene::Picker),
            ("help", Scene::Help),
            ("spec", Scene::Spec),
            ("narrow", Scene::Narrow),
            ("compact-a", Scene::CompactA),
            ("compact-b", Scene::CompactB),
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

    #[test]
    fn help_lists_every_palette_command() {
        let buffer = render(Scene::Help);
        let text = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for command in crate::palette::COMMANDS {
            assert!(
                text.contains(command.name),
                "help is missing {}",
                command.name
            );
        }
    }

    #[test]
    fn help_lists_every_keyboard_binding() {
        let buffer = render(Scene::Help);
        let text = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for section in crate::keys::KEYMAP {
            assert!(
                text.contains(section.title),
                "help is missing section {}",
                section.title
            );
            for row in section.rows {
                assert!(text.contains(row.action), "help is missing {}", row.action);
            }
        }
    }

    #[test]
    fn idle_screen_snapshots_at_three_widths_in_both_glyph_modes() {
        use std::{fmt::Write as _, fs, path::PathBuf};

        let recent = [
            crate::recent::RecentSession {
                title: "Read every module that touches the event sink and report the call chain"
                    .to_owned(),
                turns: 5,
                status: Some(TaskStatus::Completed),
            },
            crate::recent::RecentSession {
                title: "Add a regression test for the retry loop".to_owned(),
                turns: 4,
                status: Some(TaskStatus::Cancelled),
            },
        ];
        let mut actual = String::new();
        for (width, height) in [(140, 40), (100, 30), (70, 24)] {
            for (mode, set) in [("unicode", GlyphSet::Unicode), ("ascii", GlyphSet::Ascii)] {
                for (state, input) in [("closed", ""), ("open", "/")] {
                    let _ = writeln!(actual, "=== {width}x{height} {mode} {state} ===");
                    let options = LiveViewOptions {
                        workspace: Some("~/work/loom"),
                        session_id: Some("0194a7e2-1f9c-7c3e-9b2a-4d5e6f7a8b9c"),
                        model: Some("deepseek-flash"),
                        effort: Some("high"),
                        thinking: Some("on"),
                        turn_limit: Some(50),
                        idle: IdleFacts {
                            tool_call_budget: Some(200),
                            wall_budget: Some(Duration::from_secs(600)),
                            ..IdleFacts::default()
                        },
                        recent: &recent,
                        glyph_set: Some(set),
                        depth: Some(theme::Depth::TrueColor),
                        ..LiveViewOptions::default()
                    };
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            draw_live(frame, &Reducer::default(), input, 0, options);
                        })
                        .unwrap();
                    actual.push_str(&snapshot(terminal.backend().buffer()));
                }
            }
        }

        if env::var_os("UPDATE_TUI_SNAPSHOTS").is_some() {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/idle.snap");
            fs::write(path, &actual).unwrap();
        }
        assert_eq!(actual, include_str!("../tests/snapshots/idle.snap"));
    }

    fn idle_options() -> LiveViewOptions<'static> {
        LiveViewOptions {
            workspace: Some("~/work/loom"),
            session_id: Some("0194a7e2-1f9c-7c3e-9b2a-4d5e6f7a8b9c"),
            model: Some("deepseek-flash"),
            effort: Some("high"),
            thinking: Some("on"),
            turn_limit: Some(50),
            idle: IdleFacts {
                tool_call_budget: Some(200),
                wall_budget: Some(Duration::from_secs(600)),
                ..IdleFacts::default()
            },
            glyph_set: Some(GlyphSet::Unicode),
            depth: Some(theme::Depth::TrueColor),
            ..LiveViewOptions::default()
        }
    }

    #[allow(clippy::large_types_passed_by_value)]
    fn text_rows(
        width: u16,
        height: u16,
        input: &str,
        options: LiveViewOptions<'_>,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(frame, &Reducer::default(), input, 0, options);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wide_layout_reserves_exactly_38_columns_for_the_sidebar() {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let options = LiveViewOptions {
            sidebar_visible: true,
            ..idle_options()
        };
        terminal
            .draw(|frame| {
                draw_live(frame, &Reducer::default(), "", 0, options);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in [2_u16, 10, 20, 30] {
            assert_eq!(
                buffer.cell((140 - SIDEBAR_WIDTH, y)).unwrap().symbol(),
                "│",
                "sidebar must start 38 columns from the right at row {y}"
            );
        }
    }

    #[test]
    fn medium_hides_the_sidebar_until_it_is_opened() {
        let closed = text_rows(
            100,
            30,
            "",
            LiveViewOptions {
                sidebar_visible: false,
                ..idle_options()
            },
        );
        assert!(!closed.iter().any(|row| row.contains("DEFAULT BUDGET")));
        let open = text_rows(
            100,
            30,
            "",
            LiveViewOptions {
                sidebar_visible: true,
                ..idle_options()
            },
        );
        assert!(open.iter().any(|row| row.contains("DEFAULT BUDGET")));
        assert!(open.iter().any(|row| row.contains("no turns yet")));
    }

    #[test]
    fn no_color_ascii_and_16_colour_still_carry_words_and_glyphs() {
        for depth in [theme::Depth::NoColor, theme::Depth::Ansi16] {
            let rows = text_rows(
                140,
                40,
                "",
                LiveViewOptions {
                    sidebar_visible: true,
                    glyph_set: Some(GlyphSet::Ascii),
                    depth: Some(depth),
                    ..idle_options()
                },
            );
            let text = rows.join("\n");
            assert!(text.contains("# loom"), "missing ASCII brand: {text}");
            assert!(text.contains("> Describe a task…"));
            assert!(text.contains("SESSION"));
            assert!(text.contains("no turns yet"));
            assert!(text.contains("[/help] help"));
        }
    }

    pub(super) fn snapshot(buffer: &ratatui::buffer::Buffer) -> String {
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
                Scene::Main
                | Scene::Picker
                | Scene::Help
                | Scene::Spec
                | Scene::Narrow
                | Scene::CompactA
                | Scene::CompactB => {}
            }
        }
    }

    #[test]
    fn narrow_view_puts_status_in_the_header_and_keeps_one_hint() {
        let reducer = Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", 0, LiveViewOptions::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("idle · turn") && row.contains("cost")),
            "no status line in {rows:#?}"
        );
        assert!(rows.iter().any(|row| row.contains("[/help] help")));
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("[Esc]") || row.contains("models"))
        );
    }

    #[test]
    fn command_palette_scrolls_to_keep_the_selected_command_visible() {
        let last = crate::palette::COMMANDS.len() - 1;
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| draw_palette(frame, frame.area(), "/", last, GlyphSet::Unicode))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains(crate::palette::COMMANDS[last].name))
        );
    }

    #[test]
    fn slash_input_opens_the_command_palette_over_the_composer() {
        let reducer = Reducer::default();
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "/mod",
                    4,
                    LiveViewOptions {
                        palette_selected: 0,
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(rows.iter().any(|row| row.contains("commands")));
        assert!(rows.iter().any(|row| row.contains("/model")));
        assert!(
            !rows.iter().any(|row| row.contains("/export")),
            "filter should hide non-matching commands"
        );
        assert!(rows.iter().any(|row| row.contains("Enter run")));

        // A notice renders above the footer.
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        notice: Some("exported to /tmp/x"),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("exported to /tmp/x"));
    }

    #[test]
    fn medium_header_truncates_workspace_instead_of_gluing_onto_the_model() {
        let reducer = Reducer::default();
        let long_path = "/var/folders/sf/kblwt_lj3c1gbsvjy52tt2fr0000gn/T/tmp.oTHmCBHiSr/ws";
        let mut terminal = Terminal::new(TestBackend::new(105, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        workspace: Some(long_path),
                        model: Some("deepseek-flash"),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let header: String = (0..buffer.area.width)
            .map(|x| buffer.cell((x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(header.contains('…'), "workspace not truncated: {header}");
        assert!(
            !header.contains("wsdeepseek-flash"),
            "workspace glued onto model: {header}"
        );
        assert!(header.contains("deepseek-flash"));
    }

    #[test]
    fn idle_home_lists_recent_sessions_and_try_hints() {
        let reducer = Reducer::default();
        let recent = [crate::recent::RecentSession {
            title: "Inspect the retry path".into(),
            turns: 5,
            status: Some(TaskStatus::Completed),
        }];
        let mut terminal = Terminal::new(TestBackend::new(154, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        recent: &recent,
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
        assert!(rendered.contains("RECENT"));
        assert!(rendered.contains("Inspect the retry path"));
        assert!(rendered.contains("✓ 5 turns"));
        assert!(rendered.contains("TRY"));
        assert!(rendered.contains("Explain this crate"));
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
    fn read_only_profile_is_visible_in_the_header() {
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
                        task_mode: Some("read-only"),
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
        assert!(rendered.contains("read-only"));
        assert!(rendered.contains("blocked"));
        assert!(!rendered.contains("asks each time"));
    }

    #[test]
    fn live_read_file_result_shows_a_bounded_numbered_preview() {
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect the file");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 0,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::TurnStarted { turn: 1 },
            },
            1,
        );
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
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
                timestamp_millis: None,
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
        reducer.selected = Some(crate::reducer::Selection::ToolCall {
            turn: 1,
            call_id: "read-1".to_owned(),
        });
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
        assert!(rendered.contains("… 3 more lines · ^O"));
    }

    #[test]
    fn tool_errors_keep_their_category_and_message() {
        let mut reducer = Reducer::default();
        reducer.begin_task("run the tool");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
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
                timestamp_millis: None,
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
                timestamp_millis: None,
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
                timestamp_millis: None,
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

        assert!(rendered.contains("Timeout"));
        assert!(rendered.contains("model sees: error: timed out after 30s waiting for cargo test"));
        // Duration stays visible even though the call failed.
        assert!(rendered.contains("250 ms"), "{rendered}");
        assert!(!rendered.contains("· done"));
    }

    #[test]
    fn streaming_turn_shows_cursor_and_provisional_reasoning() {
        let mut reducer = Reducer::default();
        reducer.begin_task("streaming task");
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 0,
                task_id: loom_core::TaskId::new(),
                kind: loom_core::EventKind::TaskStarted,
            },
            0,
        );
        reducer.reduce(
            &loom_core::EventEnvelope {
                schema_version: loom_core::kernel::event::EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 1,
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

        assert!(rendered.contains("reasoning ·"));
        assert!(rendered.contains("weighing two approaches"));
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

        assert!(rendered.contains("Needs you"));
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
        assert!(rendered.contains("❯ ask @src/lib.rs"));
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
    fn markdown_is_rendered_without_syntax_markers() {
        let lines = markdown_lines(
            "## Repo state\n`main`, **59 commits ahead**\n\n- first item\n- second *emphasis*\n\n\
             ```rust\nfn main() {}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted",
        );
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered[0], "Repo state");
        assert_eq!(rendered[1], "main, 59 commits ahead");
        assert!(rendered.iter().any(|line| line == "• first item"));
        assert!(rendered.iter().any(|line| line == "• second emphasis"));
        assert!(rendered.iter().any(|line| line.trim() == "fn main() {}"));
        assert!(rendered.iter().any(|line| line == "a │ b"));
        assert!(rendered.iter().any(|line| line == "│ quoted"));
        assert!(
            rendered.iter().all(|line| !line.contains("**")
                && !line.contains('`')
                && !line.contains("---")
                && !line.contains("##")),
            "markdown markers must not reach the screen: {rendered:?}"
        );
    }

    #[test]
    fn thread_text_does_not_run_under_the_scrollbar() {
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        let mut lines: Vec<Line<'static>> = (0..12).map(|_| Line::from("filler")).collect();
        lines.push(Line::from("aaaa bbbb cccc ddddd"));
        terminal
            .draw(|frame| render_thread_scrolled(frame, frame.area(), lines, 0))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            text.contains("ddddd"),
            "wrapped text must not be overwritten by the scrollbar: {text:?}"
        );
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
    fn prompt_cursor_sits_after_the_prompt_glyph_and_scrolls_with_wrapped_input() {
        let area = Rect::new(4, 7, 12, 3);
        assert_eq!(prompt_cursor("", 0, area), (7, 8, 0));
        assert_eq!(prompt_cursor("abc", 3, area), (10, 8, 0));
        let input = "abcdefghijklmno";
        assert_eq!(prompt_cursor(input, input.len(), area), (12, 8, 1));
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
        assert!(text.contains("system + tools"));
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
        // The footer owns the event counter; the sidebar must NOT repeat it.
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
        assert_eq!(
            rendered.matches("events 1284 · dropped 0").count(),
            1,
            "the event counter must render exactly once, in the footer only"
        );
    }

    #[test]
    fn dropped_events_turn_the_footer_into_an_incomplete_trace() {
        let mut reducer = Reducer::default();
        reducer.task.status = Some(loom_core::kernel::session::TaskStatus::Running);
        reducer.event_count = 12;
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    &reducer,
                    "",
                    0,
                    LiveViewOptions {
                        dropped: Some(3),
                        ..idle_options()
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
        assert!(
            rendered.contains("events 12 · dropped 3 · trace incomplete"),
            "{rendered}"
        );
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
        assert!(segments[0].to_string().contains("system + tools"));
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
        assert!(rendered.contains("total (est.) 0"));
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

    fn batch_event(
        sequence: u64,
        task: loom_core::TaskId,
        kind: EventKind,
    ) -> loom_core::EventEnvelope {
        loom_core::EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            timestamp_millis: None,
            sequence,
            task_id: task,
            kind,
        }
    }

    fn live_rows(reducer: &Reducer, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_live(
                    frame,
                    reducer,
                    "",
                    0,
                    LiveViewOptions {
                        sidebar_visible: true,
                        glyph_set: Some(GlyphSet::Unicode),
                        depth: Some(theme::Depth::TrueColor),
                        ..LiveViewOptions::default()
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).unwrap(),
            name: name.to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({})).unwrap(),
        }
    }

    #[test]
    fn live_batch_reports_an_interleaved_completion_at_wide_and_narrow_widths() {
        let task = loom_core::TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect both paths");
        for (sequence, timestamp, kind) in [
            (0, 1_000, EventKind::TaskStarted),
            (1, 1_000, EventKind::TurnStarted { turn: 1 }),
            (
                2,
                1_100,
                EventKind::ModelCompleted {
                    response: ModelResponse {
                        message: AssistantMessage {
                            content: String::new(),
                            reasoning: None,
                            tool_calls: vec![call("slow", "read_file"), call("fast", "search")],
                            stop_reason: StopReason::ToolCalls,
                        },
                        usage: Usage::default(),
                    },
                },
            ),
            (
                3,
                2_000,
                EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: ToolCallId::try_from("slow".to_owned()).unwrap(),
                    arguments: "{}".into(),
                },
            ),
            (
                4,
                2_001,
                EventKind::ToolStarted {
                    name: "search".into(),
                    call_id: ToolCallId::try_from("fast".to_owned()).unwrap(),
                    arguments: "{}".into(),
                },
            ),
            (
                5,
                2_050,
                EventKind::ToolCompleted {
                    name: "search".into(),
                    call_id: ToolCallId::try_from("fast".to_owned()).unwrap(),
                    status: ToolStatus::Success,
                    content_head: "found it".into(),
                },
            ),
        ] {
            reducer.reduce(&batch_event(sequence, task, kind), timestamp);
        }

        for (width, height) in [(140, 40), (70, 24)] {
            let text = live_rows(&reducer, width, height).join("\n");
            assert!(
                text.contains("2 read-only calls · 1 active · 1 done"),
                "live phase counts missing at {width}x{height}\n{text}"
            );
            assert!(
                !text.contains("1 queued"),
                "settled fast call queued\n{text}"
            );
        }
    }

    #[test]
    fn cancelled_batch_settles_started_and_never_polled_calls() {
        let task = loom_core::TaskId::new();
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect three paths");
        for (sequence, timestamp, kind) in [
            (0, 1_000, EventKind::TaskStarted),
            (1, 1_000, EventKind::TurnStarted { turn: 1 }),
            (
                2,
                1_100,
                EventKind::ModelCompleted {
                    response: ModelResponse {
                        message: AssistantMessage {
                            content: String::new(),
                            reasoning: None,
                            tool_calls: vec![
                                call("done", "read_file"),
                                call("inflight", "search"),
                                call("never", "list_dir"),
                            ],
                            stop_reason: StopReason::ToolCalls,
                        },
                        usage: Usage::default(),
                    },
                },
            ),
            (
                3,
                2_000,
                EventKind::ToolStarted {
                    name: "read_file".into(),
                    call_id: ToolCallId::try_from("done".to_owned()).unwrap(),
                    arguments: "{}".into(),
                },
            ),
            (
                4,
                2_010,
                EventKind::ToolCompleted {
                    name: "read_file".into(),
                    call_id: ToolCallId::try_from("done".to_owned()).unwrap(),
                    status: ToolStatus::Success,
                    content_head: "ok".into(),
                },
            ),
            (
                5,
                2_020,
                EventKind::ToolStarted {
                    name: "search".into(),
                    call_id: ToolCallId::try_from("inflight".to_owned()).unwrap(),
                    arguments: "{}".into(),
                },
            ),
            (
                6,
                2_100,
                EventKind::ToolCompleted {
                    name: "search".into(),
                    call_id: ToolCallId::try_from("inflight".to_owned()).unwrap(),
                    status: ToolStatus::Cancelled { started: true },
                    content_head: "cancelled during execution".into(),
                },
            ),
            (
                7,
                2_100,
                EventKind::ToolCompleted {
                    name: "list_dir".into(),
                    call_id: ToolCallId::try_from("never".to_owned()).unwrap(),
                    status: ToolStatus::Cancelled { started: false },
                    content_head: "not started".into(),
                },
            ),
            (8, 2_200, EventKind::TaskCancelled),
        ] {
            reducer.reduce(&batch_event(sequence, task, kind), timestamp);
        }

        for (width, height) in [(140, 40), (70, 24)] {
            let text = live_rows(&reducer, width, height).join("\n");
            assert!(
                text.matches("cancelled").count() >= 2,
                "both cancellation outcomes missing at {width}x{height}\n{text}"
            );
            assert!(!text.contains("1 active"), "stale active row\n{text}");
            assert!(!text.contains("1 queued"), "stale queued row\n{text}");
        }
    }
}

#[cfg(test)]
mod approval_render_tests {
    use crate::approval::Request;
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::sync::oneshot;

    #[test]
    fn approval_box_names_the_path_and_all_four_keys() {
        let (reply, _rx) = oneshot::channel();
        let request = Request {
            path: "notes.md".to_owned(),
            content: "hello".to_owned(),
            reply,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::draw_approval_box(frame, area, &request);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Allow this write?"), "prompt missing");
        assert!(text.contains("notes.md"), "path missing");
        for key in ["[y] once", "[a] session", "[n] deny", "[e] edit first"] {
            assert!(text.contains(key), "key hint `{key}` missing");
        }
    }

    #[test]
    fn approval_box_shows_the_wireframe_diff_preview() {
        let (reply, _rx) = oneshot::channel();
        let content = "alpha\n\nbeta\ngamma\ndelta\n".to_owned();
        let request = Request {
            path: "notes.md".to_owned(),
            content,
            reply,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::draw_approval_box(frame, area, &request);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        // Whole-file insert: +N −0 stats, @@ header, two preview lines,
        // then the more-lines trailer pointing at ^O.
        assert!(text.contains("+5 −0"), "line stats missing: {text}");
        assert!(
            text.contains("@@ notes.md · 5 lines"),
            "diff header missing"
        );
        assert!(text.contains("+ alpha"), "first preview line missing");
        assert!(text.contains("+ beta"), "second preview line missing");
        assert!(
            text.contains("2 more lines · ^O expand"),
            "trailer missing: {text}"
        );
    }
}

#[cfg(test)]
mod pending_sidebar_tests {
    use crate::approval::Request;
    use crate::glyphs::GlyphSet;
    use crate::picker::ModelPricing;
    use crate::reducer::Reducer;
    use crate::scene_views::{LiveViewOptions, draw_live};
    use crate::theme;
    use ratatui::{Terminal, backend::TestBackend};
    use std::env;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[test]
    fn pending_section_lists_the_open_write_request() {
        let (reply, _rx) = oneshot::channel();
        let request = Request {
            path: "notes.md".to_owned(),
            content: "hello".to_owned(),
            reply,
        };
        let reducer = crate::reducer::Reducer::default();
        let options = LiveViewOptions {
            approval: Some(&request),
            sidebar_visible: true,
            ..LiveViewOptions::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(154, 80)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", 0, options))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            text.contains("◇ notes.md"),
            "pending path missing from sidebar"
        );
        assert!(
            text.contains("y once   a session   n deny"),
            "pending keys missing"
        );
    }

    #[test]
    fn blocked_write_row_reads_awaiting_not_running() {
        use loom_core::EventEnvelope;
        use loom_core::EventKind;
        use loom_core::TaskId;
        use loom_core::kernel::event::EVENT_SCHEMA_VERSION;
        use loom_core::kernel::model::{ModelResponse, StopReason, Usage};
        use loom_core::kernel::transcript::{
            AssistantMessage, ToolArguments, ToolCall, ToolCallId,
        };

        let mut reducer = crate::reducer::Reducer::default();
        reducer.begin_task("write the notes");
        let call = ToolCallId::try_from("w-1".to_owned()).unwrap();
        reducer.reduce(
            &EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 0,
                task_id: TaskId::new(),
                kind: EventKind::TurnStarted { turn: 1 },
            },
            1,
        );
        reducer.reduce(
            &EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 1,
                task_id: TaskId::new(),
                kind: EventKind::ModelCompleted {
                    response: ModelResponse {
                        message: AssistantMessage {
                            content: String::new(),
                            reasoning: None,
                            tool_calls: vec![ToolCall {
                                id: call.clone(),
                                name: "write_file".to_owned(),
                                arguments: ToolArguments::try_from(serde_json::json!({
                                    "path": "notes.md"
                                }))
                                .unwrap(),
                            }],
                            stop_reason: StopReason::ToolCalls,
                        },
                        usage: Usage::default(),
                    },
                },
            },
            100,
        );
        reducer.reduce(
            &EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION,
                timestamp_millis: None,
                sequence: 2,
                task_id: TaskId::new(),
                kind: EventKind::ToolStarted {
                    name: "write_file".into(),
                    call_id: call,
                    arguments: r#"{"path":"notes.md"}"#.to_owned(),
                },
            },
            200,
        );
        let (reply, _rx) = oneshot::channel();
        let request = Request {
            path: "notes.md".to_owned(),
            content: "hello".to_owned(),
            reply,
        };
        let options = LiveViewOptions {
            approval: Some(&request),
            sidebar_visible: true,
            ..LiveViewOptions::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(154, 80)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", 0, options))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("write_file"), "tool row missing");
        assert!(text.contains("awaiting"), "needs-you state missing");
    }
    #[test]
    #[allow(clippy::wildcard_enum_match_arm)] // only the four checkpoint events matter
    fn live_fixture_replays_through_the_pure_reducer_at_layout_checkpoints() {
        use std::fmt::Write as _;

        let fixture = include_str!("../tests/fixtures/live-read-only.ndjson");
        let events = fixture
            .lines()
            .map(serde_json::from_str::<loom_core::EventEnvelope>)
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture is valid NDJSON");
        let mut reducer = Reducer::default();
        reducer.begin_task("Inspect this repository and report the event-sink call chain");
        let mut snapshots = String::new();
        let mut tool_completions = 0;
        let mut model_completions = 0;

        for event in &events {
            reducer.reduce(event, event.timestamp_millis.unwrap_or(0));
            let checkpoint = match &event.kind {
                loom_core::EventKind::TurnStarted { turn: 1 } => Some("after-turn-started"),
                loom_core::EventKind::ToolCompleted { .. } => {
                    tool_completions += 1;
                    (tool_completions == 2).then_some("mid-batch")
                }
                loom_core::EventKind::ModelCompleted { .. } => {
                    model_completions += 1;
                    (model_completions == 2).then_some("after-model-completed")
                }
                loom_core::EventKind::ReasoningDelta { .. } => Some("mid-stream"),
                _ => None,
            };
            let Some(checkpoint) = checkpoint else {
                continue;
            };
            for (width, height) in [(140, 40), (100, 30), (70, 24)] {
                for (mode, glyph_set) in
                    [("unicode", GlyphSet::Unicode), ("ascii", GlyphSet::Ascii)]
                {
                    let mut state = reducer.clone();
                    if checkpoint == "after-model-completed" {
                        state.expanded_result = Some("call-read".to_owned());
                    }
                    let options = LiveViewOptions {
                        sidebar_visible: true,
                        elapsed: Some(Duration::from_secs(102)),
                        dropped: Some(0),
                        workspace: Some("~/work/loom"),
                        session_id: Some("0194a7e2-1f9c-7c3e-9b2a-4d5e6f7a8b9c"),
                        model: Some("deepseek-flash"),
                        effort: Some("high"),
                        thinking: Some("on"),
                        task_mode: Some("read-only"),
                        task_label: Some("benchmark B1"),
                        billing_period: Some("off-peak"),
                        turn_limit: Some(50),
                        pricing: Some(ModelPricing {
                            input_per_million: Some(0.14),
                            output_per_million: Some(0.28),
                        }),
                        depth: Some(theme::Depth::TrueColor),
                        glyph_set: Some(glyph_set),
                        ..LiveViewOptions::default()
                    };
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| draw_live(frame, &state, "", 0, options))
                        .unwrap();
                    let _ = writeln!(snapshots, "=== {checkpoint} {width}x{height} {mode} ===");
                    snapshots.push_str(&super::tests::snapshot(terminal.backend().buffer()));
                }
            }
        }

        if env::var_os("UPDATE_TUI_SNAPSHOTS").is_some() {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/snapshots/live-replay.snap");
            std::fs::write(path, &snapshots).unwrap();
        }
        assert_eq!(
            snapshots,
            include_str!("../tests/snapshots/live-replay.snap")
        );
    }
}

#[cfg(test)]
mod idle_sidebar_tests {
    use crate::scene_views::{LiveViewOptions, draw_live};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn idle_sidebar_shows_session_and_tool_permissions() {
        let reducer = crate::reducer::Reducer::default();
        let options = LiveViewOptions {
            sidebar_visible: true,
            model: Some("deepseek-chat"),
            effort: Some("medium"),
            thinking: Some("on"),
            ..LiveViewOptions::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(154, 80)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, &reducer, "", 0, options))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("deepseek-chat"), "session model missing");
        assert!(text.contains("effort / thinking"), "session effort missing");
        assert!(text.contains("TOOLS"), "tool permissions missing");
        assert!(text.contains("ask"), "tool permission missing");
    }
}

#[cfg(test)]
mod retry_scene_tests {
    use std::fmt::Write as _;
    use std::time::Duration;

    use loom_core::EventEnvelope;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::glyphs::GlyphSet;
    use crate::picker::ModelPricing;
    use crate::reducer::Reducer;
    use crate::theme;

    use super::{LiveViewOptions, draw_live};

    fn events(fixture: &str) -> Vec<EventEnvelope> {
        fixture
            .lines()
            .map(serde_json::from_str::<EventEnvelope>)
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture is valid NDJSON")
    }

    fn options(glyph_set: GlyphSet) -> LiveViewOptions<'static> {
        LiveViewOptions {
            sidebar_visible: true,
            elapsed: Some(Duration::from_secs(19)),
            dropped: Some(0),
            workspace: Some("~/work/loom"),
            session_id: Some("0194a7e2-1f9c-7c3e-9b2a-4d5e6f7a8b9c"),
            model: Some("deepseek-flash"),
            effort: Some("high"),
            thinking: Some("on"),
            task_mode: Some("read-only"),
            task_label: Some("benchmark B1"),
            billing_period: Some("off-peak"),
            turn_limit: Some(50),
            pricing: Some(ModelPricing {
                input_per_million: Some(0.14),
                output_per_million: Some(0.28),
            }),
            depth: Some(theme::Depth::TrueColor),
            glyph_set: Some(glyph_set),
            ..LiveViewOptions::default()
        }
    }

    fn render(reducer: &Reducer, width: u16, height: u16, glyph_set: GlyphSet) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, reducer, "", 0, options(glyph_set)))
            .unwrap();
        super::tests::snapshot(terminal.backend().buffer())
    }

    fn render_plain(reducer: &Reducer, width: u16, height: u16, glyph_set: GlyphSet) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_live(frame, reducer, "", 0, options(glyph_set)))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn push_snapshots(out: &mut String, reducer: &Reducer, name: &str, now: u64) {
        let mut state = reducer.clone();
        state.tick(now);
        for (width, height) in [(140, 40), (100, 30), (70, 24)] {
            for (mode, glyph_set) in [("unicode", GlyphSet::Unicode), ("ascii", GlyphSet::Ascii)] {
                let _ = writeln!(out, "=== {name} {width}x{height} {mode} ===");
                out.push_str(&render(&state, width, height, glyph_set));
            }
        }
    }

    #[test]
    #[allow(clippy::wildcard_enum_match_arm)]
    fn retry_fixture_replays_at_deterministic_checkpoints() {
        let fixture = include_str!("../tests/fixtures/retry-then-success.ndjson");
        let trace = events(fixture);
        let mut reducer = Reducer::default();
        reducer.begin_task("Explain how the DeepSeek adapter normalizes finish reasons.");
        let mut snapshots = String::new();

        for event in &trace {
            let timestamp = event.timestamp_millis.unwrap_or(0);
            reducer.reduce(event, timestamp);
            match &event.kind {
                loom_core::EventKind::ModelRetry { attempt: 2, .. } => {
                    push_snapshots(&mut snapshots, &reducer, "waiting", timestamp);
                    push_snapshots(&mut snapshots, &reducer, "mid-countdown", timestamp + 1_500);
                }
                loom_core::EventKind::ModelRequestStarted { attempt: 3 } => {
                    push_snapshots(&mut snapshots, &reducer, "attempt-started", timestamp);
                }
                _ => {}
            }
        }

        let failures = events(include_str!("../tests/fixtures/retries-exhausted.ndjson"));
        let mut failed = Reducer::default();
        failed.begin_task("Explain how the DeepSeek adapter normalizes finish reasons.");
        for event in &failures {
            failed.reduce(event, event.timestamp_millis.unwrap_or(0));
        }
        push_snapshots(
            &mut snapshots,
            &failed,
            "retries-exhausted",
            failures
                .last()
                .and_then(|event| event.timestamp_millis)
                .unwrap_or(0),
        );

        if std::env::var_os("UPDATE_TUI_SNAPSHOTS").is_some() {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/snapshots/retry-replay.snap");
            std::fs::write(path, &snapshots).unwrap();
        }
        assert_eq!(
            snapshots,
            include_str!("../tests/snapshots/retry-replay.snap")
        );
    }

    #[test]
    fn terminal_failure_fixtures_render_their_card_and_never_raw_reason_only() {
        for (fixture, expected, action) in [
            (
                include_str!("../tests/fixtures/max-tokens.ndjson"),
                "MaxTokens",
                "Raise --max-tokens or narrow the task.",
            ),
            (
                include_str!("../tests/fixtures/quota-exhausted.ndjson"),
                "QuotaExhausted",
                "Top up, or loom auth switch deepseek <label>",
            ),
            (
                include_str!("../tests/fixtures/content-filter.ndjson"),
                "ContentFilter",
                "Rephrase the task and run it again.",
            ),
        ] {
            let mut reducer = Reducer::default();
            reducer.begin_task("Explain how the DeepSeek adapter normalizes finish reasons.");
            for event in events(fixture) {
                reducer.reduce(&event, event.timestamp_millis.unwrap_or(0));
            }
            for (width, height, set) in [
                (140, 40, GlyphSet::Unicode),
                (140, 40, GlyphSet::Ascii),
                (70, 24, GlyphSet::Ascii),
            ] {
                let text = render_plain(&reducer, width, height, set);
                assert!(text.contains(expected), "{expected} missing\n{text}");
                assert!(text.contains(action), "{action} clipped\n{text}");
                assert!(!text.contains("Task failed ·"), "raw reason leaked\n{text}");
            }
        }
    }

    #[test]
    fn retry_after_fixture_uses_the_event_delay() {
        let mut reducer = Reducer::default();
        reducer.begin_task("Explain the retry policy.");
        for event in events(include_str!("../tests/fixtures/retry-after.ndjson")) {
            let timestamp = event.timestamp_millis.unwrap_or(0);
            reducer.reduce(&event, timestamp);
            if matches!(
                &event.kind,
                loom_core::EventKind::ModelRetry { attempt: 1, .. }
            ) {
                assert_eq!(reducer.retry_wait.as_ref().unwrap().delay_ms, 1_234);
                reducer.tick(timestamp + 1_000);
                assert_eq!(reducer.retry_wait.as_ref().unwrap().remaining_millis(), 234);
            }
        }
    }

    #[test]
    fn esc_during_backoff_cancels_without_starting_another_attempt() {
        let mut reducer = Reducer::default();
        reducer.begin_task("Explain the retry policy.");
        for event in events(include_str!("../tests/fixtures/esc-during-backoff.ndjson")) {
            reducer.reduce(&event, event.timestamp_millis.unwrap_or(0));
        }
        assert_eq!(
            reducer.task.status,
            Some(loom_core::kernel::session::TaskStatus::Cancelled)
        );
        assert!(!reducer.is_retrying());
        assert_eq!(reducer.retries.len(), 1);
        assert_eq!(reducer.attempt, Some(2));
        assert!(reducer.failure.is_none());
    }
}
