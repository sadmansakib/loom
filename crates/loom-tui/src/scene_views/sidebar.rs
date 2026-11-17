use std::time::Duration;

use loom_core::kernel::context::ContextCensus;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    glyphs::{self, Glyph},
    picker::ModelPricing,
    reducer::{FailureKind, Reducer},
    theme,
};

use super::{
    LiveViewOptions, Scene, TaskStatus, fg, format_compact_tokens, heading, muted, status_label,
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
        Scene::Main
        | Scene::Picker
        | Scene::Help
        | Scene::Spec
        | Scene::Narrow
        | Scene::CompactA
        | Scene::CompactB => {}
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

    // Idle is not a zeroed dashboard: the wireframe keeps three quiet
    // sections (what this session is, its limits, what tools may do).
    if reducer.task.status.is_none()
        && reducer.execution_turns.is_empty()
        && reducer.original_instruction.is_none()
        && !reducer.restored
        && reducer.task.display_prompts.is_empty()
        && reducer.context_progress.is_none()
        && reducer.summary.is_none()
        && reducer.recent_events.is_empty()
        && reducer.tools.is_empty()
        && options.approval.is_none()
    {
        let pad = Rect {
            x: content.x.saturating_add(2),
            width: content.width.saturating_sub(2),
            ..content
        };
        if pad.width == 0 {
            return;
        }
        let width = pad.width;
        let turns = options
            .turn_limit
            .map_or_else(|| "unbounded".to_owned(), |limit| limit.to_string());
        let tool_calls = options
            .idle
            .tool_call_budget
            .map_or_else(|| "unbounded".to_owned(), |limit| limit.to_string());
        let wall = options
            .idle
            .wall_budget
            .map_or_else(|| "unbounded".to_owned(), format_duration);
        // The wireframe gives each `.lb` a 14px top margin, which reads as
        // one blank row between the three sections in a terminal.
        let lines = vec![
            Line::default(),
            heading("SESSION"),
            sidebar_idle_row("new", "no turns yet", width, true),
            sidebar_idle_row(
                "model",
                options.model.unwrap_or("not reported"),
                width,
                false,
            ),
            sidebar_idle_row(
                "effort / thinking",
                &format!(
                    "{} · {}",
                    options.effort.unwrap_or("not reported"),
                    options.thinking.unwrap_or("not reported")
                ),
                width,
                false,
            ),
            Line::default(),
            heading("DEFAULT BUDGET"),
            sidebar_idle_row("turns", &turns, width, false),
            sidebar_idle_row("tool calls", &tool_calls, width, false),
            sidebar_idle_row("wall", &wall, width, false),
            Line::default(),
            heading("TOOLS"),
            sidebar_idle_row("read", options.idle.read_permission, width, false),
            sidebar_idle_row("write", options.idle.write_permission, width, false),
            sidebar_idle_row("shell", options.idle.shell_permission, width, false),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::BG))
                .wrap(Wrap { trim: false }),
            pad,
        );
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
    let status_color = if options.approval.is_some()
        || matches!(
            reducer.task.status,
            Some(TaskStatus::Running | TaskStatus::Paused)
        ) {
        theme::AMBER
    } else if reducer.task.status == Some(TaskStatus::Completed) {
        theme::SAGE
    } else if reducer.task.status == Some(TaskStatus::Failed) {
        theme::CORAL
    } else {
        theme::DIM
    };
    let retrying = reducer.is_retrying();
    let (status_glyph, status_text) =
        if options.approval.is_some() || reducer.task.status == Some(TaskStatus::Paused) {
            (
                crate::glyphs::glyph(crate::glyphs::Glyph::Needs, super::glyph_set()),
                "Needs you",
            )
        } else if retrying {
            (
                crate::glyphs::glyph(crate::glyphs::Glyph::Retry, super::glyph_set()),
                "Retrying",
            )
        } else {
            (super::status_glyph(reducer), status_label(reducer))
        };
    let task_detail = if retrying {
        let turn = reducer.turn.unwrap_or(0);
        let limit = options
            .turn_limit
            .map_or_else(|| "?".to_owned(), |limit| limit.to_string());
        format!("{task_id} · turn {turn} of {limit}")
    } else {
        format!(
            "{task_id} · {} · {}",
            options.task_mode.unwrap_or("mode unavailable"),
            options.task_label.unwrap_or("no label"),
        )
    };
    let task_lines = vec![
        heading("TASK"),
        {
            let gap = usize::from(content.width)
                .saturating_sub(status_text.chars().count() + elapsed_label.chars().count() + 2)
                .max(1);
            Line::from(vec![
                Span::styled(format!("{status_glyph} "), fg(status_color)),
                Span::styled(status_text, fg(status_color).add_modifier(Modifier::BOLD)),
                Span::raw(" ".repeat(gap)),
                Span::styled(elapsed_label.clone(), fg(theme::DIM)),
            ])
        },
        muted(&task_detail),
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
        budget_lines.push(sidebar_budget_text(
            "tokens in",
            &optional_budget(budget.input_tokens),
            content.width,
        ));
        budget_lines.push(sidebar_budget_text(
            "tokens out",
            &optional_budget(budget.output_tokens),
            content.width,
        ));
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
            "total (est.) {} · {utilization}",
            format_compact_tokens(u64::from(census.total)),
        )));
        if let Some(prefix) = reducer.cacheable_prefix_tokens {
            let (mark, word, color) = match reducer.cacheable_prefix_stable {
                Some(true) => (
                    crate::glyphs::glyph(crate::glyphs::Glyph::Check, super::glyph_set()),
                    "stable",
                    theme::TEAL,
                ),
                Some(false) => (
                    crate::glyphs::glyph(crate::glyphs::Glyph::Cross, super::glyph_set()),
                    "changed since last request",
                    theme::CORAL,
                ),
                None => ("?", "unverified", theme::DIM),
            };
            context_lines.push(Line::from(Span::styled(
                format!(
                    "cacheable prefix {} {mark} {word}",
                    format_compact_tokens(prefix)
                ),
                fg(color),
            )));
        }
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
    let period = options
        .billing_period
        .unwrap_or("billing unknown")
        .to_owned();
    let cost_gap = usize::from(content.width)
        .saturating_sub(cost.chars().count() + period.chars().count())
        .max(1);
    let cached = usage_value(reducer.usage.cached_input_tokens);
    let cost_lines = vec![
        heading("COST"),
        Line::from(vec![
            Span::styled(cost, fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(cost_gap)),
            Span::styled(period, fg(theme::DIM)),
        ]),
        muted(&format!(
            "in {}",
            usage_value(Some(reducer.usage.input_tokens))
        )),
        muted(&format!(
            "out {}",
            usage_value(Some(reducer.usage.output_tokens))
        )),
        Line::from(Span::styled(format!("{cached} cached"), fg(theme::TEAL))),
    ];

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

    let show_retry = retrying || !reducer.retries.is_empty() || reducer.failure.is_some();
    let retry_sections = if show_retry {
        Some((
            retry_attempt_lines(reducer, content.width),
            retry_policy_lines(reducer, content.width),
            kind_lines(true, content.width),
            kind_lines(false, content.width),
        ))
    } else {
        None
    };

    // Reserve space for every sidebar section first. Compress informational
    // rows before sacrificing the event panel at shorter terminal heights.
    let mut sections_data = if let Some((attempts, policy, retried, never)) = retry_sections {
        vec![
            ("TASK", task_lines),
            ("ATTEMPTS", attempts),
            ("POLICY", policy),
            ("RETRIED", retried),
            ("NEVER RETRIED", never),
        ]
    } else {
        vec![
            ("TASK", task_lines),
            ("BUDGET", budget_lines),
            ("NEXT REQUEST · CONTEXT", context_lines),
            ("COST", cost_lines),
        ]
    };
    if reducer.task.status.is_none() {
        sections_data.push((
            "SESSION",
            vec![
                muted(&format!(
                    "model {}",
                    options.model.unwrap_or("not reported")
                )),
                muted(&format!(
                    "effort {}",
                    options.effort.unwrap_or("not reported")
                )),
                muted(&format!(
                    "thinking {}",
                    options.thinking.unwrap_or("not reported")
                )),
            ],
        ));
        sections_data.push((
            "TOOLS",
            vec![
                muted("read   allowed"),
                muted("write  asks each time"),
                muted("shell  asks each time"),
            ],
        ));
    }
    if let Some(request) = options.approval {
        sections_data.push((
            "PENDING",
            vec![
                Line::from(Span::styled(
                    format!("◇ {}", request.path),
                    fg(theme::AMBER).add_modifier(Modifier::BOLD),
                )),
                muted("y once   a for this session   n deny"),
            ],
        ));
        sections_data.push(("PERMISSIONS", vec![muted("write  asks each time")]));
        sections_data.push((
            "IF DENIED",
            vec![muted("the write is skipped; the agent continues")],
        ));
    }
    // Each bordered section consumes a bottom-rule row in addition to its
    // text. Account for that row so natural section heights do not overrun the
    // sidebar and push EVENTS below the viewport.
    let mut minimums = if show_retry {
        vec![4_u16, 5, 5, 3, 3, 3]
    } else {
        vec![4, 6, 5, 3, 3]
    };
    if !show_retry && options.approval.is_some() {
        minimums.insert(4, 3);
        minimums.insert(5, 3);
        minimums.insert(6, 3);
    }
    if !show_retry && reducer.task.status.is_none() {
        minimums.insert(4, 3);
        minimums.insert(5, 3);
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
    event_lines.extend(
        reducer
            .recent_events
            .iter()
            .skip(options.sidebar_scroll_back)
            .take(event_rows.saturating_sub(1).min(3))
            .map(|event| event_line(event)),
    );
    frame.render_widget(
        Paragraph::new(event_lines).style(Style::default().bg(theme::BG)),
        event_area,
    );
}

fn retry_attempt_lines(reducer: &Reducer, width: u16) -> Vec<Line<'static>> {
    let set = super::glyph_set();
    let policy = reducer.retry_policy;
    let mut lines = vec![heading(&format!("ATTEMPTS · MAX {}", policy.max_attempts))];
    let final_attempt = reducer.failure.as_ref().map(|failure| failure.attempts);
    let rows = final_attempt.unwrap_or(policy.max_attempts).max(1).max(
        reducer
            .retry_wait
            .as_ref()
            .map_or(1, |wait| wait.next_attempt),
    );
    for attempt in 1..=rows {
        let record = reducer
            .retries
            .iter()
            .find(|retry| retry.attempt == attempt);
        let waiting = reducer
            .retry_wait
            .as_ref()
            .filter(|wait| wait.next_attempt == attempt);
        let final_failure = reducer
            .failure
            .as_ref()
            .filter(|failure| failure.attempts == attempt && record.is_none());
        let (glyph, color, label, right) = if let Some(record) = record {
            (
                Glyph::Cross,
                theme::CORAL,
                format!("{attempt} · {}", record.kind),
                format!("{} wait", retry_seconds(record.delay_ms)),
            )
        } else if let Some(wait) = waiting {
            (
                Glyph::Spinner,
                theme::AMBER,
                format!("{attempt} · pending"),
                retry_seconds(wait.remaining_millis()),
            )
        } else if reducer.task.status == Some(TaskStatus::Running)
            && reducer.attempt == Some(attempt)
        {
            (
                Glyph::Spinner,
                theme::AMBER,
                format!("{attempt} · streaming"),
                String::new(),
            )
        } else if let Some(failure) = final_failure {
            let kind = match failure.kind {
                FailureKind::MaxTokens => "max_tokens",
                FailureKind::QuotaExhausted => "quota_exhausted",
                FailureKind::ContentFilter => "content_filter",
                FailureKind::ContextLimit => "context_limit",
                FailureKind::RetriesExhausted => failure.retry_kind.as_deref().unwrap_or("error"),
                FailureKind::Other => "error",
            };
            (
                Glyph::Cross,
                theme::CORAL,
                format!("{attempt} · {kind}"),
                if failure.kind == FailureKind::RetriesExhausted {
                    "gave up".to_owned()
                } else {
                    "not retried".to_owned()
                },
            )
        } else {
            (
                Glyph::Pending,
                theme::DIM,
                attempt.to_string(),
                if attempt == policy.max_attempts
                    && reducer.task.status == Some(TaskStatus::Running)
                {
                    "last chance".to_owned()
                } else {
                    String::new()
                },
            )
        };
        lines.push(aligned_spans(
            vec![
                Span::styled(format!("{} ", glyphs::glyph(glyph, set)), fg(color)),
                Span::styled(
                    label,
                    fg(color).add_modifier(if waiting.is_some() {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
            ],
            right,
            width,
        ));
    }
    lines
}

fn retry_policy_lines(reducer: &Reducer, width: u16) -> Vec<Line<'static>> {
    let policy = reducer.retry_policy;
    let backoff = format!(
        "{} × {}, cap {}",
        compact_seconds(policy.initial_backoff_ms),
        policy.backoff_factor,
        compact_seconds(policy.max_backoff_ms)
    );
    let jitter = format!("±{} %", policy.jitter_percent);
    let idle = policy
        .idle_timeout_ms
        .map_or_else(|| "not exposed".to_owned(), compact_seconds);
    vec![
        heading("POLICY"),
        aligned_spans(
            vec![Span::styled("backoff", fg(theme::DIM))],
            backoff,
            width,
        ),
        aligned_spans(vec![Span::styled("jitter", fg(theme::DIM))], jitter, width),
        aligned_spans(
            vec![Span::styled("retry_after", fg(theme::DIM))],
            "overrides delay".to_owned(),
            width,
        ),
        aligned_spans(
            vec![Span::styled("idle timeout", fg(theme::DIM))],
            idle,
            width,
        ),
    ]
}

fn kind_lines(retryable: bool, width: u16) -> Vec<Line<'static>> {
    use loom_core::kernel::model::ModelErrorKind;
    const KINDS: [ModelErrorKind; 10] = [
        ModelErrorKind::Auth,
        ModelErrorKind::InvalidRequest,
        ModelErrorKind::ContextOverflow,
        ModelErrorKind::RateLimited,
        ModelErrorKind::Overloaded,
        ModelErrorKind::QuotaExhausted,
        ModelErrorKind::Transport,
        ModelErrorKind::Server,
        ModelErrorKind::Protocol,
        ModelErrorKind::Cancelled,
    ];
    let names = KINDS
        .iter()
        .filter(|kind| kind.is_retryable() == retryable)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let color = if retryable { theme::SAGE } else { theme::CORAL };
    let mut lines = vec![heading(if retryable {
        "RETRIED"
    } else {
        "NEVER RETRIED"
    })];
    for row in wrap_tokens(&names, usize::from(width).max(8)) {
        lines.push(Line::from(Span::styled(row, fg(color))));
    }
    lines
}

fn aligned_spans(mut left: Vec<Span<'static>>, right: String, width: u16) -> Line<'static> {
    let left_width = left
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    let right_width = right.chars().count();
    let gap = usize::from(width)
        .saturating_sub(left_width + right_width)
        .max(1);
    left.push(Span::raw(" ".repeat(gap)));
    if !right.is_empty() {
        left.push(Span::styled(right, fg(theme::DIM)));
    }
    Line::from(left)
}

fn wrap_tokens(names: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for (index, name) in names.iter().enumerate() {
        let separator = if index == 0 { "" } else { " · " };
        if !current.is_empty() && current.chars().count() + separator.len() + name.len() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(separator);
        }
        current.push_str(name);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn compact_seconds(millis: u64) -> String {
    if millis.is_multiple_of(1_000) {
        format!("{} s", millis / 1_000)
    } else {
        format!("{}.{} s", millis / 1_000, millis % 1_000 / 100)
    }
}

fn retry_seconds(millis: u64) -> String {
    if millis >= 10_000 {
        format!("{} s", (millis + 500) / 1_000)
    } else {
        format!("{}.{} s", millis / 1_000, millis % 1_000 / 100)
    }
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
    let over_ninety = limit > 0 && used.saturating_mul(100) / limit >= 90;
    let color = if over_ninety {
        theme::CORAL
    } else {
        theme::AMBER
    };
    Line::from(vec![
        Span::styled("━".repeat(filled), fg(color)),
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
    sidebar_row(label, value, width, fg(theme::DIM), fg(theme::IVORY))
}

/// Idle rows follow the wireframe's `r()`: the label is normal text and only
/// the placeholder value (`no turns yet`) is dim.
fn sidebar_idle_row(label: &str, value: &str, width: u16, value_dim: bool) -> Line<'static> {
    sidebar_row(
        label,
        value,
        width,
        fg(theme::IVORY),
        fg(if value_dim { theme::DIM } else { theme::IVORY }),
    )
}

fn sidebar_row(
    label: &str,
    value: &str,
    width: u16,
    label_style: Style,
    value_style: Style,
) -> Line<'static> {
    let label_len = label.chars().count();
    let value_len = value.chars().count();
    let gap = usize::from(width)
        .saturating_sub(label_len.saturating_add(value_len))
        .max(2);
    Line::from(vec![
        Span::styled(label.to_owned(), label_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(value.to_owned(), value_style),
    ])
}

pub(crate) fn context_bar(census: ContextCensus, width: u16) -> Line<'static> {
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
        (
            "system + tools",
            census
                .system_prompt
                .saturating_add(census.repo_facts)
                .saturating_add(census.tool_specs),
            theme::DIM,
        ),
        ("user", census.user_items, theme::BLUE),
        ("reasoning", census.reasoning_replay, theme::VIOLET),
        ("assistant", census.assistant_history, theme::IVORY),
        ("tool results", census.tool_results, theme::SAGE),
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
    let label_width = max_label.min(inner_width.saturating_sub(count_width + 3));
    categories
        .into_iter()
        .map(|(label, tokens, color)| {
            Line::from(vec![
                Span::styled("■ ", fg(color)),
                Span::styled(format!("{label:<label_width$} "), fg(theme::IVORY)),
                Span::styled(
                    format!("{:>count_width$}", format_compact_tokens(u64::from(tokens))),
                    fg(theme::DIM),
                ),
            ])
        })
        .collect()
}

pub(super) fn event_line(event: &str) -> Line<'static> {
    muted(event)
}

pub(super) fn optional_budget(value: Option<u64>) -> String {
    value.map_or_else(
        || "unbounded".to_owned(),
        |remaining| format!("{remaining} remaining"),
    )
}
