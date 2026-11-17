//! Live-task thread: committed turns, streaming output, tool batches and
//! result excerpts. This module reads reducer state only; it performs no I/O.

use std::collections::{BTreeMap, BTreeSet};

use loom_core::kernel::transcript::{Item, ToolCall, ToolResult, ToolStatus};
use ratatui::{
    Frame,
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
};

use crate::{
    glyphs::{self, Glyph},
    reducer::{Reducer, Selection},
    theme,
};

use super::{
    fg, glyph_set,
    thread::{self},
};

const READ_ONLY_TOOLS: [&str; 6] = ["read_file", "search", "list_dir", "grep", "glob", "read"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rail {
    Committed,
    Provisional,
}

impl Rail {
    fn glyph(self) -> Glyph {
        match self {
            Self::Committed => Glyph::Vertical,
            Self::Provisional => Glyph::Dotted,
        }
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            Self::Committed => theme::THREAD,
            Self::Provisional => theme::DOTTED,
        }
    }
}

#[allow(clippy::too_many_lines, clippy::wildcard_enum_match_arm)] // ordered rendering; Item is non-exhaustive
pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    thread_scroll_back: usize,
    approval_path: Option<&str>,
) {
    let set = glyph_set();
    let width = usize::from(area.width).max(1);
    let mut lines = Vec::new();

    if let Some(prompt) = reducer
        .original_instruction
        .as_deref()
        .or_else(|| reducer.task.display_prompts.first().map(String::as_str))
    {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", glyphs::glyph(Glyph::Prompt, set)),
                fg(theme::AMBER),
            ),
            Span::styled(prompt, fg(theme::IVORY).add_modifier(Modifier::BOLD)),
        ]));
    }
    let mut rendered_retries = BTreeSet::new();
    let mut rendered_call_ids = BTreeSet::new();
    let mut selected_anchor = None;
    let results = reducer
        .committed_transcript
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some((result.call_id.to_string(), result)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();

    for (turn_number, turn) in &reducer.execution_turns {
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
            lines.push(Line::from(vec![
                Span::styled("── ", fg(theme::THREAD)),
                Span::styled("compacted", fg(theme::AMBER)),
                Span::styled(
                    format!(" · {turn_label} · {chars} chars of handoff summary ──"),
                    fg(theme::DIM),
                ),
            ]));
        }

        let live = display_turn == reducer.turn
            && reducer.task.status == Some(loom_core::kernel::session::TaskStatus::Running);
        let wait = reducer
            .retry_wait
            .as_ref()
            .filter(|wait| wait.turn == Some(*turn_number));
        let retrying = live && wait.is_some();
        let streaming =
            live && (reducer.has_provisional_output() || reducer.provisional.attempt.is_some());
        let has_retries = reducer
            .retries
            .iter()
            .any(|retry| retry.turn == Some(*turn_number));
        let rail = if streaming || retrying || has_retries {
            Rail::Provisional
        } else {
            Rail::Committed
        };
        let turn_selected = matches!(
            reducer.selected,
            Some(Selection::Turn(selected)) if selected == *turn_number
        );
        if turn_selected {
            selected_anchor = Some(lines.len());
        }
        lines.push(turn_heading(
            *turn_number,
            display_turn,
            width.saturating_sub(2),
            streaming,
            retrying,
            turn_selected,
            rail,
            reducer,
            set,
        ));

        for input in &turn.user_inputs {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", glyphs::glyph(Glyph::Prompt, set)),
                    fg(theme::AMBER),
                ),
                Span::styled(
                    input.as_str(),
                    fg(theme::IVORY).add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        for (retry_index, retry) in reducer
            .retries
            .iter()
            .enumerate()
            .filter(|(_, retry)| retry.turn == Some(*turn_number))
        {
            rendered_retries.insert(retry_index);
            let discarded = retry
                .discarded_reasoning_chars
                .saturating_add(retry.discarded_text_chars);
            lines.push(rail_line(
                rail,
                vec![
                    Span::styled(
                        format!(
                            "{} attempt {} · discarded {discarded} reasoning chars · ",
                            glyphs::glyph(Glyph::Retry, set),
                            retry.attempt
                        ),
                        fg(theme::DIM),
                    ),
                    Span::styled(retry.kind.clone(), fg(theme::CORAL)),
                ],
            ));
        }
        if let Some(wait) = wait {
            lines.push(rail_line(
                rail,
                vec![
                    Span::styled(
                        format!(
                            "{} attempt {} · ",
                            glyphs::glyph(Glyph::Spinner, set),
                            wait.next_attempt
                        ),
                        fg(theme::AMBER),
                    ),
                    Span::styled(
                        format!("waiting {}", seconds(wait.remaining_millis())),
                        fg(theme::AMBER).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            " (backoff {} ±{}%)",
                            seconds(reducer.retry_policy.base_backoff_ms(wait.next_attempt)),
                            reducer.retry_policy.jitter_percent
                        ),
                        fg(theme::DIM),
                    ),
                ],
            ));
            lines.push(rail_line(rail, progress_spans(wait.progress(), width, set)));
        }

        for item in &turn.transcript {
            if let Item::Assistant(message) = item {
                if let Some(reasoning) = message
                    .reasoning
                    .as_ref()
                    .filter(|trace| !trace.text.is_empty())
                {
                    let expanded = live || reducer.expanded.reasoning;
                    let size = thread::reasoning_size(&reasoning.text, reducer, *turn_number);
                    let duration = turn_duration(reducer, *turn_number);
                    let fold = if expanded {
                        Glyph::Collapse
                    } else {
                        Glyph::Expand
                    };
                    lines.push(rail_line(
                        rail,
                        vec![Span::styled(
                            format!("{} reasoning · {size}{duration}", glyphs::glyph(fold, set)),
                            fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                        )],
                    ));
                    if expanded {
                        let text = if thread::looks_like_structured_json(&reasoning.text) {
                            "(structured output hidden)".to_owned()
                        } else {
                            reasoning.text.clone()
                        };
                        lines.extend(rail_wrapped(rail, &text, width, theme::VIOLET));
                    }
                }
                if !message.content.trim().is_empty() {
                    for line in thread::markdown_lines(&message.content) {
                        lines.push(rail_existing(rail, line));
                    }
                }
                lines.extend(render_tool_batch(
                    reducer,
                    rail,
                    width,
                    Some(&message.tool_calls),
                    approval_path,
                    &results,
                    &mut rendered_call_ids,
                    &mut selected_anchor,
                    lines.len(),
                    set,
                ));
            }
        }

        let uncommitted = reducer
            .tools
            .iter()
            .filter(|(id, _)| {
                reducer.tool_call_turn(id) == Some(*turn_number) && !rendered_call_ids.contains(*id)
            })
            .collect::<Vec<_>>();
        if !uncommitted.is_empty() {
            lines.push(rail_line(
                rail,
                vec![Span::styled(
                    batch_label(
                        uncommitted.len(),
                        uncommitted
                            .iter()
                            .all(|(_, tool)| READ_ONLY_TOOLS.contains(&tool.name.as_str())),
                    ),
                    fg(theme::DIM),
                )],
            ));
            for (index, (call_id, tool)) in uncommitted.iter().enumerate() {
                let last = index + 1 == uncommitted.len();
                let selected = matches!(
                    &reducer.selected,
                    Some(Selection::ToolCall { call_id: selected, .. }) if selected == *call_id
                );
                if selected {
                    selected_anchor = Some(lines.len());
                }
                lines.extend(render_live_tool_row(
                    reducer,
                    rail,
                    width,
                    tool.name.as_str(),
                    tool.arguments.as_deref().unwrap_or(""),
                    tool.status,
                    tool.content_head.as_deref(),
                    duration_between(tool.started_at_millis, tool.completed_at_millis),
                    last,
                    selected,
                    call_id,
                    approval_path.is_some_and(|path| {
                        !path.is_empty()
                            && tool
                                .arguments
                                .as_deref()
                                .is_some_and(|args| args.contains(path))
                    }),
                    set,
                ));
            }
        }

        if live {
            if !reducer.provisional.reasoning.is_empty() {
                let size =
                    thread::reasoning_size(&reducer.provisional.reasoning, reducer, *turn_number);
                lines.push(rail_line(
                    rail,
                    vec![Span::styled(
                        format!("{} reasoning · {size}", glyphs::glyph(Glyph::Collapse, set)),
                        fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
                    )],
                ));
                lines.extend(rail_wrapped_with_cursor(
                    rail,
                    &reducer.provisional.reasoning,
                    width,
                    theme::VIOLET,
                    set,
                ));
            }
            if !reducer.provisional.text.is_empty() {
                let mut rendered = thread::markdown_lines(&reducer.provisional.text);
                if rendered.is_empty() {
                    rendered.push(Line::from(""));
                }
                if let Some(last) = rendered.last_mut() {
                    last.spans.push(Span::styled(
                        streaming_cursor(set),
                        fg(theme::AMBER).add_modifier(Modifier::BOLD),
                    ));
                }
                lines.extend(rendered.into_iter().map(|line| rail_existing(rail, line)));
            }
        }

        lines.push(Line::from(""));
    }

    if reducer.execution_turns.is_empty() {
        for item in &reducer.committed_transcript {
            if let Item::Assistant(message) = item {
                lines.extend(thread::markdown_lines(&message.content));
            }
        }
    }
    for (retry_index, retry) in reducer.retries.iter().enumerate() {
        if !rendered_retries.contains(&retry_index) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{} attempt {}",
                        glyphs::glyph(Glyph::Retry, set),
                        retry.attempt
                    ),
                    fg(theme::CORAL),
                ),
                Span::styled(format!(" · {} · discarded", retry.kind), fg(theme::DIM)),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No task transcript yet. Enter a task to start.",
            fg(theme::DIM),
        )));
    }
    if reducer.task.status == Some(loom_core::kernel::session::TaskStatus::Running) {
        for prompt in reducer.task.display_prompts.iter().skip(1) {
            lines.push(Line::from(vec![
                Span::styled("  ↳ queued · ", fg(theme::AMBER)),
                Span::styled(prompt.as_str(), fg(theme::IVORY)),
            ]));
        }
    }
    if reducer.follow_up_queued && reducer.task.display_prompts.len() <= 1 {
        lines.push(Line::from(Span::styled(
            "follow-up queued for next turn",
            fg(theme::AMBER),
        )));
    }

    thread::render_thread_scrolled_to(frame, area, lines, thread_scroll_back, selected_anchor);
}

#[allow(clippy::too_many_arguments)] // one heading line assembled from reducer state
fn turn_heading(
    turn_key: u32,
    engine_turn: Option<u32>,
    width: usize,
    streaming: bool,
    retrying: bool,
    selected: bool,
    rail: Rail,
    reducer: &Reducer,
    set: crate::glyphs::GlyphSet,
) -> Line<'static> {
    let label = engine_turn.map_or_else(|| "history".to_owned(), |turn| format!("turn {turn}"));
    let mut spans = vec![
        Span::styled(
            format!("{} ", glyphs::glyph(Glyph::Bullet, set)),
            fg(theme::AMBER),
        ),
        Span::styled(
            label,
            fg(if selected { theme::AMBER } else { theme::DIM }).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
    ];
    if streaming {
        let attempt = reducer
            .provisional
            .attempt
            .map_or(String::new(), |attempt| format!(" · attempt {attempt}"));
        spans.push(Span::styled(
            format!(
                "  {} streaming{attempt}",
                glyphs::glyph(Glyph::Spinner, set)
            ),
            fg(theme::AMBER),
        ));
    } else if retrying && let Some(wait) = &reducer.retry_wait {
        spans.push(Span::styled(
            format!(
                "  {} retry {} / {}",
                glyphs::glyph(Glyph::Retry, set),
                wait.next_attempt,
                reducer.retry_policy.max_attempts
            ),
            fg(theme::AMBER),
        ));
    }
    let left = spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    let right = if retrying {
        "nothing from failed attempts is committed".to_owned()
    } else {
        turn_usage(reducer, turn_key, streaming)
    };
    let gap = width
        .saturating_sub(left)
        .saturating_sub(right.chars().count())
        .saturating_sub(2);
    spans.push(Span::styled(
        format!(" {} ", glyphs::glyph(Glyph::Horizontal, set).repeat(gap)),
        fg(theme::BORDER),
    ));
    spans.push(Span::styled(right, fg(theme::DIM)));
    rail_line(rail, spans)
}

fn turn_usage(reducer: &Reducer, turn_key: u32, streaming: bool) -> String {
    if streaming {
        return "provisional · discarded on retry".to_owned();
    }
    let Some(metrics) = reducer.turn_metrics.get(&turn_key) else {
        return String::new();
    };
    if !metrics.usage_reported {
        return String::new();
    }
    let usage = metrics.usage;
    let input = usage
        .cached_input_tokens
        .filter(|cached| *cached > 0)
        .map_or_else(
            || format!("{} in", format_count(usage.input_tokens)),
            |cached| {
                format!(
                    "{} in ({} cached)",
                    format_count(usage.input_tokens),
                    format_count(cached)
                )
            },
        );
    let mut parts = vec![input];
    if usage.output_tokens > 0 {
        parts.push(format!("{} out", format_count(usage.output_tokens)));
    }
    if let (Some(start), Some(end)) = (metrics.started_at_millis, metrics.completed_at_millis) {
        parts.push(seconds(end.saturating_sub(start)));
    }
    parts.join(" · ")
}

fn turn_duration(reducer: &Reducer, turn: u32) -> String {
    reducer
        .turn_metrics
        .get(&turn)
        .and_then(|metrics| {
            Some(seconds(
                metrics
                    .completed_at_millis?
                    .saturating_sub(metrics.started_at_millis?),
            ))
        })
        .map_or(String::new(), |duration| format!(" · {duration}"))
}

fn progress_spans(progress: f64, width: usize, set: crate::glyphs::GlyphSet) -> Vec<Span<'static>> {
    let bar_width = width.saturating_sub(6).max(4);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((progress.clamp(0.0, 1.0) * bar_width as f64).round() as usize).min(bar_width);
    let glyph = glyphs::glyph(Glyph::Horizontal, set);
    vec![
        Span::styled(format!("    {}", glyph.repeat(filled)), fg(theme::AMBER)),
        Span::styled(glyph.repeat(bar_width - filled), fg(theme::DOTTED)),
    ]
}

fn seconds(millis: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let seconds = millis as f64 / 1000.0;
    if seconds >= 10.0 {
        format!("{seconds:.0} s")
    } else {
        format!("{seconds:.1} s")
    }
}

fn format_count(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

fn batch_label(count: usize, read_only: bool) -> String {
    let noun = if read_only {
        "read-only call"
    } else {
        "tool call"
    };
    let plural = if count == 1 { "" } else { "s" };
    let concurrent = if count > 1 { " · concurrent" } else { "" };
    format!("{count} {noun}{plural}{concurrent}")
}

#[allow(clippy::too_many_arguments)] // one row of the event stream
fn render_tool_batch(
    reducer: &Reducer,
    rail: Rail,
    width: usize,
    calls: Option<&[ToolCall]>,
    approval_path: Option<&str>,
    results: &BTreeMap<String, &ToolResult>,
    rendered: &mut BTreeSet<String>,
    selected_anchor: &mut Option<usize>,
    line_base: usize,
    set: crate::glyphs::GlyphSet,
) -> Vec<Line<'static>> {
    let Some(calls) = calls.filter(|calls| !calls.is_empty()) else {
        return Vec::new();
    };
    let mut lines = vec![rail_line(
        rail,
        vec![Span::styled(
            batch_label(
                calls.len(),
                calls
                    .iter()
                    .all(|call| READ_ONLY_TOOLS.contains(&call.name.as_str())),
            ),
            fg(theme::DIM),
        )],
    )];
    for (index, call) in calls.iter().enumerate() {
        let call_id = call.id.to_string();
        rendered.insert(call_id.clone());
        let state = reducer.tools.get(&call_id);
        let result = results.get(&call_id).copied();
        let status = state
            .and_then(|tool| tool.status)
            .or(result.map(|r| r.status));
        let content = state
            .and_then(|tool| tool.content_head.as_deref())
            .or(result.map(|result| result.content.as_str()));
        let selected = matches!(
            &reducer.selected,
            Some(Selection::ToolCall { call_id: selected, .. }) if selected == &call_id
        );
        if selected {
            *selected_anchor = Some(line_base + lines.len());
        }
        lines.extend(render_live_tool_row(
            reducer,
            rail,
            width,
            call.name.as_str(),
            &call.arguments.to_string(),
            status,
            content,
            state.and_then(|tool| {
                duration_between(tool.started_at_millis, tool.completed_at_millis)
            }),
            index + 1 == calls.len(),
            selected,
            &call_id,
            approval_path
                .is_some_and(|path| !path.is_empty() && call.arguments.to_string().contains(path)),
            set,
        ));
    }
    lines
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one row plus optional excerpt
fn render_live_tool_row(
    reducer: &Reducer,
    rail: Rail,
    width: usize,
    name: &str,
    raw_arguments: &str,
    status: Option<ToolStatus>,
    content: Option<&str>,
    duration: Option<u64>,
    last: bool,
    selected: bool,
    call_id: &str,
    awaiting: bool,
    set: crate::glyphs::GlyphSet,
) -> Vec<Line<'static>> {
    let (marker, color, running) = if awaiting && status.is_none() {
        (Glyph::Needs, theme::AMBER, false)
    } else {
        match status {
            Some(ToolStatus::Success) => (Glyph::Check, theme::SAGE, false),
            Some(ToolStatus::Error(_)) => (Glyph::Cross, theme::CORAL, false),
            Some(ToolStatus::Cancelled { started: true }) => (Glyph::Warning, theme::AMBER, false),
            Some(ToolStatus::Cancelled { started: false }) => (Glyph::Pending, theme::DIM, false),
            Some(_) => (Glyph::Check, theme::DIM, false),
            None => (Glyph::Spinner, theme::AMBER, true),
        }
    };
    let branch = if last {
        Glyph::BranchLast
    } else {
        Glyph::Branch
    };
    let argument = tool_argument(raw_arguments);
    let mut summary = tool_summary(name, status, content, running);
    if awaiting && status.is_none() {
        "awaiting".clone_into(&mut summary);
    }
    let duration = duration.map_or(String::new(), |millis| format!("{millis} ms"));
    let right_width = summary.chars().count()
        + if duration.is_empty() {
            0
        } else {
            duration.chars().count() + 2
        };
    let fixed = 2
        + glyphs::glyph(branch, set).chars().count()
        + 1
        + glyphs::glyph(marker, set).chars().count()
        + 1
        + name.chars().count()
        + 2;
    let arg_limit = width.saturating_sub(fixed + right_width + 2).max(4);
    let argument = thread::truncate_chars(&argument, arg_limit);
    let gap = width
        .saturating_sub(fixed + argument.chars().count() + right_width)
        .max(1);
    let summary_color = if matches!(status, Some(ToolStatus::Error(_))) {
        theme::CORAL
    } else if awaiting && status.is_none() {
        theme::AMBER
    } else {
        theme::DIM
    };
    let mut spans = vec![
        rail_span(rail),
        Span::styled(
            format!("{} ", glyphs::glyph(branch, set)),
            fg(if selected { theme::AMBER } else { theme::DIM }),
        ),
        Span::styled(format!("{} ", glyphs::glyph(marker, set)), fg(color)),
        Span::styled(
            name.to_owned(),
            fg(theme::BLUE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {argument}"), fg(theme::IVORY)),
        Span::styled(" ".repeat(gap), fg(theme::BG)),
        Span::styled(summary, fg(summary_color)),
    ];
    if !duration.is_empty() {
        spans.push(Span::styled(format!("  {duration}"), fg(theme::DIM)));
    }
    let mut lines = vec![Line::from(spans)];

    if matches!(status, Some(ToolStatus::Error(_)))
        && let Some(message) = content.and_then(first_error_line)
    {
        lines.push(rail_line(
            rail,
            vec![Span::styled(
                format!("  model sees: error: {message}"),
                fg(theme::DIM),
            )],
        ));
    }

    let expanded = reducer.expanded_result.as_deref() == Some(call_id);
    if selected || expanded {
        lines.extend(result_excerpt(rail, width, content, expanded, set));
    }
    lines
}

fn tool_summary(
    name: &str,
    status: Option<ToolStatus>,
    content: Option<&str>,
    running: bool,
) -> String {
    if let Some(ToolStatus::Error(failure)) = status {
        return failure_title(failure);
    }
    if running {
        return String::new();
    }
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
        return String::new();
    };
    match name {
        "read_file" | "read" => numbered_summary(content).map_or_else(
            || first_line(content),
            |(range, bytes)| format!("{range} · {bytes}"),
        ),
        "list_dir" => {
            let entries = content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            format!("{entries} entries")
        }
        _ => first_line(content),
    }
}

fn numbered_summary(content: &str) -> Option<(String, String)> {
    let (header, body) = content.split_once('\n')?;
    let lines = header.trim_end().strip_suffix(" lines")?;
    let count = lines.rsplit_once(' ')?.1.parse::<usize>().ok()?;
    let mut first = None;
    let mut last = None;
    for line in body.lines() {
        let Some((number, _)) = line.split_once('\t') else {
            continue;
        };
        let Ok(number) = number.trim().parse::<usize>() else {
            continue;
        };
        first.get_or_insert(number);
        last = Some(number);
    }
    let (first, last) = (first?, last?);
    let bytes = content.len();
    Some((format!("{first}-{last} of {count}"), format_bytes(bytes)))
}

fn format_bytes(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn first_line(content: &str) -> String {
    content
        .lines()
        .find(|line| !line.trim().is_empty())
        .map_or_else(String::new, |line| thread::truncate_chars(line, 48))
}

fn first_error_line(content: &str) -> Option<String> {
    let line = content.lines().find(|line| !line.trim().is_empty())?;
    Some(line.trim().trim_start_matches("error:").trim().to_owned())
}

fn failure_title(failure: loom_core::kernel::transcript::ToolFailure) -> String {
    use loom_core::kernel::transcript::ToolFailure;
    match failure {
        ToolFailure::UnknownTool => "UnknownTool",
        ToolFailure::NotExposed => "NotExposed",
        ToolFailure::InvalidInput => "InvalidInput",
        ToolFailure::Execution => "Execution",
        ToolFailure::Timeout => "Timeout",
        ToolFailure::RepeatedCall => "RepeatedCall",
        ToolFailure::Declined => "Declined",
        _ => "Error",
    }
    .to_owned()
}

fn tool_argument(raw: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
        && let Some(object) = value.as_object()
    {
        for key in ["path", "pattern", "query", "command"] {
            if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
                return value.to_owned();
            }
        }
    }
    thread::concise_arguments(raw)
}

fn result_excerpt(
    rail: Rail,
    width: usize,
    content: Option<&str>,
    expanded: bool,
    set: crate::glyphs::GlyphSet,
) -> Vec<Line<'static>> {
    let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
        return Vec::new();
    };
    let rows = numbered_rows(content);
    let total = rows.len();
    let cap = if expanded { 16 } else { 3 };
    let mut lines = Vec::new();
    if let Some(header) = content
        .lines()
        .next()
        .filter(|line| line.trim_end().ends_with(" lines"))
    {
        lines.push(rail_line(
            rail,
            vec![Span::styled(format!("  {}", header.trim()), fg(theme::DIM))],
        ));
    }
    for (number, text) in rows.iter().take(cap) {
        let style = if text.trim_start().starts_with("//") || text.trim_start().starts_with('#') {
            theme::DIM
        } else {
            theme::IVORY
        };
        lines.push(rail_line(
            rail,
            vec![
                Span::styled(format!("  {number} │ "), fg(theme::DIM)),
                Span::styled(
                    thread::truncate_chars(text, width.saturating_sub(8)),
                    fg(style),
                ),
            ],
        ));
    }
    let remaining = total.saturating_sub(cap);
    let action = if expanded {
        format!("{} collapse", glyphs::glyph(Glyph::Collapse, set))
    } else {
        format!("{} expand", glyphs::glyph(Glyph::Expand, set))
    };
    let marker = if remaining == 0 {
        format!("  ^O {action}")
    } else {
        format!("  … {remaining} more lines · ^O {action}")
    };
    lines.push(rail_line(rail, vec![Span::styled(marker, fg(theme::DIM))]));
    lines
}

fn numbered_rows(content: &str) -> Vec<(usize, String)> {
    let mut rows = Vec::new();
    let mut lines = content.lines();
    let mut first = true;
    for line in lines.by_ref() {
        if first && line.trim_end().ends_with(" lines") {
            first = false;
            continue;
        }
        first = false;
        if let Some((number, text)) = line.split_once('\t')
            && let Ok(number) = number.trim().parse::<usize>()
        {
            rows.push((number, text.to_owned()));
            continue;
        }
        let number = rows.len() + 1;
        rows.push((number, line.to_owned()));
    }
    rows
}

fn duration_between(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    start.zip(end).map(|(start, end)| end.saturating_sub(start))
}

fn rail_span(rail: Rail) -> Span<'static> {
    Span::styled(
        format!("{} ", glyphs::glyph(rail.glyph(), glyph_set())),
        fg(rail.color()),
    )
}

fn rail_line(rail: Rail, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = Vec::with_capacity(spans.len() + 1);
    all.push(rail_span(rail));
    all.extend(spans);
    Line::from(all)
}

fn rail_existing(rail: Rail, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(rail_span(rail));
    spans.extend(line.spans);
    Line::from(spans)
}

fn rail_wrapped(
    rail: Rail,
    text: &str,
    width: usize,
    color: ratatui::style::Color,
) -> Vec<Line<'static>> {
    wrap_text(text, width.saturating_sub(2))
        .into_iter()
        .map(|line| {
            rail_line(
                rail,
                vec![Span::styled(line, fg(color).add_modifier(Modifier::ITALIC))],
            )
        })
        .collect()
}

fn rail_wrapped_with_cursor(
    rail: Rail,
    text: &str,
    width: usize,
    color: ratatui::style::Color,
    set: crate::glyphs::GlyphSet,
) -> Vec<Line<'static>> {
    let mut lines = rail_wrapped(rail, text, width, color);
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::styled(
            streaming_cursor(set),
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        ));
    }
    lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.lines() {
        if logical.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in logical.split_whitespace() {
            let word_width = word.chars().count();
            if !line.is_empty() && line.chars().count() + 1 + word_width > width {
                lines.push(line.clone());
                line.clear();
            }
            if word_width > width {
                for chunk in word.chars().collect::<Vec<_>>().chunks(width) {
                    if !line.is_empty() {
                        lines.push(line.clone());
                        line.clear();
                    }
                    line.extend(chunk);
                    if line.chars().count() >= width {
                        lines.push(line.clone());
                        line.clear();
                    }
                }
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn streaming_cursor(set: crate::glyphs::GlyphSet) -> &'static str {
    if set == crate::glyphs::GlyphSet::Unicode {
        "▌"
    } else {
        "|"
    }
}
