//! The `/context` inspector: what the next request will contain.
//!
//! Reads the live reducer only — the census from the last
//! `ContextProgress` event plus the cache split from the task summary —
//! so every number shown is one the engine itself produced.

use loom_core::kernel::context::ContextCensus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::reducer::Reducer;
use crate::theme;

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
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, reducer: &Reducer) {
    let height = area.height.clamp(12, 24);
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
    let amber = Style::default().fg(theme::AMBER);
    let dim = Style::default().fg(theme::DIM);
    let ivory = Style::default().fg(theme::IVORY);
    let body = match reducer.context_progress.as_ref() {
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
    frame.render_widget(Paragraph::new(body), rows[0]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "RULES elision keeps the first 200 chars of old assistant text; aged tool args become [pruned N chars]",
                dim,
            )),
            Line::from(Span::styled(
                "c compact (runs automatically)   Esc back",
                dim,
            )),
        ]),
        rows[1],
    );
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
        frame.draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &reducer));
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
}
