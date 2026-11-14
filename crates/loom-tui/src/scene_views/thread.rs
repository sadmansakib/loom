use loom_core::kernel::transcript::{Item, ToolFailure, ToolResult, ToolStatus};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{
    glyphs::{self, Glyph, GlyphSet},
    reducer::Reducer,
    theme,
};

use super::{Scene, TaskStatus, fg, format_compact_tokens, glyph_set, muted, tinted};

#[allow(clippy::too_many_lines, clippy::wildcard_enum_match_arm)] // transcript rendering preserves event and tool ordering; Item is non_exhaustive
pub(super) fn draw_reducer_thread(
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

pub(super) fn turn_heading(
    turn: u32,
    width: u16,
    streaming: bool,
    reducer: &Reducer,
) -> Line<'static> {
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

pub(super) fn concise_arguments(raw: &str) -> String {
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

pub(super) fn tool_result_summary(raw: &str) -> String {
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

pub(super) fn looks_like_structured_json(raw: &str) -> bool {
    let trimmed = raw.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
}

pub(super) fn human_readable_content(raw: &str) -> String {
    let trimmed = raw.trim();
    if looks_like_structured_json(trimmed) {
        return "(structured output hidden)".to_owned();
    }
    raw.to_owned()
}

/// Truncates to a character budget with an ellipsis marker.
pub(super) fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Block cursor shown while the model streams provisional output.
pub(super) fn streaming_cursor() -> &'static str {
    if matches!(glyph_set(), GlyphSet::Unicode) {
        "▌"
    } else {
        "|"
    }
}

/// Reasoning size prefers the provider-reported thinking tokens for the turn
/// and falls back to the character count of the streamed trace.
pub(super) fn reasoning_size(text: &str, reducer: &Reducer, turn: u32) -> String {
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

pub(super) fn failure_category(failure: ToolFailure) -> &'static str {
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
pub(super) struct NumberedRead<'a> {
    header: &'a str,
    body: Vec<(usize, &'a str)>,
    trailer: Option<&'a str>,
}

pub(super) fn parse_numbered_read(raw: &str) -> Option<NumberedRead<'_>> {
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

pub(super) fn comment_line(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with('#')
}

/// Renders one committed tool result: a status line with the honest category,
/// then a bounded, tool-aware preview (numbered file windows keep their
/// gutter; everything else summarizes to its first meaningful line).
pub(super) fn tool_result_lines(result: &ToolResult, expanded: bool) -> Vec<Line<'static>> {
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

pub(super) fn render_thread_scrolled(
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

pub(super) fn render_preview_thread(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
) {
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
