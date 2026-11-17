//! The `/sessions` screen: filter past sessions, preview one, then
//! resume, export, or delete it. Reads the same persisted store
//! `loom sessions` reads; a missing store shows an empty list.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use loom_core::EventKind;
use loom_core::kernel::session::TaskStatus;
use loom_core::kernel::store::{EventStore, SessionStore};
use loom_core::kernel::transcript::Item;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::glyphs::{Glyph, glyph};
use crate::scene_views::glyph_set;
use crate::theme;

/// How many sessions the list shows.
pub(crate) const SESSIONS_LIMIT: usize = 20;

/// One RECENT row in the wireframe: id, outcome, prompt, turns, wall.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) id: String,
    pub(crate) status: Option<TaskStatus>,
    pub(crate) prompt: String,
    pub(crate) turns: u32,
    pub(crate) wall_millis: Option<u64>,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) started_millis: u64,
}

/// The PREVIEW pane for the selected row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Preview {
    pub(crate) model: String,
    pub(crate) events: usize,
    pub(crate) tool_calls: usize,
    pub(crate) cached_pct: Option<u32>,
    pub(crate) last_answer: String,
    pub(crate) files: Vec<String>,
}

/// What one keypress asked the main loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionsAction {
    /// Key handled inside the screen (movement, filter edits).
    Consumed,
    Close,
    Resume(String),
    Export(String),
    Delete(String),
    NewSession,
}

#[derive(Debug, Default)]
pub(crate) struct SessionsScreen {
    pub(crate) rows: Vec<Row>,
    pub(crate) filter: String,
    /// Index into the filtered view, clamped by `key`.
    pub(crate) selected: usize,
    pub(crate) preview: Option<Preview>,
    /// Workspace label for the modal header (`~/work/loom`).
    pub(crate) workspace: String,
    command: crate::command_line::CommandLine,
}

impl SessionsScreen {
    /// Loads up to [`SESSIONS_LIMIT`] sessions, newest first. A missing
    /// or unreadable store yields an empty screen, never an error.
    pub(crate) fn load(db_path: &Path) -> Self {
        let mut screen = Self::default();
        if !db_path.exists() {
            return screen;
        }
        let Ok(store) = loom_core::store::SqliteStore::open(db_path) else {
            return screen;
        };
        let Ok(mut ids) = store.session_ids() else {
            return screen;
        };
        ids.reverse();
        screen.rows = ids
            .into_iter()
            .filter_map(|id| {
                let session = store.load(&id).ok().flatten()?;
                let prompt = session.transcript.iter().find_map(|item| {
                    if let Item::User { text } = item {
                        Some(crate::recent::first_line(text, 60))
                    } else {
                        None
                    }
                })?;
                let last = session.tasks.last();
                Some(Row {
                    id: id.to_string(),
                    status: last.map(|task| task.status),
                    prompt,
                    turns: last.map_or(0, |task| task.turns),
                    wall_millis: last.and_then(|task| {
                        task.completed_at_millis
                            .map(|done| done.saturating_sub(task.started_at_millis))
                    }),
                    cost_usd: last.and_then(|task| task.usage.cost_usd),
                    started_millis: last.map_or(0, |task| task.started_at_millis),
                })
            })
            .take(SESSIONS_LIMIT)
            .collect();
        screen
    }

    /// Row indices passing the current filter (id, prompt, or status).
    pub(crate) fn filtered(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.id.to_lowercase().contains(&needle)
                    || row.prompt.to_lowercase().contains(&needle)
                    || status_label(row.status).to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Reloads the PREVIEW pane for the row under the cursor.
    pub(crate) fn refresh_preview(&mut self, db_path: &Path) {
        let Some(row) = self
            .filtered()
            .get(self.selected)
            .map(|index| &self.rows[*index])
        else {
            self.preview = None;
            return;
        };
        let Ok(id) = row.id.parse::<loom_core::SessionId>() else {
            self.preview = None;
            return;
        };
        if !db_path.exists() {
            self.preview = None;
            return;
        }
        let Ok(store) = loom_core::store::SqliteStore::open(db_path) else {
            self.preview = None;
            return;
        };
        let events = store.load_events(&id).unwrap_or_default();
        let model = events.iter().find_map(|envelope| {
            if let EventKind::CapabilitiesResolved { model, .. } = &envelope.kind {
                Some(model.clone())
            } else {
                None
            }
        });
        let session = store.load(&id).ok().flatten();
        let cached_pct = session
            .as_ref()
            .and_then(|session| session.tasks.last().map(|task| &task.usage))
            .and_then(|usage| {
                usage
                    .cached_input_tokens
                    .filter(|cached| *cached > 0)
                    .zip(Some(usage.input_tokens))
                    .filter(|(_, input)| *input > 0)
                    .map(|(cached, input)| u32::try_from(cached * 100 / input).unwrap_or(u32::MAX))
            });
        let (tool_calls, last_answer, files) = session.map_or_else(
            || (0, String::new(), Vec::new()),
            |session| {
                let mut tool_calls = 0;
                let mut last_answer = String::new();
                let mut files = Vec::new();
                for item in &session.transcript {
                    match item {
                        Item::ToolResult(result) => {
                            tool_calls += 1;
                            for file in &result.effects.files_written {
                                if !files.contains(file) {
                                    files.push(file.clone());
                                }
                            }
                        }
                        Item::Assistant(message) if !message.content.trim().is_empty() => {
                            last_answer = crate::recent::first_line(message.content.trim(), 72);
                        }
                        Item::User { .. } | _ => {}
                    }
                }
                (tool_calls, last_answer, files)
            },
        );
        self.preview = Some(Preview {
            model: model.unwrap_or_else(|| "unknown".into()),
            events: events.len(),
            tool_calls,
            cached_pct,
            last_answer,
            files,
        });
    }

    /// Routes one keypress; `selected` is clamped to the filtered count.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<SessionsAction> {
        match self.command.key(key) {
            crate::command_line::CommandKey::Ignore => {}
            crate::command_line::CommandKey::Consumed | crate::command_line::CommandKey::Cancel => {
                return Some(SessionsAction::Consumed);
            }
            crate::command_line::CommandKey::Run(command) => {
                let selected_id = self
                    .filtered()
                    .get(self.selected)
                    .map(|index| self.rows[*index].id.clone());
                return Some(
                    match command.split_whitespace().collect::<Vec<_>>().as_slice() {
                        ["export"] => {
                            selected_id.map_or(SessionsAction::Consumed, SessionsAction::Export)
                        }
                        ["delete"] => {
                            selected_id.map_or(SessionsAction::Consumed, SessionsAction::Delete)
                        }
                        ["resume" | "open"] => {
                            selected_id.map_or(SessionsAction::Consumed, SessionsAction::Resume)
                        }
                        ["new"] => SessionsAction::NewSession,
                        ["close"] => SessionsAction::Close,
                        _ => SessionsAction::Consumed,
                    },
                );
            }
        }
        let count = self.filtered().len();
        if key.code == KeyCode::Esc {
            return Some(SessionsAction::Close);
        }
        if key.code == KeyCode::Enter {
            let id = self
                .filtered()
                .get(self.selected)
                .map(|index| self.rows[*index].id.clone());
            return id.map(SessionsAction::Resume);
        }
        if key.code == KeyCode::Char('e') && key.modifiers.is_empty() {
            let id = self
                .filtered()
                .get(self.selected)
                .map(|index| self.rows[*index].id.clone());
            return id.map(SessionsAction::Export);
        }
        if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
            let id = self
                .filtered()
                .get(self.selected)
                .map(|index| self.rows[*index].id.clone());
            return id.map(SessionsAction::Delete);
        }
        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(SessionsAction::NewSession);
        }
        if key.code == KeyCode::Up {
            self.selected = self.selected.saturating_sub(1);
            return Some(SessionsAction::Consumed);
        }
        if key.code == KeyCode::Down {
            if count > 0 {
                self.selected = (self.selected + 1).min(count - 1);
            }
            return Some(SessionsAction::Consumed);
        }
        if key.code == KeyCode::Backspace {
            self.filter.pop();
            self.selected = 0;
            Some(SessionsAction::Consumed)
        } else if let KeyCode::Char(ch) = key.code {
            if !key.modifiers.is_empty() {
                return None;
            }
            self.filter.push(ch);
            self.selected = 0;
            Some(SessionsAction::Consumed)
        } else {
            None
        }
    }
}

/// Status glyph and word, so meaning never rides on color alone.
fn status_parts(status: Option<TaskStatus>) -> (&'static str, &'static str) {
    let set = glyph_set();
    match status {
        Some(TaskStatus::Completed) => (glyph(Glyph::Check, set), "done"),
        Some(TaskStatus::Failed) => (glyph(Glyph::Cross, set), "failed"),
        Some(TaskStatus::Cancelled) => (glyph(Glyph::Cancel, set), "cancelled"),
        Some(TaskStatus::Paused) => (glyph(Glyph::Pending, set), "paused"),
        Some(TaskStatus::Running) => (glyph(Glyph::Bullet, set), "running"),
        Some(_) => (glyph(Glyph::Pending, set), "unknown"),
        None => (glyph(Glyph::Pending, set), "new"),
    }
}

fn status_label(status: Option<TaskStatus>) -> &'static str {
    status_parts(status).1
}

/// UTC `HH:MM` from epoch millis — a relative day is enough here.
fn clock_label(millis: u64) -> String {
    let secs = millis / 1000;
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

fn wall_label(millis: Option<u64>) -> String {
    millis.map_or_else(
        || "—".to_owned(),
        |millis| {
            let secs = millis / 1000;
            if secs >= 3600 {
                format!("{}h {:02}m", secs / 3600, (secs / 60) % 60)
            } else {
                format!("{:02}m {:02}s", (secs / 60) % 60, secs % 60)
            }
        },
    )
}

/// Draws the sessions modal: filter row, RECENT list, PREVIEW pane,
/// and the wireframe's key line.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, screen: &SessionsScreen) {
    let height = area.height.clamp(10, 24);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .style(Style::default().bg(theme::BG))
            .title(Span::styled(
                " ◆ sessions  /sessions ",
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
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(inner);
    let filtered = screen.filtered();
    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(28)])
        .split(rows[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("RECENT · {} of {}", filtered.len(), screen.rows.len()),
            Style::default().fg(theme::AMBER),
        ))),
        header[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if screen.workspace.is_empty() {
                format!("{} of {}", filtered.len(), screen.rows.len())
            } else {
                format!(
                    "{} · {} of {}",
                    screen.workspace,
                    filtered.len(),
                    screen.rows.len()
                )
            },
            Style::default().fg(theme::DIM),
        ))),
        header[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("FILTER ", Style::default().fg(theme::AMBER)),
            Span::styled(
                if screen.filter.is_empty() {
                    "type to filter by id, prompt, or status"
                } else {
                    &screen.filter
                },
                Style::default().fg(theme::DIM),
            ),
        ])),
        rows[1],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[2]);
    draw_list(frame, columns[0], screen, &filtered);
    draw_preview(frame, columns[1], screen, &filtered);
    let footer = screen.command.line().unwrap_or_else(|| {
        Line::from(Span::styled(
            "↑↓ move · Enter resume · [/export] · [/delete] · /new · Esc close",
            Style::default().fg(theme::DIM),
        ))
    });
    frame.render_widget(Paragraph::new(footer), rows[3]);
}

/// The RECENT column: three lines per session, cursor row highlighted.
fn draw_list(frame: &mut Frame<'_>, area: Rect, screen: &SessionsScreen, filtered: &[usize]) {
    let window = list_window(filtered.len(), screen.selected, area.height as usize);
    let mut lines = Vec::new();
    for (position, index) in filtered
        .iter()
        .enumerate()
        .skip(window.0)
        .take(window.1 - window.0)
    {
        let row = &screen.rows[*index];
        let (mark, status) = status_parts(row.status);
        let selected = position == screen.selected;
        let id_style = if selected {
            Style::default()
                .fg(theme::IVORY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        };
        let status_style = if selected {
            Style::default().fg(theme::AMBER)
        } else {
            Style::default().fg(theme::DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} {} ", short_id(&row.id)), id_style),
            Span::styled(status, status_style),
            Span::styled(
                format!("  {} · ", clock_label(row.started_millis)),
                Style::default().fg(theme::DIM),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  {}", row.prompt),
            Style::default().fg(theme::IVORY),
        )));
        let cost = row
            .cost_usd
            .map_or_else(|| "—".to_owned(), |cost| format!("${cost:.4}"));
        lines.push(Line::from(Span::styled(
            format!(
                "  {} turns · {cost} · {}",
                row.turns,
                wall_label(row.wall_millis)
            ),
            Style::default().fg(theme::DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The PREVIEW pane for the row under the cursor.
fn draw_preview(frame: &mut Frame<'_>, area: Rect, screen: &SessionsScreen, filtered: &[usize]) {
    let selected_status = filtered
        .get(screen.selected)
        .and_then(|index| screen.rows[*index].status);
    let amber = Style::default().fg(theme::AMBER);
    let ivory = Style::default().fg(theme::IVORY);
    let preview_lines = match &screen.preview {
        Some(preview) => vec![
            Line::from(vec![
                Span::styled("status  ", amber),
                Span::styled(status_label(selected_status), ivory),
            ]),
            Line::from(vec![
                Span::styled("model   ", amber),
                Span::styled(preview.model.clone(), ivory),
            ]),
            Line::from(vec![
                Span::styled("tool calls ", amber),
                Span::styled(preview.tool_calls.to_string(), ivory),
            ]),
            Line::from(vec![
                Span::styled("cached     ", amber),
                Span::styled(
                    preview
                        .cached_pct
                        .map_or_else(|| "no cache data".to_owned(), |pct| format!("{pct}%")),
                    if preview.cached_pct.is_some() {
                        Style::default().fg(theme::TEAL)
                    } else {
                        ivory
                    },
                ),
            ]),
            Line::from(vec![
                Span::styled("events     ", amber),
                Span::styled(preview.events.to_string(), ivory),
            ]),
            Line::from(Span::styled("answer", amber)),
            Line::from(Span::styled(
                if preview.last_answer.is_empty() {
                    "—"
                } else {
                    &preview.last_answer
                },
                ivory,
            )),
            Line::from(Span::styled("files", amber)),
            Line::from(Span::styled(
                preview
                    .files
                    .is_empty()
                    .then_some("—")
                    .map_or_else(|| preview.files.join("  "), ToOwned::to_owned),
                ivory,
            )),
        ],
        None => vec![Line::from(Span::styled(
            "select a session",
            Style::default().fg(theme::DIM),
        ))],
    };
    let title = filtered.get(screen.selected).map_or_else(
        || " PREVIEW ".into(),
        |index| format!(" PREVIEW · {} ", short_id(&screen.rows[*index].id)),
    );
    frame.render_widget(
        Paragraph::new(preview_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::DOTTED))
                .title(Span::styled(title, amber)),
        ),
        area,
    );
}

/// First 8 characters of a session id, as the wireframe's RECENT list shows it.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// (first visible row, end) so the cursor stays in view.
fn list_window(count: usize, selected: usize, height: usize) -> (usize, usize) {
    let per_row = 3;
    let visible = (height / per_row).max(1);
    if count <= visible {
        return (0, count);
    }
    let start = selected.saturating_sub(visible - 1).min(count - visible);
    (start, start + visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use loom_core::kernel::model::Usage;
    use loom_core::kernel::session::TaskRecord;
    use ratatui::{Terminal, backend::TestBackend};

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(id: &str, status: Option<TaskStatus>) -> Row {
        Row {
            id: id.to_owned(),
            status,
            prompt: format!("prompt {id}"),
            turns: 3,
            wall_millis: Some(65_000),
            cost_usd: Some(0.0034),
            started_millis: 1_730_000_000_000,
        }
    }

    #[test]
    fn load_reads_rows_and_preview_from_a_real_store() {
        let path = std::env::temp_dir().join(format!("loom-sessions-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let id = {
            let store = loom_core::store::SqliteStore::open(&path).unwrap();
            let id = loom_core::SessionId::new();
            store.ensure_session(&id).unwrap();
            store
                .append_item(
                    &id,
                    loom_core::kernel::transcript::ItemIndex::from(0),
                    &loom_core::kernel::transcript::Item::User {
                        text: "Fix the parser\nthen the tests".to_owned(),
                    },
                )
                .unwrap();
            store
                .upsert_task(
                    &id,
                    &TaskRecord {
                        id: loom_core::TaskId::new(),
                        status: TaskStatus::Completed,
                        turns: 4,
                        usage: Usage::default(),
                        started_at_millis: 1_000,
                        completed_at_millis: Some(61_000),
                        effort: None,
                        end_reason: None,
                    },
                )
                .unwrap();
            id
        };
        let mut screen = SessionsScreen::load(&path);
        assert_eq!(screen.rows.len(), 1);
        assert_eq!(screen.rows[0].id, id.to_string());
        assert_eq!(screen.rows[0].prompt, "Fix the parser");
        assert_eq!(screen.rows[0].turns, 4);
        assert_eq!(screen.rows[0].wall_millis, Some(60_000));
        assert_eq!(screen.rows[0].status, Some(TaskStatus::Completed));
        screen.selected = 0;
        screen.refresh_preview(&path);
        let _ = std::fs::remove_file(&path);
        let preview = screen.preview.expect("preview loads for the row");
        assert_eq!(preview.tool_calls, 0);
        assert_eq!(preview.model, "unknown");
    }

    #[test]
    fn filter_narrows_and_keys_return_actions() {
        let mut screen = SessionsScreen {
            rows: vec![
                row("aaa", Some(TaskStatus::Completed)),
                row("bbb", Some(TaskStatus::Failed)),
            ],
            ..SessionsScreen::default()
        };
        assert_eq!(screen.filtered().len(), 2);
        screen.filter = "bbb".into();
        assert_eq!(screen.filtered(), vec![1]);

        screen.filter.clear();
        assert_eq!(
            screen.key(plain(KeyCode::Down)),
            Some(SessionsAction::Consumed)
        );
        assert_eq!(screen.selected, 1);
        assert_eq!(
            screen.key(plain(KeyCode::Enter)),
            Some(SessionsAction::Resume("bbb".into()))
        );
        assert_eq!(
            screen.key(plain(KeyCode::Char('e'))),
            Some(SessionsAction::Export("bbb".into()))
        );
        assert_eq!(
            screen.key(plain(KeyCode::Char('d'))),
            Some(SessionsAction::Delete("bbb".into()))
        );
        assert_eq!(
            screen.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(SessionsAction::NewSession)
        );
        assert_eq!(screen.key(plain(KeyCode::Esc)), Some(SessionsAction::Close));
        // Typing edits the filter and resets the selection.
        screen.selected = 1;
        assert_eq!(
            screen.key(plain(KeyCode::Char('a'))),
            Some(SessionsAction::Consumed)
        );
        assert_eq!(screen.filter, "a");
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn draw_shows_filter_list_preview_and_keys() {
        let screen = SessionsScreen {
            rows: vec![
                row("cccc", Some(TaskStatus::Completed)),
                row("dddd", Some(TaskStatus::Failed)),
            ],
            selected: 0,
            preview: Some(Preview {
                model: "deepseek-flash".into(),
                events: 12,
                tool_calls: 5,
                cached_pct: Some(40),
                last_answer: "done in three turns".into(),
                files: vec!["src/main.rs".into()],
            }),
            ..SessionsScreen::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &screen))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("sessions"));
        assert!(text.contains("FILTER"));
        assert!(text.contains("prompt cccc"));
        assert!(text.contains("PREVIEW"));
        assert!(text.contains("deepseek-flash"));
        assert!(text.contains("done in three turns"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("Enter resume"));
    }
}
