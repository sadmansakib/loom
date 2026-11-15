//! The `/compare` screen: recent runs side by side.
//!
//! Rows come from the store's task records; calls and model are
//! recovered from each session's event log. Cost is an estimate at the
//! current model's pricing — runs on other models show `≈` to say so.
//! Effort is not recorded per run, so the wireframe's effort column is
//! omitted.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use loom_core::EventKind;
use loom_core::kernel::session::TaskStatus;
use loom_core::kernel::store::{EventStore, SessionStore};
use loom_core::kernel::transcript::Item;
use loom_core::store::SqliteStore;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::picker::ModelPricing;
use crate::recent::first_line;
use crate::theme;

pub(crate) struct RunRow {
    session: String,
    task: String,
    status: Option<TaskStatus>,
    model: String,
    turns: u32,
    calls: u64,
    wall_millis: Option<u64>,
    input: u64,
    output: u64,
    cached: Option<u64>,
    prompt: String,
}

pub(crate) struct CompareScreen {
    rows: Vec<RunRow>,
    filter: String,
    selected: usize,
    baseline: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompareAction {
    Consumed,
    Close,
    Open(String),
    ExportNdjson,
    ExportCsv,
}

impl CompareScreen {
    pub(crate) fn load(db_path: &Path) -> Self {
        let mut rows = Vec::new();
        if db_path.exists()
            && let Ok(store) = SqliteStore::open(db_path)
            && let Ok(ids) = store.session_ids()
        {
            for id in ids.iter().rev() {
                let Ok(Some(session)) = store.load(id) else {
                    continue;
                };
                let prompt = session
                    .transcript
                    .iter()
                    .find_map(|item| match item {
                        Item::User { text } => Some(first_line(text, 34)),
                        Item::Assistant(_) | Item::ToolResult(_) | _ => None,
                    })
                    .unwrap_or_else(|| "(no prompt)".into());
                let events = store.load_events(id).unwrap_or_default();
                let model = events
                    .iter()
                    .find_map(|event| {
                        if let EventKind::CapabilitiesResolved { model, .. } = &event.kind {
                            Some(model.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "unknown".into());
                for task in &session.tasks {
                    let calls = events
                        .iter()
                        .filter(|event| {
                            event.task_id == task.id
                                && matches!(event.kind, EventKind::ToolCompleted { .. })
                        })
                        .count();
                    rows.push(RunRow {
                        session: id.to_string(),
                        task: short_id(&task.id.to_string()),
                        status: Some(task.status),
                        model: model.clone(),
                        turns: task.turns,
                        calls: u64::try_from(calls).unwrap_or(u64::MAX),
                        wall_millis: task
                            .completed_at_millis
                            .map(|end| end.saturating_sub(task.started_at_millis)),
                        input: task.usage.input_tokens,
                        output: task.usage.output_tokens,
                        cached: task.usage.cached_input_tokens,
                        prompt: prompt.clone(),
                    });
                }
            }
        }
        rows.truncate(12);
        Self {
            rows,
            filter: String::new(),
            selected: 0,
            baseline: None,
        }
    }

    fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        (0..self.rows.len())
            .filter(|&i| {
                let row = &self.rows[i];
                row.prompt.to_lowercase().contains(&needle)
                    || row.model.to_lowercase().contains(&needle)
                    || status_label(row.status).to_lowercase().contains(&needle)
                    || row.task.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The rows to export, in table order.
    pub(crate) fn visible(&self) -> Vec<&RunRow> {
        self.filtered().into_iter().map(|i| &self.rows[i]).collect()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<CompareAction> {
        if key.code == KeyCode::Esc {
            return Some(CompareAction::Close);
        }
        if key.code == KeyCode::Up {
            self.selected = self.selected.saturating_sub(1);
            return Some(CompareAction::Consumed);
        }
        if key.code == KeyCode::Down {
            let last = self.filtered().len().saturating_sub(1);
            self.selected = Ord::clamp(self.selected + 1, 0, last);
            return Some(CompareAction::Consumed);
        }
        if key.modifiers.is_empty() {
            if key.code == KeyCode::Char(' ') || key.code == KeyCode::Char('b') {
                let visible = self.filtered();
                let index = visible.get(self.selected).copied();
                self.baseline = index;
                return Some(CompareAction::Consumed);
            }
            if key.code == KeyCode::Enter {
                let visible = self.filtered();
                let session = visible
                    .get(self.selected)
                    .map(|i| self.rows[*i].session.clone())?;
                return Some(CompareAction::Open(session));
            }
            if key.code == KeyCode::Char('e') {
                return Some(CompareAction::ExportNdjson);
            }
            if key.code == KeyCode::Char('c') {
                return Some(CompareAction::ExportCsv);
            }
            if let KeyCode::Char(ch) = key.code {
                self.filter.push(ch);
                self.selected = 0;
                return Some(CompareAction::Consumed);
            }
        }
        if key.code == KeyCode::Backspace {
            self.filter.pop();
            self.selected = 0;
            return Some(CompareAction::Consumed);
        }
        None
    }
}

fn short_id(id: &str) -> String {
    id.chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub(crate) fn status_label(status: Option<TaskStatus>) -> &'static str {
    match status {
        Some(TaskStatus::Completed) | None => "completed",
        Some(TaskStatus::Failed) => "failed",
        Some(TaskStatus::Cancelled) => "cancelled",
        Some(TaskStatus::Paused) => "paused",
        Some(TaskStatus::Running) => "running",
        Some(_) => "ended",
    }
}

fn wall_label(millis: Option<u64>) -> String {
    match millis {
        None => "—".into(),
        Some(ms) if ms < 3_600_000 => format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000),
        Some(ms) => format!("{}h {:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000),
    }
}

fn cost_label(row: &RunRow, pricing: Option<ModelPricing>) -> String {
    pricing
        .and_then(|p| p.estimate(row.input, row.output))
        .map_or_else(|| "—".into(), |cost| format!("≈${cost:.3}"))
}

fn cache_label(row: &RunRow) -> String {
    match (row.cached, row.input) {
        (Some(cached), input) if input > 0 => {
            format!("{}%", u128::from(cached) * 100 / u128::from(input))
        }
        _ => "—".into(),
    }
}

/// One table row line: baseline marker, selection, status, model,
/// numbers, prompt.
fn row_line(
    row: &RunRow,
    selected: bool,
    is_baseline: bool,
    pricing: Option<ModelPricing>,
    ivory: Style,
    amber: Style,
    dim: Style,
) -> Line<'static> {
    let marker = if is_baseline { "◆" } else { " " };
    let mut spans = vec![Span::styled(
        if selected {
            format!("{marker}❯ ")
        } else {
            format!("{marker}  ")
        },
        amber,
    )];
    spans.push(Span::styled(
        format!("{:<10} ", row.task),
        if selected {
            ivory.add_modifier(Modifier::BOLD)
        } else {
            ivory
        },
    ));
    spans.push(Span::styled(
        format!("{:<9} ", status_label(row.status)),
        Style::default().fg(match row.status {
            Some(TaskStatus::Completed) | None => theme::SAGE,
            Some(TaskStatus::Failed | TaskStatus::Cancelled) => theme::CORAL,
            _ => theme::DIM,
        }),
    ));
    spans.push(Span::styled(
        format!("{:<16} ", row.model),
        Style::default().fg(theme::BLUE),
    ));
    spans.push(Span::styled(
        format!(
            "{:<5} {:<5} {:<8} ",
            row.turns,
            row.calls,
            wall_label(row.wall_millis)
        ),
        dim,
    ));
    spans.push(Span::styled(
        format!("{:<8} {}", cost_label(row, pricing), cache_label(row)),
        dim,
    ));
    spans.push(Span::styled(format!("  {}", row.prompt), dim));
    Line::from(spans)
}

pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &CompareScreen,
    pricing: Option<ModelPricing>,
) {
    let height = area.height.clamp(16, 28);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .title(Span::styled(
                " runs · compare ",
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
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);
    let amber = Style::default().fg(theme::AMBER);
    let dim = Style::default().fg(theme::DIM);
    let ivory = Style::default().fg(theme::IVORY);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("FILTER ", amber),
            Span::styled(
                if screen.filter.is_empty() {
                    "all runs".into()
                } else {
                    screen.filter.clone()
                },
                ivory,
            ),
        ])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "task        st        model            turns calls wall     cost    cache",
            dim,
        ))),
        rows[1],
    );

    let visible = screen.filtered();
    let mut lines = Vec::new();
    for (position, index) in visible.iter().enumerate() {
        let row = &screen.rows[*index];
        let is_baseline = screen
            .baseline
            .is_some_and(|i| screen.rows.get(i).is_some_and(|base| base.task == row.task));
        lines.push(row_line(
            row,
            position == screen.selected,
            is_baseline,
            pricing,
            ivory,
            amber,
            dim,
        ));
    }
    if let Some(diff) = baseline_diff(screen) {
        lines.push(Line::from(Span::styled("", dim)));
        lines.push(Line::from(Span::styled(
            diff,
            Style::default().fg(theme::SAGE),
        )));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("no runs match the filter", dim)));
    }
    frame.render_widget(Paragraph::new(lines), rows[2]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ move · Space baseline · Enter open · e ndjson · c csv · Esc back",
            dim,
        ))),
        rows[3],
    );
}

/// The `VS BASELINE` deltas: selected run against the marked baseline.
pub(crate) fn baseline_diff(screen: &CompareScreen) -> Option<String> {
    let baseline = screen.baseline.and_then(|i| screen.rows.get(i))?;
    let selected = screen
        .filtered()
        .get(screen.selected)
        .and_then(|i| screen.rows.get(*i))?;
    let delta = |sel: u64, base: u64| match sel.cmp(&base) {
        std::cmp::Ordering::Equal => "±0".into(),
        std::cmp::Ordering::Greater => format!("+{}", sel - base),
        std::cmp::Ordering::Less => format!("-{}", base - sel),
    };
    Some(format!(
        "vs baseline {}: turns {} calls {} tokens {}",
        baseline.task,
        delta(u64::from(selected.turns), u64::from(baseline.turns)),
        delta(selected.calls, baseline.calls),
        delta(
            selected.input + selected.output,
            baseline.input + baseline.output
        ),
    ))
}

/// NDJSON export of the visible rows.
pub(crate) fn ndjson(screen: &CompareScreen) -> String {
    screen
        .visible()
        .iter()
        .map(|row| {
            serde_json::json!({
                "task": row.task,
                "session": row.session,
                "status": status_label(row.status),
                "model": row.model,
                "turns": row.turns,
                "calls": row.calls,
                "wall_millis": row.wall_millis,
                "input_tokens": row.input,
                "output_tokens": row.output,
                "cached_input_tokens": row.cached,
                "prompt": row.prompt,
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// CSV export of the visible rows.
pub(crate) fn csv(screen: &CompareScreen) -> String {
    let mut out = String::from(
        "task,session,status,model,turns,calls,wall_millis,input,output,cached,prompt\n",
    );
    for row in screen.visible() {
        let prompt = row.prompt.replace(['"', ',', '\n'], " ");
        let fields = [
            row.task.clone(),
            row.session.clone(),
            status_label(row.status).to_string(),
            row.model.clone(),
            row.turns.to_string(),
            row.calls.to_string(),
            row.wall_millis.map_or_else(String::new, |v| v.to_string()),
            row.input.to_string(),
            row.output.to_string(),
            row.cached.map_or_else(String::new, |v| v.to_string()),
            prompt,
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_loads_no_rows_and_keys_close() {
        let mut screen = CompareScreen::load(Path::new("/nonexistent/loom.db"));
        assert!(screen.rows.is_empty());
        assert_eq!(screen.key(KeyCode::Esc.into()), Some(CompareAction::Close));
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Char('e'))),
            Some(CompareAction::ExportNdjson)
        );
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Char('c'))),
            Some(CompareAction::ExportCsv)
        );
        assert_eq!(ndjson(&screen), "");
        assert_eq!(
            csv(&screen),
            "task,session,status,model,turns,calls,wall_millis,input,output,cached,prompt\n"
        );
    }

    #[test]
    fn short_ids_take_the_last_six() {
        assert_eq!(short_id("01a1dfa6-5835-76f8-8fe0-3e9d43e78c36"), "e78c36");
    }
}
