use std::time::Duration;

use loom_core::kernel::context::ContextCensus;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{picker::ModelPricing, reducer::Reducer, theme};

use super::{
    LiveViewOptions, Scene, TaskStatus, fg, format_compact_tokens, heading, muted, status_label,
    tinted,
};

/// Sidebar for the static preview scenes: every live counter is explicitly
/// marked unavailable so a preview can never imply a running task.
pub(super) fn draw_scene_sidebar(
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
pub(super) fn draw_runtime_sidebar(
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
    if let Some(request) = options.approval {
        sections_data.push((
            "PENDING",
            vec![
                Line::from(Span::styled(
                    format!("◇ {}", request.path),
                    fg(theme::AMBER).add_modifier(Modifier::BOLD),
                )),
                muted("y allow once   a always   n decline"),
            ],
        ));
    }
    // Each bordered section consumes a bottom-rule row in addition to its
    // text. Account for that row so natural section heights do not overrun the
    // sidebar and push EVENTS below the viewport.
    let mut minimums = vec![4_u16, 6, 5, 3, 3];
    if options.approval.is_some() {
        minimums.insert(4, 3);
    }
    let mut heights = sections_data
        .iter()
        .map(|(_, lines)| {
            u16::try_from(lines.len())
                .unwrap_or(u16::MAX)
                .saturating_add(1)
        })
        .collect::<Vec<_>>();
    let available = content.height;
    while heights.iter().copied().sum::<u16>() + minimums[minimums.len() - 1] > available {
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

pub(super) fn render_sidebar_section(
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

pub(super) fn sidebar_limit_bar(used: u64, limit: u64, width: u16) -> Line<'static> {
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

pub(super) fn sidebar_unbounded_bar(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "━".repeat(usize::from(width)),
        fg(theme::BORDER),
    ))
}

pub(super) fn format_duration(duration: Duration) -> String {
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

pub(super) fn format_run_cost(reducer: &Reducer, pricing: Option<ModelPricing>) -> Option<String> {
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

pub(super) fn sidebar_budget_width(
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

pub(super) fn sidebar_budget_text(label: &str, value: &str, width: u16) -> Line<'static> {
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

pub(super) fn context_bar(census: ContextCensus, width: u16) -> Line<'static> {
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

pub(super) fn context_categories(census: ContextCensus) -> Vec<(&'static str, u32, Color)> {
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

pub(super) fn context_segments(census: ContextCensus, width: u16) -> Vec<Line<'static>> {
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

pub(super) fn event_line(event: &str) -> Line<'static> {
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

pub(super) fn optional_budget(value: Option<u64>) -> String {
    value.map_or_else(
        || "unbounded".to_owned(),
        |remaining| format!("{remaining} remaining"),
    )
}
