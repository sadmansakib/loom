//! The `/tools` screen: the built-in registry and the workspace's
//! declared MCP servers.
//!
//! The built-in table mirrors what `loom_engine`'s `build_registry`
//! registers; permissions read the live trust store, so `Space` on a
//! write-class tool toggles the same persisted choice the approval
//! prompt's `a` key makes. MCP servers are read from `.loom/mcp.toml`
//! — status is "declared" because servers attach when a session opens,
//! not on demand.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme;

/// Class of a built-in tool, as the wireframe's table shows it.
enum Class {
    Read,
    Write,
    Shell,
}

/// The built-in registry `build_registry` installs. Keep in sync.
const BUILT_IN: [(&str, Class); 9] = [
    ("read_file", Class::Read),
    ("list_dir", Class::Read),
    ("search", Class::Read),
    ("artifact_read", Class::Read),
    ("todo", Class::Read),
    ("write_file", Class::Write),
    ("edit_file", Class::Write),
    ("run_command", Class::Shell),
    ("verify", Class::Shell),
];

/// One declared MCP server from `.loom/mcp.toml`, plus its live attach
/// outcome from the open engine (`None` when the screen has no engine
/// yet — the server has not been attempted).
pub(crate) struct McpRow {
    name: String,
    command: String,
    connected: Option<bool>,
}

pub(crate) struct ToolsScreen {
    mcp: Vec<McpRow>,
    selected: usize,
    /// Snapshot of the persisted writes choice, refreshed by the owner
    /// after a toggle.
    pub(crate) writes_allowed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ToolsAction {
    /// Key consumed internally (movement).
    Consumed,
    Close,
    /// Space: persist the always-allow writes choice for the selected
    /// write-class tool (one-way, like the approval prompt's `a`).
    ToggleWrites,
    /// `r`: reattach MCP servers.
    Retry,
}

impl ToolsScreen {
    /// Loads declared MCP servers; a missing or invalid manifest means
    /// none, which is the truth the engine itself attaches with. `live`
    /// is the open engine's per-server outcome (manifest order, same
    /// source), matched by name.
    pub(crate) fn load(workspace: &Path, live: &[(String, bool)]) -> Self {
        let mcp =
            std::fs::read_to_string(workspace.join(loom_core::tools::mcp::MANIFEST_RELATIVE_PATH))
                .ok()
                .and_then(|text| loom_core::tools::mcp::McpManifest::parse(&text).ok())
                .map_or_else(Vec::new, |manifest| {
                    manifest
                        .servers
                        .into_iter()
                        .map(|(name, decl)| McpRow {
                            connected: live
                                .iter()
                                .find(|(server, _)| *server == name)
                                .map(|(_, ok)| *ok),
                            command: decl.command.join(" "),
                            name,
                        })
                        .collect()
                });
        Self {
            mcp,
            selected: 0,
            writes_allowed: loom_core::tools::trust::writes_allowed(workspace),
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<ToolsAction> {
        if key.code == KeyCode::Esc {
            return Some(ToolsAction::Close);
        }
        if key.code == KeyCode::Up {
            self.selected = self.selected.saturating_sub(1);
            return Some(ToolsAction::Consumed);
        }
        if key.code == KeyCode::Down {
            let last = BUILT_IN.len().saturating_sub(1);
            self.selected = Ord::clamp(self.selected + 1, 0, last);
            return Some(ToolsAction::Consumed);
        }
        if key.modifiers.is_empty() {
            if key.code == KeyCode::Char(' ') {
                return Some(ToolsAction::ToggleWrites);
            }
            if key.code == KeyCode::Char('r') {
                return Some(ToolsAction::Retry);
            }
        }
        None
    }
}

/// The permission the selected tool runs with, given the persisted
/// writes choice. Read tools are always allowed; shell tools answer to
/// the command manifest's policy.
fn permission(name: &str, writes_allowed: bool) -> &'static str {
    match BUILT_IN.iter().find(|(tool, _)| *tool == name) {
        Some((_, Class::Read)) => "allowed",
        Some((_, Class::Write)) => {
            if writes_allowed {
                "allowed (saved)"
            } else {
                "ask each time"
            }
        }
        Some((_, Class::Shell)) => "manifest policy",
        None => "—",
    }
}

pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, screen: &ToolsScreen) {
    let height = area.height.clamp(14, 26);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .title(Span::styled(
                " /tools ",
                Style::default()
                    .fg(theme::AMBER)
                    .add_modifier(Modifier::BOLD),
            )),
        modal,
    );
    let inner = modal.inner(ratatui::layout::Margin::new(1, 1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(inner);
    let amber = Style::default().fg(theme::AMBER);
    let dim = Style::default().fg(theme::DIM);
    let ivory = Style::default().fg(theme::IVORY);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("BUILT-IN", amber))),
        rows[0],
    );

    let mut lines = Vec::new();
    for (index, (name, class)) in BUILT_IN.iter().enumerate() {
        let selected = index == screen.selected;
        let (glyph, color) = match class {
            Class::Read => ("●", theme::SAGE),
            Class::Write => ("◆", theme::AMBER),
            Class::Shell => ("◆", theme::VIOLET),
        };
        let label = match class {
            Class::Read => "read",
            Class::Write => "read-write",
            Class::Shell => "shell",
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "❯ " } else { "  " }, amber),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(
                format!("{name:<14}"),
                if selected {
                    ivory.add_modifier(Modifier::BOLD)
                } else {
                    ivory
                },
            ),
            Span::styled(format!("{label:<11}"), dim),
            Span::styled(permission(name, screen.writes_allowed), dim),
        ]));
    }
    if !screen.mcp.is_empty() {
        lines.push(Line::from(Span::styled("MCP SERVERS", amber)));
        for server in &screen.mcp {
            let (glyph, tone, status) = match server.connected {
                Some(true) => ("●", theme::SAGE, "connected"),
                Some(false) => ("○", theme::CORAL, "failed to attach"),
                None => ("·", theme::DIM, "attaches when a session opens"),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(tone)),
                Span::styled(format!("{:<14}", server.name), ivory),
                Span::styled(format!("{status:<26}"), Style::default().fg(tone)),
                Span::styled(server.command.clone(), dim),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), rows[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{} built-in · {} MCP   Space permission · r retry · Esc back",
                BUILT_IN.len(),
                screen.mcp.len()
            ),
            dim,
        ))),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn screen() -> ToolsScreen {
        ToolsScreen {
            mcp: vec![McpRow {
                name: "github".into(),
                command: "npx -y @modelcontextprotocol/server-github".into(),
                connected: Some(true),
            }],
            selected: 5,
            writes_allowed: false,
        }
    }

    #[test]
    fn keys_move_toggle_retry_and_close() {
        let mut tools = ToolsScreen::load(Path::new("/nonexistent"), &[]);
        assert!(tools.mcp.is_empty());
        assert_eq!(tools.key(KeyCode::Up.into()), Some(ToolsAction::Consumed)); // clamps at top
        tools.selected = 5;
        assert_eq!(tools.key(KeyCode::Up.into()), Some(ToolsAction::Consumed));
        assert_eq!(tools.key(KeyCode::Down.into()), Some(ToolsAction::Consumed));
        assert_eq!(
            tools.key(KeyEvent::from(KeyCode::Char(' '))),
            Some(ToolsAction::ToggleWrites)
        );
        assert_eq!(
            tools.key(KeyEvent::from(KeyCode::Char('r'))),
            Some(ToolsAction::Retry)
        );
        assert_eq!(tools.key(KeyCode::Esc.into()), Some(ToolsAction::Close));
    }

    #[test]
    fn draw_lists_builtin_classes_and_mcp_servers() {
        let backend = TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let tools = screen();
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &tools))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for needle in [
            "/tools",
            "BUILT-IN",
            "read_file",
            "write_file",
            "ask each time",
            "manifest policy",
            "MCP SERVERS",
            "github",
            "connected",
            "9 built-in · 1 MCP",
            "Space permission",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn failed_server_shows_the_failure_and_unattempted_the_open_hint() {
        let mut tools = screen();
        tools.mcp[0].connected = Some(false);
        let backend = TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &tools))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("failed to attach"));
        tools.mcp[0].connected = None;
        let backend = TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &tools))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("attaches when a session opens"));
    }

    #[test]
    fn narrow_terminal_keeps_the_attach_status_visible() {
        // At 80 columns the command can be clipped; the status may not,
        // because it is the only thing that says whether the server's
        // tools exist in this session.
        let mut tools = screen();
        tools.mcp[0].connected = Some(true);
        tools.mcp.push(crate::tools_screen::McpRow {
            name: "broken".to_owned(),
            command: "some-very-long-command-name-that-will-not-fit-anywhere".to_owned(),
            connected: Some(false),
        });
        let backend = TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 80, 30), &tools))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("connected"), "missing connected at 80 cols");
        assert!(
            text.contains("failed to attach"),
            "missing failed to attach at 80 cols"
        );
    }
}
