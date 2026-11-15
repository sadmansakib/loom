//! The `/context` inspector: what the next request will contain.
//!
//! Reads the live reducer for the census and cache split, and the
//! session transcript for the largest tool results — every number shown
//! is derived from engine state, never re-estimated by the provider.
//! Pin/drop actions go through the engine's pruning policy.

use loom_core::Session;
use loom_core::kernel::context::{ContextCensus, PruneOps};
use loom_core::kernel::transcript::Item;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::reducer::Reducer;
use crate::theme;

/// One tool result the operator can pin or drop, in transcript order.
pub(crate) struct ContextItem {
    pub(crate) call_id: String,
    /// Tool name plus its primary argument, e.g. `read_file agent/mod.rs`.
    pub(crate) label: String,
    /// Estimated tokens (the kernel's bytes ÷ 3 heuristic).
    pub(crate) tokens: u64,
    pub(crate) pinned: bool,
    pub(crate) dropped: bool,
}

/// The largest tool results in `session`, biggest first — the wireframe's
/// LARGEST ITEMS list. `limit` caps the rows the modal can show.
pub(crate) fn items(session: &Session, ops: &PruneOps, limit: usize) -> Vec<ContextItem> {
    let mut labels: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for item in &session.transcript {
        let Item::Assistant(message) = item else {
            continue;
        };
        for call in &message.tool_calls {
            let arguments: serde_json::Value = call.arguments.clone().into();
            let primary = arguments
                .get("path")
                .or_else(|| arguments.get("command"))
                .and_then(serde_json::Value::as_str)
                .map_or_else(String::new, |text| {
                    format!(" {}", crate::recent::first_line(text, 32))
                });
            labels.insert(call.id.as_str(), format!("{}{primary}", call.name));
        }
    }
    let mut rows: Vec<ContextItem> = session
        .transcript
        .iter()
        .filter_map(|item| {
            let Item::ToolResult(result) = item else {
                return None;
            };
            let call_id = result.call_id.as_str().to_owned();
            Some(ContextItem {
                pinned: ops.pinned.contains(&call_id),
                dropped: ops.dropped.contains(&call_id),
                label: labels
                    .get(result.call_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| "tool result".into()),
                tokens: u64::try_from(result.content.len() / 3).unwrap_or(u64::MAX),
                call_id,
            })
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.label.cmp(&right.label))
    });
    rows.truncate(limit);
    rows
}

type CensusField = fn(&ContextCensus) -> u32;

/// The wireframe's category rows: label, census field, swatch color.
const BREAKDOWN: [(&str, CensusField, ratatui::style::Color); 5] = [
    (
        "system + tools",
        |c| c.system_prompt + c.tool_specs,
        theme::SAGE,
    ),
    ("user", |c| c.user_items, theme::IVORY),
    ("reasoning", |c| c.reasoning_replay, theme::VIOLET),
    ("assistant", |c| c.assistant_history, theme::BLUE),
    ("tool results", |c| c.tool_results, theme::TEAL),
];

/// Render the inspector modal over the dimmed live view.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    reducer: &Reducer,
    items: &[ContextItem],
    selected: usize,
) {
    let height = area.height.clamp(12, 26);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .title(Span::styled(
                " /context ",
                Style::default()
                    .fg(theme::AMBER)
                    .add_modifier(Modifier::BOLD),
            )),
        modal,
    );
    let inner = modal.inner(ratatui::layout::Margin::new(1, 1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(2)])
        .split(inner);
    let dim = Style::default().fg(theme::DIM);
    let body = body_lines(reducer, items, selected);
    frame.render_widget(Paragraph::new(body), rows[0]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "RULES dropping keeps the call line; the model sees [pruned] and can re-run it",
                dim,
            )),
            Line::from(Span::styled(
                "↑↓ move   p pin   x drop   c compact next task   Esc back",
                dim,
            )),
        ]),
        rows[1],
    );
}

/// The modal body: the census breakdown, then the operator's largest
/// tool results with pin/drop state. Split from `draw` so each half
/// stays one screenful of logic.
fn body_lines(reducer: &Reducer, items: &[ContextItem], selected: usize) -> Vec<Line<'static>> {
    let amber = Style::default().fg(theme::AMBER);
    let dim = Style::default().fg(theme::DIM);
    let ivory = Style::default().fg(theme::IVORY);
    let mut body = match reducer.context_progress.as_ref() {
        Some(progress) => {
            let census = &progress.census;
            let budget = progress.context_budget.max_input_tokens;
            let mut lines = vec![Line::from(Span::styled("BREAKDOWN", amber))];
            for (label, field, color) in BREAKDOWN {
                let tokens = field(census);
                lines.push(Line::from(vec![
                    Span::styled("■ ", Style::default().fg(color)),
                    Span::styled(format!("{label:<16}"), ivory),
                    Span::styled(tokens_label(u64::from(tokens)), dim),
                    Span::styled(format!("  {:>3}%", share(tokens, census.total)), dim),
                ]));
            }
            let total_line = format!(
                "total (est.) {} · {}% of {} input budget",
                tokens_label(u64::from(census.total)),
                percent_1dp(u64::from(census.total), u64::from(budget)),
                tokens_label(u64::from(budget)),
            );
            lines.push(Line::from(Span::styled(total_line, amber)));
            if let Some((cached, share)) = cached_split(reducer) {
                lines.push(Line::from(vec![
                    Span::styled("cacheable prefix ", ivory),
                    Span::styled(
                        format!("{} · {share}% of last request cached", tokens_label(cached)),
                        Style::default().fg(theme::SAGE),
                    ),
                ]));
            }
            lines
        }
        None => vec![
            Line::from(Span::styled("BREAKDOWN", amber)),
            Line::from(Span::styled(
                "no compiled request yet — run a task to see the census",
                dim,
            )),
        ],
    };
    body.push(Line::from(""));
    body.push(Line::from(Span::styled("LARGEST ITEMS", amber)));
    if items.is_empty() {
        body.push(Line::from(Span::styled(
            "no tool results in this session yet",
            dim,
        )));
    }
    for (position, item) in items.iter().enumerate() {
        let (status, tone) = if item.dropped {
            ("dropped", theme::CORAL)
        } else if item.pinned {
            ("pinned", theme::SAGE)
        } else {
            ("—", theme::DIM)
        };
        let style = if position == selected {
            Style::default().fg(theme::IVORY).bg(theme::PANEL)
        } else {
            ivory
        };
        body.push(Line::from(vec![
            Span::styled(if position == selected { "▸ " } else { "  " }, amber),
            Span::styled(format!("{:<28}", item.label), style),
            Span::styled(
                format!("{:>7}  ", tokens_label(item.tokens)),
                Style::default().fg(theme::BLUE),
            ),
            Span::styled(status, Style::default().fg(tone)),
        ]));
    }
    body
}

/// Cached input tokens and their whole-number share of the last request,
/// when a summary reported both halves. Integer math keeps clippy's
/// lossy-cast lint at bay without fudging token counts.
fn cached_split(reducer: &Reducer) -> Option<(u64, u64)> {
    let summary = reducer.summary.as_ref()?;
    let cached = summary.cached_input_tokens?;
    if summary.input_tokens == 0 {
        return None;
    }
    let input = u128::from(summary.input_tokens);
    let pct = (u128::from(cached) * 100 + input / 2) / input;
    Some((cached, u64::try_from(pct).unwrap_or(100)))
}

fn tokens_label(tokens: u64) -> String {
    if tokens >= 1000 {
        let tenths = (tokens * 10 + 500) / 1000;
        format!("{}.{}K", tenths / 10, tenths % 10)
    } else {
        format!("{tokens}")
    }
}

fn share(part: u32, total: u32) -> u32 {
    if total == 0 {
        return 0;
    }
    u32::try_from(u128::from(part) * 100 / u128::from(total)).unwrap_or(100)
}

fn percent_1dp(part: u64, total: u64) -> String {
    if total == 0 {
        return "0.0".into();
    }
    let tenths = u128::from(part) * 1000 / u128::from(total);
    format!("{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::kernel::agent::RemainingBudget;
    use ratatui::backend::TestBackend;

    fn reducer_with_progress() -> Reducer {
        let mut reducer = Reducer::default();
        // The kernel structs are non_exhaustive; deserialize them the way
        // the reducer tests do.
        let census: ContextCensus = serde_json::from_value(serde_json::json!({
            "system_prompt": 2_300,
            "repo_facts": 0,
            "user_items": 100,
            "assistant_history": 400,
            "reasoning_replay": 900,
            "tool_results": 12_700,
            "tool_specs": 400,
            "total": 16_800
        }))
        .expect("census");
        reducer.context_progress = Some(crate::reducer::ContextProgress {
            turn: 2,
            context_budget: serde_json::from_value(serde_json::json!({
                "max_input_tokens": 131_072,
                "reserved_output_tokens": 8_192
            }))
            .expect("budget"),
            census,
            remaining_budget: RemainingBudget::default(),
            elapsed_at_compile: None,
            tool_calls_used: 45,
        });
        let task = "01a1df62-c545-7366-a8a5-4c3992e63bce"
            .parse::<loom_core::TaskId>()
            .expect("task id");
        let mut summary = loom_core::TaskSummary::empty(task);
        summary.input_tokens = 6_100;
        summary.cached_input_tokens = Some(5_600);
        reducer.set_summary(summary);
        reducer
    }

    #[test]
    fn context_screen_shows_breakdown_total_and_cache_split() {
        let backend = TestBackend::new(130, 30);
        let mut frame = ratatui::Terminal::new(backend).expect("test terminal");
        let reducer = reducer_with_progress();
        frame.draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &reducer, &[], 0));
        let buffer = frame.backend().buffer().clone();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for needle in [
            "/context",
            "BREAKDOWN",
            "system + tools",
            "tool results",
            "total (est.) 16.8K",
            "12.8% of 131.1K input budget",
            "cacheable prefix 5.6K",
            "92% of last request cached",
            "RULES",
            "Esc back",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn items_list_the_largest_results_and_mark_pin_and_drop() {
        use loom_core::kernel::transcript::{
            AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
        };

        let mut session = loom_core::Session::new(loom_core::SessionId::new());
        session.transcript.push(Item::Assistant(AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![
                ToolCall {
                    id: ToolCallId::try_from("call_1".to_owned()).expect("id"),
                    name: "read_file".to_owned(),
                    arguments: ToolArguments::try_from(serde_json::json!({"path": "src/a.rs"}))
                        .expect("args"),
                },
                ToolCall {
                    id: ToolCallId::try_from("call_2".to_owned()).expect("id"),
                    name: "search".to_owned(),
                    arguments: ToolArguments::try_from(
                        serde_json::json!({"command": "RetryPolicy"}),
                    )
                    .expect("args"),
                },
            ],
            stop_reason: loom_core::kernel::model::StopReason::ToolCalls,
        }));
        session.transcript.push(Item::ToolResult(ToolResult::new(
            ToolCallId::try_from("call_1".to_owned()).expect("id"),
            ToolStatus::Success,
            "x".repeat(3_000),
            false,
        )));
        session.transcript.push(Item::ToolResult(ToolResult::new(
            ToolCallId::try_from("call_2".to_owned()).expect("id"),
            ToolStatus::Success,
            "y".repeat(300),
            false,
        )));
        let mut ops = loom_core::kernel::context::PruneOps::default();
        ops.dropped.insert("call_2".to_owned());
        let items = items(&session, &ops, 6);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "read_file src/a.rs", "biggest first");
        assert_eq!(items[0].tokens, 1_000);
        assert!(!items[0].dropped);
        assert!(items[1].dropped);
    }
}
