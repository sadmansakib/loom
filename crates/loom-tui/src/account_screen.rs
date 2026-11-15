//! The `/account` screen (wireframes J1–J3): list accounts, switch the
//! active one, remove it, or add a provider. Verify and model choice
//! (J4–J5) are the existing model picker: adding or testing an account
//! opens discovery, which is the connection test.

use crossterm::event::{KeyCode, KeyEvent};
use loom_core::engine::auth::Account;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme;

/// The add-a-provider wizard's choice of type (J2).
const TYPES: [(&str, &str); 2] = [
    ("deepseek", "hosted · api.deepseek.com"),
    ("openai-compatible", "any OpenAI-compatible /v1 endpoint"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stage {
    List,
    Type,
    Credentials,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AccountAction {
    Consumed,
    Close,
    /// Enter: make this account the provider's active one.
    Activate {
        provider: String,
        label: String,
    },
    /// d: remove the account.
    Remove {
        provider: String,
        label: String,
    },
    /// t: test the connection — the caller opens model discovery.
    Test {
        provider: String,
    },
    /// The credentials form is complete: save and verify.
    Add {
        provider: String,
        label: String,
        base_url: String,
        key: String,
    },
}

pub(crate) struct AccountScreen {
    pub(crate) stage: Stage,
    pub(crate) accounts: Vec<Account>,
    pub(crate) selected: usize,
    /// Add-wizard state (J2/J3).
    pub(crate) type_selected: usize,
    pub(crate) label: String,
    pub(crate) base_url: String,
    pub(crate) key: String,
    /// Which credentials field Tab is on: 0 label, 1 base URL, 2 key.
    pub(crate) field: usize,
}

impl AccountScreen {
    pub(crate) fn new(accounts: Vec<Account>) -> Self {
        Self {
            stage: Stage::List,
            accounts,
            selected: 0,
            type_selected: 0,
            label: String::new(),
            base_url: String::new(),
            key: String::new(),
            field: 0,
        }
    }

    /// The provider name the wizard registers: the label for
    /// openai-compatible (each endpoint needs its own registry name),
    /// or the builtin name for hosted.
    fn wizard_provider(&self) -> String {
        if self.type_selected == 0 {
            TYPES[0].0.into()
        } else {
            let label = if self.label.is_empty() {
                "local"
            } else {
                self.label.as_str()
            };
            label.replace(' ', "-").to_lowercase()
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<AccountAction> {
        match self.stage {
            Stage::List => self.list_key(key),
            Stage::Type => self.type_key(key),
            Stage::Credentials => self.credentials_key(key),
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<AccountAction> {
        if key.code == KeyCode::Esc {
            return Some(AccountAction::Close);
        }
        if key.code == KeyCode::Up {
            self.selected = self.selected.saturating_sub(1);
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Down {
            let last = self.accounts.len().saturating_sub(1);
            self.selected = Ord::clamp(self.selected + 1, 0, last);
            return Some(AccountAction::Consumed);
        }
        if !key.modifiers.is_empty() {
            return None;
        }
        if key.code == KeyCode::Char('a') {
            self.stage = Stage::Type;
            self.label.clear();
            self.base_url.clear();
            self.key.clear();
            self.field = 0;
            return Some(AccountAction::Consumed);
        }
        let account = self.accounts.get(self.selected).cloned()?;
        if key.code == KeyCode::Enter {
            return Some(AccountAction::Activate {
                provider: account.provider,
                label: account.label,
            });
        }
        if key.code == KeyCode::Char('d') {
            return Some(AccountAction::Remove {
                provider: account.provider,
                label: account.label,
            });
        }
        if key.code == KeyCode::Char('t') {
            return Some(AccountAction::Test {
                provider: account.provider,
            });
        }
        if let KeyCode::Char(ch) = key.code
            && ch.is_ascii_graphic()
        {
            return Some(AccountAction::Consumed);
        }
        None
    }

    fn type_key(&mut self, key: KeyEvent) -> Option<AccountAction> {
        if key.code == KeyCode::Esc {
            self.stage = Stage::List;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Up && self.type_selected > 0 {
            self.type_selected -= 1;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Down && self.type_selected + 1 < TYPES.len() {
            self.type_selected += 1;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Enter {
            self.stage = Stage::Credentials;
            return Some(AccountAction::Consumed);
        }
        None
    }

    fn credentials_key(&mut self, key: KeyEvent) -> Option<AccountAction> {
        if key.code == KeyCode::Esc {
            self.stage = Stage::List;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Tab {
            let fields = if self.type_selected == 0 { 2 } else { 3 };
            self.field = (self.field + 1) % fields;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::BackTab {
            let fields = if self.type_selected == 0 { 2 } else { 3 };
            self.field = (self.field + fields - 1) % fields;
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Backspace {
            let target = self.text_field_mut();
            target.pop();
            return Some(AccountAction::Consumed);
        }
        if let KeyCode::Char(ch) = key.code
            && (key.modifiers.is_empty()
                || key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT))
        {
            self.text_field_mut().push(ch);
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Enter {
            if self.key.is_empty() {
                return Some(AccountAction::Consumed); // nothing to save yet
            }
            return Some(AccountAction::Add {
                provider: self.wizard_provider(),
                label: if self.label.is_empty() {
                    self.wizard_provider()
                } else {
                    self.label.clone()
                },
                base_url: if self.type_selected == 0 {
                    String::new()
                } else {
                    self.base_url.clone()
                },
                key: self.key.clone(),
            });
        }
        None
    }

    fn text_field_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.label,
            1 if self.type_selected == 1 => &mut self.base_url,
            _ => &mut self.key,
        }
    }
}

fn method_label(method: &str) -> &'static str {
    match method {
        "key" => "keychain key",
        "command" => "command",
        _ => "environment",
    }
}

/// The stage-dependent middle of the modal.
fn body_lines(screen: &AccountScreen) -> Vec<Line<'static>> {
    let amber = Style::default().fg(theme::AMBER);
    let dim = Style::default().fg(theme::DIM);
    let ivory = Style::default().fg(theme::IVORY);
    let sage = Style::default().fg(theme::SAGE);
    match screen.stage {
        Stage::List => list_lines(screen, amber, dim, ivory, sage),
        Stage::Type => type_lines(screen, amber, dim, ivory),
        Stage::Credentials => credentials_lines(screen, amber, dim, ivory),
    }
}

fn list_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
    sage: Style,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("ACCOUNTS", amber))];
    if screen.accounts.is_empty() {
        lines.push(Line::from(Span::styled(
            "no accounts yet — press a to add a provider",
            dim,
        )));
    }
    for (index, account) in screen.accounts.iter().enumerate() {
        let selected = index == screen.selected;
        let glyph = if account.active { "✓" } else { "○" };
        let status_color = if account.active { sage } else { dim };
        let last4 = account
            .last4
            .as_deref()
            .map_or_else(String::new, |four| format!(" •••• {four}"));
        lines.push(Line::from(vec![
            Span::styled(if selected { "❯ " } else { "  " }, amber),
            Span::styled(
                format!("{glyph} {} · {}", account.provider, account.label),
                if selected {
                    ivory.add_modifier(Modifier::BOLD)
                } else {
                    ivory
                },
            ),
            Span::styled(
                format!(
                    "  · {}{}{}",
                    if account.active { "active · " } else { "" },
                    method_label(account.method.as_str()),
                    last4
                ),
                status_color,
            ),
        ]));
    }
    lines.push(Line::from(Span::styled("", dim)));
    lines.push(Line::from(Span::styled(
        "a add a provider   t test connection",
        sage,
    )));
    lines
}

fn type_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("CHOOSE A TYPE", amber))];
    for (index, (name, blurb)) in TYPES.iter().enumerate() {
        let selected = index == screen.type_selected;
        lines.push(Line::from(vec![
            Span::styled(if selected { "❯ " } else { "  " }, amber),
            Span::styled(
                format!("{name:<20}"),
                if selected {
                    ivory.add_modifier(Modifier::BOLD)
                } else {
                    ivory
                },
            ),
            Span::styled((*blurb).to_string(), dim),
        ]));
    }
    lines.push(Line::from(Span::styled("", dim)));
    lines.push(Line::from(Span::styled(
        "YOU WILL NEED an API key (and a base URL for openai-compatible).",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "Loom tests the connection before anything is used: the model picker's discovery is the test.",
        dim,
    )));
    lines
}

fn credentials_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("CREDENTIALS", amber))];
    let fields: [(&str, &str); 3] = [
        ("label", screen.label.as_str()),
        (
            "base URL",
            if screen.type_selected == 1 {
                screen.base_url.as_str()
            } else {
                "(not needed for deepseek)"
            },
        ),
        ("key (pasted, stored in the OS keychain)", &screen.key),
    ];
    for (index, (name, value)) in fields.iter().enumerate() {
        if index == 1 && screen.type_selected == 0 {
            lines.push(Line::from(Span::styled(
                format!("  {name:<6} (not needed for deepseek)"),
                dim,
            )));
            continue;
        }
        let active = index == screen.field;
        lines.push(Line::from(vec![
            Span::styled(if active { "❯ " } else { "  " }, amber),
            Span::styled(format!("{name:<6} "), if active { ivory } else { dim }),
            Span::raw((*value).to_string()),
        ]));
    }
    lines.push(Line::from(Span::styled("", dim)));
    lines.push(Line::from(Span::styled(
        "Keys never enter the transcript, run record, or event stream.",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "Tab next field · Enter saves and opens discovery (verify + model)",
        dim,
    )));
    lines
}
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, screen: &AccountScreen) {
    let height = area.height.clamp(14, 26);
    let modal = crate::modal::centered(area, height);
    crate::modal::dim_scene(frame, area, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::AMBER))
            .title(Span::styled(
                " /account ",
                Style::default()
                    .fg(theme::AMBER)
                    .add_modifier(Modifier::BOLD),
            )),
        modal,
    );
    let inner = modal.inner(ratatui::layout::Margin::new(1, 1));
    let dim = Style::default().fg(theme::DIM);

    let breadcrumb = match screen.stage {
        Stage::List => "providers",
        Stage::Type => "providers · type",
        Stage::Credentials => "providers · type · credentials · verify · model",
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(breadcrumb, dim))),
        rows[0],
    );

    let lines = body_lines(screen);
    frame.render_widget(Paragraph::new(lines), rows[1]);
    if screen.stage == Stage::List {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "NOTE a task keeps the account it started with",
                dim,
            ))),
            rows[2],
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            match screen.stage {
                Stage::List => "↑↓ move · Enter switch · t test · a add · d remove · Esc back",
                Stage::Type => "↑↓ move · Enter choose · Esc back",
                Stage::Credentials => "Tab next field · Enter save + verify · Esc back",
            },
            dim,
        ))),
        rows[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(provider: &str, label: &str, active: bool) -> Account {
        Account {
            id: loom_core::engine::auth::AccountId::new(),
            provider: provider.into(),
            label: label.into(),
            method: loom_core::engine::auth::Method::Key,
            active,
            created_at_millis: 0,
            expires_at_millis: None,
            command: None,
            last4: Some("3f9a".into()),
        }
    }

    #[test]
    fn list_keys_switch_test_and_remove() {
        let mut screen = AccountScreen::new(vec![
            account("deepseek", "work", true),
            account("deepseek", "personal", false),
        ]);
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Enter)),
            Some(AccountAction::Activate {
                provider: "deepseek".into(),
                label: "work".into()
            })
        );
        screen.selected = 1;
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Char('t'))),
            Some(AccountAction::Test {
                provider: "deepseek".into()
            })
        );
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Char('d'))),
            Some(AccountAction::Remove {
                provider: "deepseek".into(),
                label: "personal".into()
            })
        );
        assert_eq!(screen.key(KeyCode::Esc.into()), Some(AccountAction::Close));
    }

    #[test]
    fn add_wizard_walks_type_then_credentials() {
        let mut screen = AccountScreen::new(vec![]);
        screen.key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(screen.stage, Stage::Type);
        screen.key(KeyCode::Enter.into()); // deepseek
        assert_eq!(screen.stage, Stage::Credentials);
        for ch in "mykey".chars() {
            screen.key(KeyEvent::from(KeyCode::Char(ch)));
        }
        screen.key(KeyCode::Tab.into()); // to key field
        for ch in "sk-test".chars() {
            screen.key(KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(
            screen.key(KeyCode::Enter.into()),
            Some(AccountAction::Add {
                provider: "deepseek".into(),
                label: "mykey".into(),
                base_url: String::new(),
                key: "sk-test".into(),
            })
        );
    }

    #[test]
    fn draw_lists_accounts_and_the_note() {
        let backend = ratatui::backend::TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let screen = AccountScreen::new(vec![account("deepseek", "work", true)]);
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 130, 30), &screen))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for needle in [
            "/account",
            "ACCOUNTS",
            "deepseek · work",
            "active",
            "keychain key",
            "•••• 3f9a",
            "NOTE a task keeps the account",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }
}
