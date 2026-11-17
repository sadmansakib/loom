//! Static renderings of the wireframe screens that have no live counterpart:
//! the design spec, the narrow layout, and the two compact-notice states.
//!
//! These are previews, not simulations: they never claim a task is running.
//! The only sample values are the ones the wireframe itself prints (the spec's
//! token table and the compact notice's 16.0K → 4.2K example).

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::theme;

fn fg(color: Color) -> Style {
    Style::default().fg(color).bg(theme::BG)
}

fn key(k: &str, label: &str) -> Span<'static> {
    Span::styled(
        format!("[{k}] {label}  "),
        fg(theme::IVORY).add_modifier(Modifier::BOLD),
    )
}

fn header(frame: &mut Frame<'_>, area: Rect, right: &str) {
    let content = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("  ·  design spec", fg(theme::DIM)),
        ])),
        cols[0],
    );
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), cols[1]);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(fg(theme::BORDER)),
        Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..area
        },
    );
}

fn footer(frame: &mut Frame<'_>, area: Rect, left: Vec<Span<'static>>, right: &str) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(right)
            .alignment(Alignment::Right)
            .style(fg(theme::DIM)),
        cols[1],
    );
}

/// Screen 10 · design spec: palette, glyphs, grid and recipe.
pub(crate) fn draw_spec(frame: &mut Frame<'_>) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    header(frame, rows[0], "tokens match docs/wireframes");

    let glyphs: [(&str, &str); 13] = [
        ("●", "turn node"),
        ("✓", "succeeded"),
        ("✗", "failed"),
        ("⚠", "interrupted"),
        ("○", "queued"),
        ("⊘", "cancelled"),
        ("↻", "retrying"),
        ("⠇", "working"),
        ("▸ ▾", "reasoning fold"),
        ("├─ └─", "tool batch"),
        ("❯", "prompt"),
        ("◆", "brand"),
        ("◇", "needs you (new)"),
    ];
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(28),
            Constraint::Percentage(42),
        ])
        .split(rows[1]);

    let mut swatches = vec![heading("PALETTE")];
    for (name, hex, color) in theme::PALETTE {
        swatches.push(Line::from(vec![
            Span::styled("■ ", fg(color)),
            Span::styled(format!("{name:<8}"), fg(theme::IVORY)),
            Span::styled(hex.to_owned(), fg(theme::DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(swatches), cols[0]);

    let mut glyph_lines = vec![heading("GLYPHS")];
    for (glyph, meaning) in glyphs {
        glyph_lines.push(Line::from(vec![
            Span::styled(format!("{glyph:<6}"), fg(theme::AMBER)),
            Span::styled(meaning.to_owned(), fg(theme::DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(glyph_lines), cols[1]);

    let grid = vec![
        heading("GRID"),
        muted("Monospace 13/20, 1 col ≈ 7.8 px."),
        muted("Wide 120+: thread + 38-col sidebar."),
        muted("Medium 90–119: Tab opens sidebar."),
        muted("Narrow <90: one status line."),
        heading("RECIPE"),
        muted("Header 36 px · thread · optional sidebar · composer (amber border) · key hints."),
        muted("Modals: 980 px, amber border, 82% dim."),
        muted("Amber = live or needs you."),
        muted("Actions are slash commands; live keys: Esc ^C ^O ↑↓."),
        muted("State = glyph + word + color."),
        heading("REGENERATE"),
        muted("python3 loom_wireframes.py"),
        muted("then publish project/*.dc.html"),
    ];
    frame.render_widget(Paragraph::new(grid).wrap(Wrap { trim: false }), cols[2]);
    footer(frame, rows[2], vec![key("Esc", "close")], "design spec");
}

/// Screen 9 · narrow layout (under 90 columns): one status line, no sidebar.
pub(crate) fn draw_narrow(frame: &mut Frame<'_>) {
    // The wireframe is 640 px wide; reproduce it as a centered 80-col column.
    let full = frame.area();
    let width = full.width.min(80);
    let area = Rect {
        x: full.x + full.width.saturating_sub(width) / 2,
        width,
        ..full
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let content = Rect {
        height: rows[0].height.saturating_sub(1),
        ..rows[0]
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "◆ loom",
            fg(theme::AMBER).add_modifier(Modifier::BOLD),
        )])),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new("⠇ turn 2 · $0.0012")
            .alignment(Alignment::Right)
            .style(fg(theme::AMBER)),
        cols[1],
    );
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(rows[0].width))).style(fg(theme::BORDER)),
        Rect {
            y: rows[0].y + rows[0].height.saturating_sub(1),
            height: 1,
            ..rows[0]
        },
    );

    let thread = vec![
        Line::from(Span::styled(
            "❯ Explain the retry loop.",
            fg(theme::IVORY).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("● ", fg(theme::AMBER)),
            Span::styled("turn 1 ", fg(theme::IVORY)),
            Span::styled("✓ 3 calls · 1.9 s", fg(theme::SAGE)),
        ]),
        Line::from(vec![
            Span::styled("● ", fg(theme::AMBER)),
            Span::styled("turn 2 ", fg(theme::IVORY)),
            Span::styled("⠇ streaming", fg(theme::AMBER)),
        ]),
        Line::from(vec![
            Span::styled("  ", fg(theme::DIM)),
            Span::styled(
                "▸ reasoning · 212 tok",
                fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ├─ ", fg(theme::THREAD)),
            Span::styled("✓ ", fg(theme::SAGE)),
            Span::styled("read_file ", fg(theme::BLUE)),
            Span::styled("retry.rs", fg(theme::IVORY)),
        ]),
        Line::from(vec![
            Span::styled("  └─ ", fg(theme::THREAD)),
            Span::styled("⠇ ", fg(theme::AMBER)),
            Span::styled("search ", fg(theme::BLUE)),
            Span::styled("“emit(”", fg(theme::IVORY)),
        ]),
    ];
    frame.render_widget(Paragraph::new(thread), rows[1]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("Task running…", fg(theme::DIM)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(fg(theme::AMBER)),
        ),
        rows[2],
    );
    footer(
        frame,
        rows[3],
        vec![key("Esc", "cancel"), key("/help", "help")],
        "",
    );
}

/// Screen 16 · /compact from /context: the notice is hidden under the modal.
pub(crate) fn draw_compact_a(frame: &mut Frame<'_>) {
    let area = frame.area();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled("❯ Inspect this repository…", fg(theme::IVORY))),
            Line::from(vec![
                Span::styled("● ", fg(theme::AMBER)),
                Span::styled("turn 1 ", fg(theme::DIM)),
                Span::styled("✓ 3 calls", fg(theme::SAGE)),
            ]),
        ]),
        area,
    );
    // Inset the modal so the dimmed live view stays visible at the edges,
    // like the wireframe's 1120 px box over the 1200 px canvas.
    let modal = Rect {
        x: area.x.saturating_add(2),
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(fg(theme::AMBER))
            .style(Style::default().bg(theme::BG)),
        modal,
    );
    let inner = modal.inner(ratatui::layout::Margin::new(2, 1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(4),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ context ", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("/context", fg(theme::DIM)),
            Span::raw("    "),
            Span::styled("after c · compacted", fg(theme::DIM)),
        ])),
        rows[0],
    );
    let lines = vec![
        heading("NEXT REQUEST"),
        Line::from(vec![
            Span::styled("█".repeat(45), fg(theme::DIM)),
            Span::styled("█".repeat(8), fg(theme::VIOLET)),
            Span::styled("█".repeat(2), fg(theme::IVORY)),
        ]),
        Line::from(Span::styled(
            "│ total (est.) 4.2K · 0.4% of 1M   was 16.0K │",
            fg(theme::AMBER),
        )),
        heading("LARGEST ITEMS"),
        item_line("turns 1–12", "summary (new)", "3.1K", "—", theme::VIOLET),
        item_line(
            "turn 13",
            "read_file retry.rs",
            "1.1K",
            "cached",
            theme::TEAL,
        ),
        Line::from(""),
        Line::from(vec![
            key("↑↓", "move"),
            Span::raw("  "),
            key("/compact", "compact again"),
            Span::raw("  "),
            key("Esc", "back"),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);
    // Hatched slot: the status bar sits under this modal, waiting to be read.
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "▒ status bar sits under this modal",
                fg(theme::DOTTED),
            )),
            Line::from(Span::styled(
                "▒ the compact notice is posted there and waits",
                fg(theme::DOTTED),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(fg(theme::DOTTED)),
        ),
        rows[2],
    );
}

/// Screen 17 · after Esc: the compact notice is revealed in the status bar.
#[allow(clippy::too_many_lines)] // header, thread, sidebar and amber-ruled notice
pub(crate) fn draw_compact_b(frame: &mut Frame<'_>) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
    let content = Rect {
        height: rows[0].height.saturating_sub(1),
        ..rows[0]
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(content);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("   ", fg(theme::DIM)),
            Span::styled("✓ idle", fg(theme::SAGE).add_modifier(Modifier::BOLD)),
        ])),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new("events 2,044 · dropped 0")
            .alignment(Alignment::Right)
            .style(fg(theme::DIM)),
        cols[1],
    );
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(rows[0].width))).style(fg(theme::BORDER)),
        Rect {
            y: rows[0].y + rows[0].height.saturating_sub(1),
            height: 1,
            ..rows[0]
        },
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(rows[1]);
    let thread = vec![
        Line::from(Span::styled(
            "❯ Inspect this repository, find where model retry is implemented.",
            fg(theme::IVORY).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("● ", fg(theme::AMBER)),
            Span::styled("turns 1–12 ", fg(theme::DIM)),
            Span::styled(
                "summarised",
                fg(theme::VIOLET).add_modifier(Modifier::ITALIC),
            ),
        ]),
        Line::from(Span::styled(
            "── compacted · 12 turns → 1 summary · 16.0K → 4.2K ──",
            fg(theme::THREAD),
        )),
        Line::from(vec![
            Span::styled("● ", fg(theme::AMBER)),
            Span::styled("turn 13 ", fg(theme::IVORY)),
            Span::styled("✓ 1 call", fg(theme::SAGE)),
        ]),
    ];
    frame.render_widget(Paragraph::new(thread), body[0]);
    let sidebar = vec![
        heading("TASK"),
        Line::from(Span::styled(
            "✓ Idle",
            fg(theme::SAGE).add_modifier(Modifier::BOLD),
        )),
        heading("CONTEXT"),
        Line::from(vec![
            Span::styled("next request ", fg(theme::DIM)),
            Span::styled("4.2K", fg(theme::IVORY)),
        ]),
        heading("NOTICE RULES"),
        muted("One slot, latest wins."),
        muted("Timer starts when the status bar"),
        muted("is uncovered, not when posted."),
        muted("Stays until next keypress or 8 s."),
        muted("The modal never reads it."),
    ];
    frame.render_widget(
        Paragraph::new(sidebar).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(fg(theme::BORDER)),
        ),
        body[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("Next task in this session…", fg(theme::DIM)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(fg(theme::AMBER)),
        ),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("✓ ", fg(theme::SAGE)),
            Span::styled("compacted ", fg(theme::IVORY).add_modifier(Modifier::BOLD)),
            Span::styled("12 turns into 1 summary · 16.0K → 4.2K", fg(theme::DIM)),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(fg(theme::AMBER)),
        ),
        rows[3],
    );
}

fn item_line(
    turn: &str,
    label: &str,
    tokens: &str,
    status: &str,
    status_color: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{turn:<12}"), fg(theme::DIM)),
        Span::styled(format!("{label:<22}"), fg(theme::BLUE)),
        Span::styled(format!("{tokens:<6}"), fg(theme::IVORY)),
        Span::styled(status.to_owned(), fg(status_color)),
    ])
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), fg(theme::DIM)))
}

fn muted(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), fg(theme::DIM)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(f: impl FnOnce(&mut Frame<'_>)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal.draw(|frame| f(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn spec_lists_every_palette_token_and_glyph() {
        let text = render(draw_spec);
        for needle in [
            "PALETTE",
            "ivory",
            "#E9E3D7",
            "ground",
            "#12100E",
            "GLYPHS",
            "turn node",
            "needs you (new)",
            "GRID",
            "Wide 120+",
            "RECIPE",
            "REGENERATE",
            "loom_wireframes.py",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn narrow_keeps_one_status_line_and_no_sidebar() {
        let text = render(draw_narrow);
        assert!(text.contains("Explain the retry loop."));
        assert!(text.contains("streaming"));
        assert!(text.contains("reasoning · 212 tok"));
        assert!(text.contains("Task running…"));
        assert!(text.contains("[Esc]"));
        assert!(!text.contains("LIVE USAGE"));
    }

    #[test]
    fn compact_notice_states_are_distinct() {
        let covered = render(draw_compact_a);
        let revealed = render(draw_compact_b);
        assert!(covered.contains("status bar sits under this modal"));
        assert!(covered.contains("summary (new)"));
        assert!(revealed.contains("NOTICE RULES"));
        assert!(revealed.contains("compacted 12 turns into 1 summary"));
        assert!(!revealed.contains("status bar sits under this modal"));
    }
}
