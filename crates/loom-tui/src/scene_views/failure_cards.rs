//! Terminal-failure cards. Rendering reads reducer state only.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{glyphs, reducer::FailureState, theme};

use super::{fg, glyph_set};

/// Product view: only the failure that actually happened.
pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, failure: &FailureState) {
    draw_cards(frame, area, std::slice::from_ref(failure));
}

/// Two columns at wide widths, one column below 120.
pub(super) fn draw_cards(frame: &mut Frame<'_>, area: Rect, cards: &[FailureState]) {
    draw_cards_with_set(frame, area, cards, glyph_set());
}

fn draw_cards_with_set(
    frame: &mut Frame<'_>,
    area: Rect,
    cards: &[FailureState],
    set: crate::glyphs::GlyphSet,
) {
    if cards.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }
    let columns = if area.width >= 120 { 2 } else { 1 };
    let rows = cards.len().div_ceil(columns);
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Ratio(1, u32::try_from(rows).unwrap_or(1));
            rows
        ])
        .split(area);
    for (row, row_area) in row_areas.iter().copied().enumerate() {
        let cards_in_row = cards
            .iter()
            .skip(row * columns)
            .take(columns)
            .collect::<Vec<_>>();
        let card_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![
                Constraint::Ratio(
                    1,
                    u32::try_from(columns).unwrap_or(1)
                );
                columns
            ])
            .split(row_area);
        for (card, card_area) in cards_in_row.into_iter().zip(card_areas.iter().copied()) {
            draw_card(frame, card_area, card, set);
        }
    }
}

fn draw_card(frame: &mut Frame<'_>, area: Rect, card: &FailureState, set: crate::glyphs::GlyphSet) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let mut title = vec![
        Span::styled(
            format!("{} ", glyphs::glyph(glyphs::Glyph::Cross, set)),
            fg(theme::CORAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            card.title.clone(),
            fg(theme::CORAL).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(subtitle) = &card.subtitle {
        title.push(Span::styled(
            format!(" {subtitle}"),
            fg(theme::CORAL).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(status) = card.http_status {
        title.push(Span::styled(format!(" HTTP {status}"), fg(theme::DIM)));
    }
    let mut lines = vec![Line::from(title), Line::from("")];
    for line in wrap_text(&card.body, usize::from(area.width.saturating_sub(4))) {
        lines.push(Line::from(Span::styled(line, fg(theme::IVORY))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(action_spans(
        &card.action,
        card.command.as_deref(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .style(ratatui::style::Style::default().bg(theme::BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(fg(theme::CORAL)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn action_spans<'a>(action: &'a str, command: Option<&'a str>) -> Vec<Span<'a>> {
    let Some(command) = command.filter(|command| action.contains(command)) else {
        return vec![Span::styled(action, fg(theme::IVORY))];
    };
    let Some(index) = action.find(command) else {
        return vec![Span::styled(action, fg(theme::IVORY))];
    };
    let mut spans = Vec::new();
    let prefix = &action[..index];
    let suffix = &action[index + command.len()..];
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, fg(theme::IVORY)));
    }
    spans.push(Span::styled(
        command,
        fg(theme::BLUE).add_modifier(Modifier::BOLD),
    ));
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, fg(theme::IVORY)));
    }
    spans
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
pub(super) fn reference_cards() -> Vec<FailureState> {
    use crate::reducer::FailureKind;

    vec![
        FailureState {
            kind: FailureKind::MaxTokens,
            title: "MaxTokens".to_owned(),
            subtitle: None,
            http_status: None,
            body: "Reply hit the output limit. Partial text kept; tool calls dropped, none run."
                .to_owned(),
            action: "Raise --max-tokens or narrow the task.".to_owned(),
            command: Some("--max-tokens".to_owned()),
            retry_kind: None,
            attempts: 1,
            intact_turn: 0,
            elapsed_ms: None,
        },
        FailureState {
            kind: FailureKind::QuotaExhausted,
            title: "QuotaExhausted".to_owned(),
            subtitle: None,
            http_status: Some(402),
            body: "DeepSeek reports insufficient balance. Not retried.".to_owned(),
            action: "Top up, or loom auth switch deepseek <label>".to_owned(),
            command: Some("loom auth switch deepseek <label>".to_owned()),
            retry_kind: None,
            attempts: 1,
            intact_turn: 0,
            elapsed_ms: None,
        },
        FailureState {
            kind: FailureKind::ContentFilter,
            title: "ContentFilter".to_owned(),
            subtitle: None,
            http_status: None,
            body: "Reply was filtered. Stored, but hidden from later turns.".to_owned(),
            action: "Rephrase the task and run it again.".to_owned(),
            command: None,
            retry_kind: None,
            attempts: 1,
            intact_turn: 0,
            elapsed_ms: None,
        },
        FailureState {
            kind: FailureKind::RetriesExhausted,
            title: "Retries exhausted".to_owned(),
            subtitle: Some("overloaded ×4".to_owned()),
            http_status: None,
            body: "Gave up after 46 s. Session intact at turn 1.".to_owned(),
            action: "/retry re-run the task".to_owned(),
            command: Some("/retry".to_owned()),
            retry_kind: Some("overloaded".to_owned()),
            attempts: 4,
            intact_turn: 1,
            elapsed_ms: Some(46_000),
        },
    ]
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn rendered(area: ratatui::layout::Rect, set: crate::glyphs::GlyphSet) -> String {
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                draw_cards_with_set(frame, frame.area(), &reference_cards(), set);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(area.width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn all_four_cards_render_exact_messages() {
        for set in [
            crate::glyphs::GlyphSet::Unicode,
            crate::glyphs::GlyphSet::Ascii,
        ] {
            let text = rendered(ratatui::layout::Rect::new(0, 0, 140, 24), set);
            for expected in [
                "MaxTokens",
                "Reply hit the output limit. Partial text kept;",
                "Raise --max-tokens or narrow the task.",
                "QuotaExhausted HTTP 402",
                "DeepSeek reports insufficient balance. Not retried.",
                "loom auth switch deepseek <label>",
                "ContentFilter",
                "Reply was filtered. Stored, but hidden from later turns.",
                "Retries exhausted overloaded ×4",
                "Gave up after 46 s. Session intact at turn 1.",
                "/retry re-run the task",
            ] {
                assert!(text.contains(expected), "missing {expected:?}\n{text}");
            }
        }
    }

    #[test]
    fn narrow_cards_stack_in_one_column_without_clipping_actions() {
        let text = rendered(
            ratatui::layout::Rect::new(0, 0, 70, 32),
            crate::glyphs::GlyphSet::Ascii,
        );
        for expected in [
            "MaxTokens",
            "Raise --max-tokens or narrow the task.",
            "QuotaExhausted HTTP 402",
            "loom auth switch deepseek <label>",
            "ContentFilter",
            "Rephrase the task and run it again.",
            "Retries exhausted overloaded ×4",
            "/retry re-run the task",
        ] {
            assert!(text.contains(expected), "missing {expected:?}\n{text}");
        }
        let find_y = |needle: &str| text.lines().position(|line| line.contains(needle));
        let quota = find_y("QuotaExhausted").expect("quota card");
        let content = find_y("ContentFilter").expect("content card");
        assert!(quota > 0 && content > quota);
    }
}
