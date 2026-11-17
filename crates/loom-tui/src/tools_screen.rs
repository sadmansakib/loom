//! The `/tools` screen: the built-in registry and the workspace's
//! declared MCP servers.
//!
//! The built-in table mirrors what `loom_engine`'s `build_registry`
//! registers; permissions read the live trust store, so `Space` on a
//! write-class tool toggles the same persisted choice the approval
//! prompt's `a` key makes. MCP servers are read from `.loom/mcp.toml`
//! — status is "declared" because servers attach when a session opens,
//! not on demand.

use std::collections::BTreeMap;
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
    /// Completed calls per tool in this session, for the wireframe's count
    /// column. Derived from the event log, never invented.
    calls: BTreeMap<String, usize>,
    /// Snapshot of the persisted writes choice, refreshed by the owner
    /// after a toggle.
    pub(crate) writes_allowed: bool,
    command: crate::command_line::CommandLine,
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
            calls: BTreeMap::new(),
            writes_allowed: loom_core::tools::trust::writes_allowed(workspace),
            command: crate::command_line::CommandLine::default(),
        }
    }

    /// Count completed tool calls from the recorded events. The wireframe's
    /// right-hand column is real session data, not sample numbers.
    pub(crate) fn with_calls(mut self, log: &[loom_core::EventEnvelope]) -> Self {
        for envelope in log {
            if let loom_core::EventKind::ToolCompleted { name, .. } = &envelope.kind {
                *self.calls.entry(name.clone()).or_default() += 1;
            }
        }
        self
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<ToolsAction> {
        match self.command.key(key) {
            crate::command_line::CommandKey::Ignore => {}
            crate::command_line::CommandKey::Consumed | crate::command_line::CommandKey::Cancel => {
                return Some(ToolsAction::Consumed);
            }
            crate::command_line::CommandKey::Run(command) => {
                return Some(match command.as_str() {
                    "permission" => ToolsAction::ToggleWrites,
                    "retry" => ToolsAction::Retry,
                    "close" => ToolsAction::Close,
                    _ => ToolsAction::Consumed,
                });
            }
        }
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

/// The wireframe's right-hand pane for the selected tool.
fn detail_lines(
    screen: &ToolsScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
) -> Vec<Line<'static>> {
    let Some((name, class)) = BUILT_IN.get(screen.selected) else {
        return vec![Line::from(Span::styled("MCP SERVER", amber))];
    };
    let (class_name, timeout, args) = match class {
        Class::Read => ("read", "10 s", "path: string"),
        Class::Write => ("write", "30 s", "path: string\ncontent: string"),
        Class::Shell => ("shell", "30 s", "command: string"),
    };
    let mut lines = vec![
        Line::from(Span::styled(*name, ivory.add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled("class      ", dim),
            Span::styled(class_name, ivory),
        ]),
        Line::from(vec![
            Span::styled("permission ", dim),
            Span::styled(permission(name, screen.writes_allowed), ivory),
        ]),
        Line::from(vec![
            Span::styled("timeout    ", dim),
            Span::styled(timeout, ivory),
        ]),
        Line::from(Span::styled("ARGS", amber)),
    ];
    for arg in args.lines() {
        lines.push(Line::from(Span::styled(format!("  {arg}"), dim)));
    }
    lines.push(Line::from(Span::styled("KEYS", amber)));
    lines.push(Line::from(Span::styled(
        "[/permission] cycle",
        Style::default().fg(theme::IVORY),
    )));
    lines.push(Line::from(Span::styled(
        "[/retry] server",
        Style::default().fg(theme::IVORY),
    )));
    lines
}

#[allow(clippy::too_many_lines)] // two-column table plus per-tool detail pane
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, screen: &ToolsScreen) {
    let height = area.height.clamp(14, 26);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .style(Style::default().bg(theme::BG))
            .title(Span::styled(
                " ◆ tools  /tools ",
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

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(52), Constraint::Length(34)])
        .split(rows[1]);
    let mut lines = vec![Line::from(vec![
        Span::styled("  tool             ", dim),
        Span::styled("class   ", dim),
        Span::styled("permission        ", dim),
        Span::styled("calls", dim),
    ])];
    for (index, (name, class)) in BUILT_IN.iter().enumerate() {
        let selected = index == screen.selected;
        let (glyph, color) = match class {
            Class::Read => ("●", theme::SAGE),
            Class::Write => ("◆", theme::AMBER),
            Class::Shell => ("◆", theme::VIOLET),
        };
        let label = match class {
            Class::Read => "read",
            Class::Write => "write",
            Class::Shell => "shell",
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "❯ " } else { "  " }, amber),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(
                format!("{name:<16}"),
                Style::default().fg(theme::BLUE).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(format!("{label:<8}"), dim),
            Span::styled(
                format!("{:<18}", permission(name, screen.writes_allowed)),
                dim,
            ),
            Span::styled(
                format!("{:>4}", screen.calls.get(*name).copied().unwrap_or(0)),
                Style::default().fg(theme::BLUE),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled("MCP SERVERS", amber)));
    if screen.mcp.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no servers declared in .loom/mcp.toml",
            dim,
        )));
    }
    for server in &screen.mcp {
        let (glyph, tone, status) = match server.connected {
            Some(true) => ("●", theme::SAGE, "connected"),
            Some(false) => ("✗", theme::CORAL, "failed to attach"),
            None => ("○", theme::DIM, "attaches when a session opens"),
        };
        let mut row = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(tone)),
            Span::styled(format!("{:<16}", server.name), ivory),
            Span::styled(format!("{status:<16}"), Style::default().fg(tone)),
        ];
        // Status is the only thing that says whether the server's tools
        // exist; the command is decorative, so drop it on narrow panes.
        if columns[0].width >= 64 {
            row.push(Span::styled(server.command.clone(), dim));
        }
        lines.push(Line::from(row));
    }
    frame.render_widget(Paragraph::new(lines), columns[0]);
    let detail = detail_lines(screen, amber, dim, ivory);
    frame.render_widget(
        Paragraph::new(detail).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(theme::BORDER)),
        ),
        columns[1],
    );
    frame.render_widget(
        Paragraph::new(screen.command.line().unwrap_or_else(|| {
            Line::from(Span::styled(
                if rows[2].width < 100 {
                    "↑↓ move · [/permission] · [/retry] · Esc back".to_owned()
                } else {
                    format!(
                        "↑↓ move · [/permission] permission · [/retry] retry server · Esc back   ·   {} built-in · {} MCP",
                        BUILT_IN.len(),
                        screen.mcp.len()
                    )
                },
                dim,
            ))
        })),
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
            calls: BTreeMap::from([("write_file".to_owned(), 3)]),
            writes_allowed: false,
            command: crate::command_line::CommandLine::default(),
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
            "[/permission]",
            "class",
            "timeout",
            "ARGS",
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
