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
    reducer::{Reducer, RestoredRun},
    theme,
};

use super::{Scene, TaskStatus, fg, format_compact_tokens, glyph_set, muted, tinted};

/// A tool with no result yet is either running or paused on the approval
/// box: arguments naming the paused write get the wireframe's needs-you
/// marker instead of a lying "running".
fn pending_state(
    approval_path: Option<&str>,
    arguments: &str,
) -> (Glyph, ratatui::style::Color, &'static str) {
    if approval_path.is_some_and(|path| !path.is_empty() && arguments.contains(path)) {
        (Glyph::Needs, theme::AMBER, "awaiting")
    } else {
        (Glyph::Spinner, theme::AMBER, "running")
    }
}

#[allow(clippy::too_many_lines, clippy::wildcard_enum_match_arm)] // transcript rendering preserves event and tool ordering; Item is non_exhaustive
pub(super) fn draw_reducer_thread(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    compact: bool,
    thread_scroll_back: usize,
    scene: Option<Scene>,
    approval_path: Option<&str>,
) {
    let mut lines = Vec::new();
    if !reducer.restored
        && let Some(prompt) = reducer
            .original_instruction
            .as_deref()
            .or_else(|| reducer.task.display_prompts.first().map(String::as_str))
    {
        lines.push(Line::from(vec![
            Span::styled("❯ ", fg(theme::AMBER)),
            Span::styled(prompt, fg(theme::IVORY).add_modifier(Modifier::BOLD)),
        ]));
    }
    if let Some(reason) = &reducer.task.failure_reason {
        lines.push(tinted(&format!("Task failed · {reason}"), theme::CORAL));
    }
    let mut rendered_retries = std::collections::BTreeSet::new();
    let mut rendered_call_ids = std::collections::BTreeSet::new();
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

    let mut last_run = None;
    for (turn_number, turn) in &reducer.execution_turns {
        if let Some(run_index) = turn.run
            && last_run != Some(run_index)
        {
            last_run = Some(run_index);
            if let Some(run) = reducer.restored_runs.get(&run_index) {
                lines.push(restored_run_heading(run, run_index));
            }
        }
        let display_turn = turn
            .engine_turn
            .or_else(|| turn.run.is_none().then_some(*turn_number));
        let turn_label =
            display_turn.map_or_else(|| "history".to_owned(), |number| format!("turn {number}"));
        if let Some((_, chars)) = reducer
            .compactions
            .iter()
            .find(|(turn, _)| turn == turn_number)
        {
            // Permanent marker: the model now sees a handoff summary, not
            // the raw turns above.
            lines.push(Line::from(vec![
                Span::styled("── ", fg(theme::THREAD)),
                Span::styled("compacted", fg(theme::AMBER)),
                Span::styled(
                    format!(" · {turn_label} · {chars} chars of handoff summary ──"),
                    fg(theme::DIM),
                ),
            ]));
        }
        let streaming = display_turn == reducer.turn && reducer.has_provisional_output();
        lines.push(turn_heading(
            *turn_number,
            display_turn,
            area.width.saturating_sub(1),
            streaming,
            reducer,
        ));
        for input in &turn.user_inputs {
            lines.push(Line::from(vec![
                Span::styled("❯ ", fg(theme::AMBER)),
                Span::styled(
                    input.as_str(),
                    fg(theme::IVORY).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        lines.push(Line::from(""));

        for (retry_index, retry) in reducer
            .retries
            .iter()
            .enumerate()
            .filter(|(_, retry)| retry.turn == Some(*turn_number))
        {
            rendered_retries.insert(retry_index);
            lines.push(Line::from(vec![
                Span::styled("", fg(theme::THREAD)),
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
                            format!("{indicator} reasoning · {size}"),
                            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                        )));
                        if reducer.expanded.reasoning
                            && !looks_like_structured_json(&reasoning.text)
                        {
                            lines.push(Line::from(Span::styled(
                                format!("   {}", reasoning.text),
                                fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                            )));
                        }
                    }
                    if !message.content.trim().is_empty() {
                        lines.extend(markdown_lines(&message.content));
                    }
                    for (call_index, call) in message.tool_calls.iter().enumerate() {
                        let call_id = call.id.to_string();
                        rendered_call_ids.insert(call_id.clone());
                        let state = reducer.tools.get(&call_id);
                        let (marker, color, label) = match state {
                            Some(tool) => match tool.status {
                                Some(ToolStatus::Success) => (Glyph::Check, theme::SAGE, "done"),
                                Some(ToolStatus::Error(_)) => {
                                    (Glyph::Cross, theme::CORAL, "failed")
                                }
                                Some(ToolStatus::Cancelled { .. }) => {
                                    (Glyph::Warning, theme::AMBER, "cancelled")
                                }
                                Some(_) => (Glyph::Check, theme::DIM, "finished"),
                                None => pending_state(approval_path, &call.arguments.to_string()),
                            },
                            None => pending_state(approval_path, &call.arguments.to_string()),
                        };
                        let duration = state.and_then(|tool| {
                            tool.started_at_millis
                                .zip(tool.completed_at_millis)
                                .map(|(start, end)| end.saturating_sub(start))
                        });
                        let timing = duration.map_or_else(String::new, |ms| format!(" · {ms}ms"));
                        let branch = if call_index + 1 == message.tool_calls.len() {
                            "└─ "
                        } else {
                            "├─ "
                        };
                        let status = format!("{label}{timing}");
                        let used = branch.chars().count()
                            + 2
                            + call.name.chars().count()
                            + status.chars().count()
                            + 7;
                        let arguments = truncate_chars(
                            &concise_arguments(&call.arguments.to_string()),
                            usize::from(area.width.saturating_sub(1))
                                .saturating_sub(used)
                                .max(6),
                        );
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
                            Span::styled(format!("  {arguments}  · {status}"), fg(theme::DIM)),
                        ]));
                        if reducer.expanded.tools {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "       args: {}",
                                    truncate_chars(&call.arguments.to_string(), 160)
                                ),
                                fg(theme::DIM),
                            )));
                        }
                    }
                }
                Item::ToolResult(result) => {
                    lines.extend(tool_result_lines(
                        result,
                        reducer.expanded.tools,
                        area.width,
                    ));
                }
                _ => {}
            }
        }
        let uncommitted = reducer
            .tools
            .iter()
            .filter(|(id, _)| {
                reducer.tool_call_turn(id) == Some(*turn_number)
                    && !transcript_tool_ids.contains(*id)
                    && !rendered_call_ids.contains(*id)
            })
            .collect::<Vec<_>>();
        for (index, (_id, tool)) in uncommitted.iter().enumerate() {
            let branch = if index + 1 == uncommitted.len() {
                "└─ "
            } else {
                "├─ "
            };
            let (marker, color, status) = match tool.status {
                Some(ToolStatus::Success) => (Glyph::Check, theme::SAGE, "done"),
                Some(ToolStatus::Error(_)) => (Glyph::Cross, theme::CORAL, "failed"),
                Some(ToolStatus::Cancelled { .. }) => (Glyph::Warning, theme::AMBER, "cancelled"),
                Some(_) => (Glyph::Check, theme::DIM, "finished"),
                None => pending_state(approval_path, tool.arguments.as_deref().unwrap_or("")),
            };
            let timing = tool
                .started_at_millis
                .zip(tool.completed_at_millis)
                .map(|(start, end)| end.saturating_sub(start))
                .map_or_else(String::new, |ms| format!(" · {ms}ms"));
            let detail = format!("{status}{timing}");
            let used =
                branch.chars().count() + 2 + tool.name.chars().count() + detail.chars().count() + 7;
            let arguments = truncate_chars(
                &concise_arguments(tool.arguments.as_deref().unwrap_or("")),
                usize::from(area.width.saturating_sub(1))
                    .saturating_sub(used)
                    .max(6),
            );
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
                Span::styled(format!("  {arguments}  · {detail}"), fg(theme::DIM)),
            ]));
            if reducer.expanded.tools
                && let Some(output) = &tool.content_head
            {
                lines.push(Line::from(Span::styled(
                    format!(
                        "       output preview: {}",
                        truncate_chars(&output.replace('\n', " ↵ "), 160)
                    ),
                    fg(color),
                )));
            }
        }
        // The model's prose lands after every execution in the turn.
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
                        "{} reasoning · provisional · streaming {}",
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
                        Span::styled("   ", fg(theme::THREAD)),
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
                // Render partial markdown too: a streaming answer used to sit
                // on a single wrapped line until the turn committed.
                let mut rendered = markdown_lines(&reducer.provisional.text);
                if rendered.is_empty() {
                    rendered.push(Line::from(""));
                }
                if let Some(last) = rendered.last_mut() {
                    last.spans.push(Span::styled(
                        streaming_cursor(),
                        fg(theme::AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
                lines.extend(rendered);
            }
        }
        lines.push(Line::from(""));
    }

    // A committed transcript may contain a fast-path result without model turns.
    if reducer.execution_turns.is_empty() {
        for item in &reducer.committed_transcript {
            if let Item::Assistant(message) = item {
                lines.extend(markdown_lines(&message.content));
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

#[allow(clippy::wildcard_enum_match_arm)] // TaskStatus is non_exhaustive
fn restored_run_heading(run: &RestoredRun, run_index: u32) -> Line<'static> {
    let (status, color) = match run.status {
        Some(TaskStatus::Completed) => ("completed", theme::SAGE),
        Some(TaskStatus::Failed) => ("failed", theme::CORAL),
        Some(TaskStatus::Cancelled) => ("cancelled", theme::DIM),
        Some(TaskStatus::Paused) => ("paused", theme::AMBER),
        Some(TaskStatus::Running) => ("interrupted", theme::AMBER),
        Some(_) => ("unknown", theme::DIM),
        None => ("history", theme::DIM),
    };
    let mut spans = vec![
        Span::styled("── ", fg(theme::THREAD)),
        Span::styled(format!("run {run_index}"), fg(theme::AMBER)),
        Span::styled(format!(" · {status}"), fg(color)),
    ];
    if run.turns > 0 {
        spans.push(Span::styled(
            format!(" · {} turns", run.turns),
            fg(theme::DIM),
        ));
    }
    if let Some(reason) = run
        .failure_reason
        .as_deref()
        .filter(|reason| !reason.trim().is_empty())
    {
        spans.push(Span::styled(
            format!(" · {}", truncate_chars(reason, 60)),
            fg(theme::CORAL),
        ));
    }
    spans.push(Span::styled(" ──", fg(theme::THREAD)));
    Line::from(spans)
}

pub(super) fn turn_heading(
    turn_key: u32,
    engine_turn: Option<u32>,
    width: u16,
    streaming: bool,
    reducer: &Reducer,
) -> Line<'static> {
    let metrics = reducer.turn_metrics.get(&turn_key);
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
    let state = if streaming {
        format!("  {} streaming", glyphs::glyph(Glyph::Spinner, glyph_set()))
    } else if reducer.task.status == Some(TaskStatus::Completed)
        && metrics.is_some_and(|metrics| metrics.completed_at_millis.is_some())
    {
        format!("  {} done", glyphs::glyph(Glyph::Check, glyph_set()))
    } else {
        String::new()
    };
    let label = engine_turn.map_or_else(|| "history".to_owned(), |turn| format!("turn {turn}"));
    let metadata = format!("{label}{usage}{cached}{duration}{state}");
    let metadata = truncate_chars(&metadata, usize::from(width).saturating_sub(3));
    Line::from(vec![
        Span::styled(
            format!("{} ", glyphs::glyph(Glyph::Bullet, glyph_set())),
            fg(theme::AMBER),
        ),
        Span::styled(metadata, fg(theme::DIM)),
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

/// Renders model prose as terminal lines: markdown syntax markers become
/// styles instead of literal characters. Unknown constructs pass through, so
/// a partial marker mid-stream stays visible until it closes.
pub(super) fn markdown_lines(raw: &str) -> Vec<Line<'static>> {
    let content = human_readable_content(raw);
    if content == "(structured output hidden)" {
        return vec![muted(&content)];
    }
    let mut lines = Vec::new();
    let mut in_code = false;
    for line in content.lines() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            lines.push(Line::from(vec![
                Span::styled(format!("{indent}  "), fg(theme::DIM)),
                Span::styled(trimmed.to_owned(), fg(theme::TEAL)),
            ]));
            continue;
        }
        if trimmed.is_empty() {
            lines.push(Line::from(""));
        } else if let Some(text) = heading_text(trimmed) {
            lines.push(Line::from(Span::styled(
                text.to_owned(),
                fg(theme::AMBER).add_modifier(Modifier::BOLD),
            )));
        } else if is_rule(trimmed) {
            lines.push(Line::from(Span::styled("─".repeat(28), fg(theme::THREAD))));
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled(format!("{indent}│ "), fg(theme::THREAD))];
            spans.extend(inline_spans(quote).into_iter().map(|span| {
                Span::styled(span.content, fg(theme::DIM).add_modifier(Modifier::ITALIC))
            }));
            lines.push(Line::from(spans));
        } else if is_table_separator(trimmed) {
            // Column widths are not computed: separators carry no content.
        } else if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
            let mut spans = Vec::new();
            for (index, cell) in table_cells(trimmed).into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" │ ", fg(theme::THREAD)));
                }
                spans.extend(inline_spans(cell));
            }
            lines.push(Line::from(spans));
        } else if let Some(body) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let mut spans = vec![Span::styled(format!("{indent}• "), fg(theme::AMBER))];
            spans.extend(inline_spans(body));
            lines.push(Line::from(spans));
        } else if let Some((marker, body)) = numbered_item(trimmed) {
            let mut spans = vec![Span::styled(format!("{indent}{marker} "), fg(theme::AMBER))];
            spans.extend(inline_spans(body));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(inline_spans(line)));
        }
    }
    lines
}

fn heading_text(text: &str) -> Option<&str> {
    let hashes = text
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6)
        .contains(&hashes)
        .then(|| text[hashes..].strip_prefix(' '))
        .flatten()
}

fn is_rule(text: &str) -> bool {
    let mut markers = text.chars().filter(|character| !character.is_whitespace());
    let Some(first) = markers.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_')
        && markers.clone().count() >= 2
        && markers.all(|character| character == first)
}

fn numbered_item(text: &str) -> Option<(&str, &str)> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    (digits > 0)
        .then(|| text[digits..].strip_prefix(". "))
        .flatten()
        .map(|body| (&text[..digits], body))
}

fn is_table_separator(text: &str) -> bool {
    text.starts_with('|')
        && text.ends_with('|')
        && text
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' '))
}

fn table_cells(text: &str) -> Vec<&str> {
    text.strip_prefix('|')
        .and_then(|row| row.strip_suffix('|'))
        .unwrap_or(text)
        .split('|')
        .map(str::trim)
        .collect()
}

/// Turns inline markdown into styled spans: `code`, **bold**, *italic*, and
/// links keep their text; the markers do not reach the screen.
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(after[..end].to_owned(), fg(theme::TEAL)));
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**").filter(|index| *index > 0)
        {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                after[..end].to_owned(),
                fg(theme::IVORY).add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find("](")
            && let Some(end) = after[close + 2..].find(')')
        {
            flush_plain(&mut spans, &mut plain);
            let label = &after[..close];
            spans.push(Span::styled(label.to_owned(), fg(theme::IVORY)));
            let url = &after[close + 2..close + 2 + end];
            if url != label {
                spans.push(Span::styled(format!(" ({url})"), fg(theme::DIM)));
            }
            rest = &after[close + 2 + end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*').filter(|index| *index > 0)
        {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                after[..end].to_owned(),
                fg(theme::IVORY).add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }
        plain.push(character);
        rest = &rest[character.len_utf8()..];
    }
    flush_plain(&mut spans, &mut plain);
    spans
}

fn flush_plain(spans: &mut Vec<Span<'static>>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), fg(theme::IVORY)));
    }
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
        ToolFailure::Declined => "declined",
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
pub(super) fn tool_result_lines(
    result: &ToolResult,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(read) = parse_numbered_read(&result.content) {
        let truncated = if result.truncated {
            " · truncated"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", fg(theme::DIM)),
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
                Span::styled(format!("    {number:>3} │ "), fg(theme::DIM)),
                Span::styled(truncate_chars(text, 80), fg(style)),
            ]));
        }
        if read.body.len() > cap {
            lines.push(Line::from(Span::styled(
                format!("    … +{} more lines", read.body.len() - cap),
                fg(theme::DIM),
            )));
        }
        if let Some(trailer) = read.trailer {
            lines.push(Line::from(Span::styled(
                format!("    {trailer}"),
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
        _ => (theme::DIM, Glyph::Check, "finished".to_owned()),
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
        Span::styled(
            truncate_chars(
                &tool_result_summary(&result.content),
                usize::from(width).saturating_sub(12),
            ),
            fg(color),
        ),
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
                Span::styled("    ", fg(theme::THREAD)),
                Span::styled(truncate_chars(line, 96), fg(theme::DIM)),
            ]));
        }
        if content_lines.len() > cap {
            lines.push(Line::from(Span::styled(
                format!("    … +{} more lines", content_lines.len() - cap),
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
    let inner_height = usize::from(area.height);
    let paragraph = Paragraph::new(lines)
        .style(Style::default().bg(theme::BG))
        .wrap(Wrap { trim: false });
    // Reserve the last column for the scrollbar. Without the reservation the
    // paragraph wraps to the full width and the scrollbar paints over the
    // final character of every full row.
    let full_width = usize::from(area.width.max(1));
    let mut text_width = full_width;
    let mut visual_lines = paragraph.line_count(area.width.max(1));
    let scrollbar = visual_lines > inner_height && area.width > 1;
    if scrollbar {
        text_width = full_width.saturating_sub(1).max(1);
        visual_lines = paragraph.line_count(u16::try_from(text_width).unwrap_or(u16::MAX));
    }
    let max_scroll = visual_lines.saturating_sub(inner_height);
    let scroll = u16::try_from(
        max_scroll
            .saturating_sub(thread_scroll_back)
            .min(usize::from(u16::MAX)),
    )
    .unwrap_or(u16::MAX);
    let text_area = Rect {
        width: u16::try_from(text_width).unwrap_or(area.width),
        ..area
    };
    frame.render_widget(paragraph.scroll((scroll, 0)), text_area);
    if scrollbar {
        let track = usize::from(area.height);
        let thumb = (inner_height * track / visual_lines.max(1)).clamp(1, track);
        let top = (usize::from(scroll) * track.saturating_sub(thumb))
            .checked_div(max_scroll)
            .unwrap_or(0);
        let buffer = frame.buffer_mut();
        for row in 0..track {
            let (symbol, style) = if row >= top && row < top + thumb {
                ("█", fg(theme::THREAD))
            } else {
                ("│", fg(theme::DOTTED))
            };
            buffer.set_string(
                area.right().saturating_sub(1),
                area.y + u16::try_from(row).unwrap_or(u16::MAX),
                symbol,
                style,
            );
        }
    }
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
