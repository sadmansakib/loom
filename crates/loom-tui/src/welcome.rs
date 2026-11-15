//! First-run flow: pick a provider, supply a key, and trust the folder.
//!
//! Pure state transitions live here so they can be tested without a
//! terminal. The caller performs the side effects (keychain write, trust pin).

use loom_core::engine::auth::{Auth, LoginSpec, Method, provider_env_state};
use std::path::Path;

/// One step of the first-run flow, in wireframe order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Provider,
    Key,
    Trust,
}

/// Mutable first-run state.
#[derive(Debug, Clone)]
pub struct Welcome {
    pub step: Step,
    pub provider: String,
    /// The env var that already holds the provider's key, if any.
    pub env_var: Option<String>,
    /// Key the user pastes; masked on screen, never echoed.
    pub key: String,
    pub error: Option<String>,
}

/// What the caller must do after a key press.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep showing the flow.
    Stay,
    /// The flow is complete: pin the workspace and start the session.
    Finish,
}

impl Welcome {
    /// Starts the flow for `provider`, noting any env key already set.
    #[must_use]
    pub fn new(provider: &str) -> Self {
        let env_var = provider_env_state(provider)
            .into_iter()
            .find(|state| state.set)
            .map(|state| state.name);
        Self {
            step: Step::Provider,
            provider: provider.to_owned(),
            env_var,
            key: String::new(),
            error: None,
        }
    }

    /// Whether a key is available to continue past step 2.
    #[must_use]
    pub fn has_key(&self) -> bool {
        self.env_var.is_some() || !self.key.trim().is_empty()
    }

    /// Advances one step on Enter. Saving a pasted key is the caller's job.
    pub fn advance(&mut self) -> Action {
        self.error = None;
        match self.step {
            Step::Provider => {
                self.step = Step::Key;
                Action::Stay
            }
            Step::Key => {
                if self.has_key() {
                    self.step = Step::Trust;
                } else {
                    self.error = Some("Paste a key or set the provider's env var.".into());
                }
                Action::Stay
            }
            Step::Trust => Action::Finish,
        }
    }
}

/// Saves a pasted key into the keychain as the provider's account.
///
/// # Errors
///
/// Returns the engine's auth error text when the keychain or store write fails.
pub fn save_pasted_key(db_path: &Path, provider: &str, key: &str) -> Result<(), String> {
    let auth = Auth::open(db_path).map_err(|error| error.to_string())?;
    auth.login(LoginSpec {
        provider: provider.to_owned(),
        label: None,
        method: Method::Key,
        command: None,
        secret: Some(key.trim().to_owned()),
    })
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Draws the first-run screen as a centered 620 px flow: brand,
/// description, then the three steps with the active one expanded.
#[allow(clippy::too_many_lines)] // three-step flow, one row block per step
pub fn draw_welcome(frame: &mut ratatui::Frame<'_>, flow: &Welcome, workspace: &str) {
    use crate::theme;
    use ratatui::layout::{Constraint, Direction, Layout, Rect};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Wrap};

    let fg = |color| Style::default().fg(color);
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◆ loom", fg(theme::AMBER).add_modifier(Modifier::BOLD)),
            Span::styled("  first run", fg(theme::DIM)),
        ])),
        rows[0],
    );

    // 620 px at the wireframe's 7.8 px per column, capped to the terminal.
    let column_width = area.width.min(79);
    let top_margin = if rows[1].height >= 26 { 3 } else { 0 };
    let column = Rect {
        x: area.x + area.width.saturating_sub(column_width) / 2,
        y: rows[1].y.saturating_add(top_margin),
        width: column_width,
        height: rows[1].height.saturating_sub(top_margin),
    };
    let mut row = column.y;
    macro_rules! put {
        ($lines:expr, $height:expr) => {{
            let top = row;
            let height = u16::min($height, column.height.saturating_sub(top - column.y));
            if height > 0 {
                frame.render_widget(
                    Paragraph::new($lines)
                        .style(fg(theme::IVORY))
                        .wrap(Wrap { trim: false }),
                    Rect {
                        x: column.x,
                        y: top,
                        width: column.width,
                        height,
                    },
                );
                row = top.saturating_add($height);
            }
        }};
    }
    macro_rules! put_box {
        ($lines:expr, $height:expr, $color:expr) => {{
            let top = row;
            let height = u16::min($height, column.height.saturating_sub(top - column.y));
            if height > 0 {
                frame.render_widget(
                    Paragraph::new($lines)
                        .block(section_box($color))
                        .style(fg(theme::IVORY))
                        .wrap(Wrap { trim: false }),
                    Rect {
                        x: column.x,
                        y: top,
                        width: column.width,
                        height,
                    },
                );
                row = top.saturating_add($height);
            }
        }};
    }

    put!(
        vec![
            Line::from(Span::styled(
                "◆ loom",
                fg(theme::AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "A coding agent for your terminal. One thread per task.",
                fg(theme::DIM),
            )),
            Line::from(""),
            step_line(1, "PROVIDER", flow.step == Step::Provider),
            Line::from(vec![
                Span::styled("  ● ", fg(theme::AMBER)),
                Span::styled(flow.provider.clone(), fg(theme::IVORY)),
            ]),
            Line::from(""),
            step_line(2, "KEY", flow.step == Step::Key),
        ],
        7
    );
    let key_lines = match &flow.env_var {
        Some(name) => vec![
            Line::from(vec![
                Span::styled(" ❯  ✓ ", fg(theme::SAGE)),
                Span::styled(format!("{name} found in env"), fg(theme::IVORY)),
            ]),
            Line::from(Span::styled(
                " Or paste a key. Stored in the OS keychain, never in the transcript.",
                fg(theme::DIM),
            )),
        ],
        None => vec![
            Line::from(vec![
                Span::styled(" ❯  ", fg(theme::AMBER)),
                Span::styled("•".repeat(flow.key.chars().count()), fg(theme::IVORY)),
            ]),
            Line::from(Span::styled(
                "  Pasted keys go to the OS keychain, never the transcript.",
                fg(theme::DIM),
            )),
        ],
    };
    put_box!(key_lines, 4, theme::DOTTED);
    put!(vec![Line::from("")], 1);
    put!(
        vec![step_line(3, "TRUST THIS FOLDER", flow.step == Step::Trust,)],
        1
    );
    let trust_color = if flow.step == Step::Trust {
        theme::AMBER
    } else {
        theme::DOTTED
    };
    put_box!(
        vec![
            Line::from(Span::styled(format!(" {workspace}"), fg(theme::IVORY),)),
            Line::from(Span::styled(
                " Reads allowed. Writes and shell ask each time. Later in /tools.",
                fg(theme::DIM),
            )),
        ],
        4,
        trust_color
    );
    if let Some(error) = &flow.error {
        put!(
            vec![Line::from(Span::styled(error.clone(), fg(theme::CORAL),))],
            1
        );
    }
    put!(vec![Line::from("")], 1);
    put!(
        key_hints(&[
            (
                "Enter",
                if flow.step == Step::Trust {
                    "start"
                } else {
                    "continue"
                }
            ),
            ("Esc", "quit")
        ]),
        1
    );
    let _ = row;
}

fn section_box(color: ratatui::style::Color) -> ratatui::widgets::Block<'static> {
    use ratatui::style::Style;
    use ratatui::widgets::{Block, Borders};
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
}

fn key_hints(pairs: &[(&str, &str)]) -> ratatui::text::Line<'static> {
    use crate::theme;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    let mut spans = Vec::new();
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(theme::IVORY)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme::DIM),
        ));
    }
    Line::from(spans)
}

fn step_line(number: u8, title: &str, active: bool) -> ratatui::text::Line<'static> {
    use crate::theme;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    let style = if active {
        Style::default()
            .fg(theme::AMBER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::DIM)
    };
    Line::from(Span::styled(format!("{number} · {title}"), style))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_walks_provider_key_trust_then_finishes() {
        let mut flow = Welcome {
            step: Step::Provider,
            provider: "deepseek".into(),
            env_var: Some("DEEPSEEK_API_KEY".into()),
            key: String::new(),
            error: None,
        };
        assert_eq!(flow.advance(), Action::Stay);
        assert_eq!(flow.step, Step::Key);
        assert_eq!(flow.advance(), Action::Stay);
        assert_eq!(flow.step, Step::Trust);
        assert_eq!(flow.advance(), Action::Finish);
    }

    #[test]
    fn key_step_blocks_without_env_or_paste() {
        let mut flow = Welcome {
            step: Step::Key,
            provider: "deepseek".into(),
            env_var: None,
            key: "   ".into(),
            error: None,
        };
        assert_eq!(flow.advance(), Action::Stay);
        assert_eq!(flow.step, Step::Key);
        assert!(flow.error.is_some());
    }

    #[test]
    fn pasted_key_satisfies_the_key_step() {
        let mut flow = Welcome {
            step: Step::Key,
            provider: "deepseek".into(),
            env_var: None,
            key: "sk-test".into(),
            error: None,
        };
        assert_eq!(flow.advance(), Action::Stay);
        assert_eq!(flow.step, Step::Trust);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn trust_step_renders_the_folder_and_its_rules() {
        let mut flow = Welcome::new("deepseek");
        flow.step = Step::Trust;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| draw_welcome(frame, &flow, "~/work/loom"))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            text.contains("TRUST THIS FOLDER"),
            "trust step title missing"
        );
        assert!(
            text.contains("Writes and shell ask each time"),
            "trust rule missing"
        );
        assert!(text.contains("~/work/loom"), "trusted folder missing");
    }
}
