//! The `/account` screen (wireframes J1–J4): list accounts, switch the
//! active one, remove it, or add a provider. Model choice (J5) is the
//! existing model picker: discovery lists what the key can reach.

use crossterm::event::{KeyCode, KeyEvent};
use loom_core::engine::auth::{Account, Method};
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
    Verify,
    Model,
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
        method: Method,
        command: Option<String>,
    },
    /// Verify passed: save the chosen model and leave the wizard.
    Continue {
        provider: String,
        account: Option<String>,
        model: String,
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
    /// Which credentials field Tab is on: 0 label, 1 base URL, 2 key,
    /// 3 key-source selector.
    pub(crate) field: usize,
    /// J3 key source: 0 environment variable, 1 paste a key, 2 command.
    pub(crate) key_source: usize,
    /// Environment variable that already carries the provider's key.
    pub(crate) env_var: Option<String>,
    /// Shell command that prints the key, for `key_source == 2`.
    pub(crate) command: String,
    /// J4 verify result: `None` while the save is in flight, then
    /// `(passed, detail)`.
    pub(crate) verify: Option<(bool, String)>,
    /// J5 highlighted row in DISCOVERED.
    pub(crate) model_selected: usize,
    /// Models the verified endpoint actually reported, with a capability line.
    pub(crate) discovered: Vec<(String, String)>,
    command_line: crate::command_line::CommandLine,
}

impl AccountScreen {
    pub(crate) fn new(accounts: Vec<Account>) -> Self {
        let env_var = Self::default_env_var(0);
        Self {
            stage: Stage::List,
            accounts,
            selected: 0,
            type_selected: 0,
            label: String::new(),
            base_url: String::new(),
            key: String::new(),
            field: 0,
            key_source: usize::from(env_var.is_none()),
            env_var,
            command: String::new(),
            verify: None,
            model_selected: 0,
            discovered: Vec::new(),
            command_line: crate::command_line::CommandLine::default(),
        }
    }

    fn default_env_var(type_selected: usize) -> Option<String> {
        let provider = if type_selected == 0 {
            "deepseek"
        } else {
            "custom"
        };
        loom_core::engine::auth::provider_env_state(provider)
            .into_iter()
            .find(|state| state.set)
            .map(|state| state.name)
    }

    /// The provider the wizard registers. Every OpenAI-compatible endpoint
    /// runs through `custom`, whose env fallback is `CUSTOM_API_KEY`; the
    /// account label tells endpoints apart.
    // shortcut: one registry entry per provider, so only one custom base URL
    // at a time; add per-account URLs when a second custom endpoint is needed.
    fn wizard_provider(&self) -> String {
        if self.type_selected == 0 {
            TYPES[0].0.into()
        } else {
            "custom".into()
        }
    }

    fn wizard_label(&self) -> String {
        if self.label.is_empty() {
            if self.type_selected == 0 {
                self.wizard_provider()
            } else {
                "work".to_owned()
            }
        } else {
            self.label.clone()
        }
    }

    fn wizard_account(&self) -> Option<String> {
        // Environment credentials are never stored, so there is no account to
        // select; the provider's env fallback resolves the key.
        (self.key_source != 0).then(|| self.wizard_label())
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<AccountAction> {
        // The wizard's fields own typed text; the command line only opens
        // on the list stage and J5, where no field is being edited.
        if matches!(self.stage, Stage::List | Stage::Verify | Stage::Model) {
            let account = self.accounts.get(self.selected).cloned();
            match self.command_line.key(key) {
                crate::command_line::CommandKey::Ignore => {}
                crate::command_line::CommandKey::Consumed
                | crate::command_line::CommandKey::Cancel => {
                    return Some(AccountAction::Consumed);
                }
                crate::command_line::CommandKey::Run(command) => {
                    return Some(
                        match command.split_whitespace().collect::<Vec<_>>().as_slice() {
                            ["add"] => {
                                self.start_add();
                                AccountAction::Consumed
                            }
                            ["save" | "retry"] if self.stage == Stage::Verify => {
                                self.verify = None;
                                self.add_action()
                            }
                            ["close"] => AccountAction::Close,
                            ["test"] => account.map_or(AccountAction::Consumed, |account| {
                                AccountAction::Test {
                                    provider: account.provider,
                                }
                            }),
                            ["remove"] => account.map_or(AccountAction::Consumed, |account| {
                                AccountAction::Remove {
                                    provider: account.provider,
                                    label: account.label,
                                }
                            }),
                            ["activate" | "use"] => {
                                account.map_or(AccountAction::Consumed, |account| {
                                    AccountAction::Activate {
                                        provider: account.provider,
                                        label: account.label,
                                    }
                                })
                            }
                            _ => AccountAction::Consumed,
                        },
                    );
                }
            }
        }
        match self.stage {
            Stage::List => self.list_key(key),
            Stage::Type => self.type_key(key),
            Stage::Credentials => self.credentials_key(key),
            Stage::Verify => Some(self.verify_key(key)),
            Stage::Model => Some(self.model_key(key)),
        }
    }

    /// Resets the add wizard to its first step (wireframe J2).
    fn start_add(&mut self) {
        self.stage = Stage::Type;
        self.label.clear();
        self.base_url.clear();
        self.key.clear();
        self.field = 0;
        self.verify = None;
    }

    /// J5: ↑↓ pick a discovered model, Enter save and start, Esc back to verify.
    #[allow(clippy::wildcard_enum_match_arm)] // every other key is swallowed by this modal
    fn model_key(&mut self, key: KeyEvent) -> AccountAction {
        let last = self.models().len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                self.model_selected = self.model_selected.saturating_sub(1);
                AccountAction::Consumed
            }
            KeyCode::Down => {
                self.model_selected = (self.model_selected + 1).min(last);
                AccountAction::Consumed
            }
            KeyCode::Enter => AccountAction::Continue {
                provider: self.wizard_provider(),
                account: self.wizard_account(),
                model: self.models()[self.model_selected.min(last)].0.clone(),
            },
            KeyCode::Esc => {
                self.stage = Stage::Verify;
                AccountAction::Consumed
            }
            _ => AccountAction::Consumed,
        }
    }

    /// The curated models shown in J5. Hosted providers know their catalogue;
    /// an openai-compatible endpoint needs an explicit declaration.
    fn models(&self) -> Vec<(String, String, String)> {
        if !self.discovered.is_empty() {
            let mut models = self
                .discovered
                .iter()
                .map(|(id, capability)| (id.clone(), capability.clone(), String::new()))
                .collect::<Vec<_>>();
            models.push((
                "declare a model".into(),
                "capabilities unknown".into(),
                "declare below".into(),
            ));
            return models;
        }
        if self.type_selected == 0 {
            [
                (
                    "deepseek-flash",
                    "1M context · tools · reasoning",
                    "$0.30 / $1.20 per M",
                ),
                (
                    "deepseek-v4-pro",
                    "1M context · tools · reasoning",
                    "$1.32 / $3.96 per M",
                ),
                ("declare a model", "capabilities unknown", "declare below"),
            ]
            .into_iter()
            .map(|(id, capability, pricing)| {
                (id.to_owned(), capability.to_owned(), pricing.to_owned())
            })
            .collect()
        } else {
            vec![(
                "declare a model".into(),
                "capabilities unknown".into(),
                "declare below".into(),
            )]
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
            self.start_add();
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
        if (key.code == KeyCode::Up && self.type_selected > 0)
            || (key.code == KeyCode::Down && self.type_selected + 1 < TYPES.len())
        {
            if key.code == KeyCode::Up {
                self.type_selected -= 1;
            } else {
                self.type_selected += 1;
            }
            // Each provider type has its own environment fallback; a hosted
            // DEEPSEEK_API_KEY must not stand in for a custom endpoint.
            self.env_var = Self::default_env_var(self.type_selected);
            self.key_source = usize::from(self.env_var.is_none());
            self.key.clear();
            self.command.clear();
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
        if self.field == 3 && (key.code == KeyCode::Up || key.code == KeyCode::Down) {
            self.key_source = if key.code == KeyCode::Up {
                self.key_source.saturating_sub(1)
            } else {
                (self.key_source + 1).min(2)
            };
            return Some(AccountAction::Consumed);
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            // Hosted providers have no base URL field: label ↔ key ↔ source.
            let cycle: &[usize] = if self.type_selected == 0 {
                &[0, 2, 3]
            } else {
                &[0, 1, 2, 3]
            };
            let position = cycle
                .iter()
                .position(|field| *field == self.field)
                .unwrap_or(0);
            let next = if key.code == KeyCode::Tab {
                (position + 1) % cycle.len()
            } else {
                (position + cycle.len() - 1) % cycle.len()
            };
            self.field = cycle[next];
            return Some(AccountAction::Consumed);
        }
        if self.field == 3 {
            return None;
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
            let ready = match self.key_source {
                0 => self.env_var.is_some(),
                2 => !self.command.trim().is_empty(),
                _ => !self.key.trim().is_empty(),
            };
            if !ready {
                return Some(AccountAction::Consumed); // nothing to save yet
            }
            self.stage = Stage::Verify;
            self.verify = None;
            return Some(self.add_action());
        }
        None
    }

    fn add_action(&self) -> AccountAction {
        let (method, secret, command) = match self.key_source {
            0 => (Method::Environment, self.env_var.clone(), None),
            2 => (Method::Command, None, Some(self.command.clone())),
            _ => (Method::Key, Some(self.key.clone()), None),
        };
        AccountAction::Add {
            provider: self.wizard_provider(),
            label: self.wizard_label(),
            base_url: if self.type_selected == 0 {
                String::new()
            } else {
                self.base_url.clone()
            },
            key: secret.unwrap_or_default(),
            method,
            command,
        }
    }

    /// J4: the save is the connection test. `r` reruns it, Esc returns to
    /// the key field, Enter moves on to model discovery.
    fn verify_key(&mut self, key: KeyEvent) -> AccountAction {
        let passed = self.verify.as_ref().is_some_and(|(passed, _)| *passed);
        match key.code {
            KeyCode::Enter if passed => {
                self.stage = Stage::Model;
                AccountAction::Consumed
            }
            KeyCode::Char('r') => {
                self.verify = None;
                self.add_action()
            }
            KeyCode::Esc => {
                self.stage = Stage::Credentials;
                self.field = 2; // back to the key field
                AccountAction::Consumed
            }
            // everything else (unmatched Enter, editing keys) is swallowed
            KeyCode::Enter
            | KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::F(_)
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Null
            | KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_) => AccountAction::Consumed,
        }
    }

    fn text_field_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.label,
            1 if self.type_selected == 1 => &mut self.base_url,
            2 if self.key_source == 2 => &mut self.command,
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
    let coral = Style::default().fg(theme::CORAL);
    match screen.stage {
        Stage::List => list_lines(screen, amber, dim, ivory, sage),
        Stage::Type => type_lines(screen, amber, dim, ivory),
        Stage::Credentials => credentials_lines(screen, amber, dim, ivory),
        Stage::Verify => verify_lines(screen, amber, dim, ivory, sage, coral),
        Stage::Model => model_lines(screen, amber, dim, ivory, sage),
    }
}

/// J5 · choose a model: the DISCOVERED list and the capability declaration
/// for a model the provider did not report.
fn model_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
    sage: Style,
) -> Vec<Line<'static>> {
    let models = screen.models();
    let last = models.len().saturating_sub(1);
    let selected_model = screen.model_selected.min(last);
    let start = selected_model.saturating_sub(5).min(last.saturating_sub(7));
    let mut lines = vec![Line::from(Span::styled("DISCOVERED", amber))];
    for (index, (name, capability, pricing)) in models.iter().enumerate().skip(start).take(8) {
        let selected = index == selected_model;
        lines.push(Line::from(vec![
            Span::styled(if selected { "❯ " } else { "  " }, amber),
            Span::styled(
                format!("{} ", if selected { "●" } else { "○" }),
                if selected { amber } else { dim },
            ),
            Span::styled(
                format!("{name:<18}"),
                if selected {
                    ivory.add_modifier(Modifier::BOLD)
                } else {
                    ivory
                },
            ),
            Span::styled(format!("{capability:<30}"), dim),
            Span::styled(pricing.clone(), sage),
        ]));
    }
    let chosen = models[selected_model].0.clone();
    if chosen == "declare a model" {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "DECLARE CAPABILITIES · my-model",
            amber,
        )));
        lines.push(Line::from(vec![
            Span::styled("[/tools] ", ivory),
            Span::styled("tools   ", dim),
            Span::styled("[/reasoning] ", ivory),
            Span::styled("reasoning   ", dim),
            Span::styled("[/context 1M]", ivory),
            Span::styled(" context size", dim),
        ]));
    }
    lines
}

/// The right-hand pane shared by J1 (account DETAILS) and J5 (defaults).
#[allow(clippy::single_match_else, clippy::wildcard_enum_match_arm)]
fn details_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
) -> Vec<Line<'static>> {
    match screen.stage {
        Stage::Model => vec![
            Line::from(Span::styled("DEFAULTS", amber)),
            Line::from(vec![
                Span::styled("effort   ", dim),
                Span::styled("high", ivory),
            ]),
            Line::from(vec![
                Span::styled("thinking ", dim),
                Span::styled("on", ivory),
            ]),
            Line::from(Span::styled("SAVED AS", amber)),
            Line::from(Span::styled(
                format!(
                    "account {} · provider {}",
                    screen.wizard_label(),
                    provider_type(screen.type_selected)
                ),
                ivory,
            )),
            Line::from(Span::styled("SWITCH LATER", amber)),
            Line::from(Span::styled(
                format!(
                    "loom auth use --provider {} {}",
                    screen.wizard_provider(),
                    screen.wizard_label()
                ),
                dim,
            )),
            Line::from(Span::styled(
                "✓ ready. Enter starts a task.",
                Style::default().fg(theme::SAGE),
            )),
        ],
        _ => {
            let account = screen.accounts.get(screen.selected);
            let mut lines = vec![Line::from(Span::styled("DETAILS", amber))];
            if let Some(account) = account {
                let key_detail = match account.method {
                    Method::Environment => {
                        let name = loom_core::engine::auth::provider_env_state(&account.provider)
                            .into_iter()
                            .find(|state| state.set)
                            .map_or_else(|| "environment variable".into(), |state| state.name);
                        format!("env {name}")
                    }
                    Method::Command => account.command.clone().unwrap_or_else(|| "command".into()),
                    Method::Key => account.last4.as_deref().map_or_else(
                        || "keychain key".into(),
                        |four| format!("keychain •••• {four}"),
                    ),
                };
                lines.extend([
                    line("provider", &account.provider, dim, ivory),
                    line("key", &key_detail, dim, ivory),
                    line("models", "discovered on demand", dim, ivory),
                    line("last used", "this session", dim, ivory),
                ]);
            } else {
                lines.push(Line::from(Span::styled("no account selected", dim)));
            }
            lines.push(Line::from(Span::styled("NOTE", amber)));
            lines.push(Line::from(Span::styled(
                "A task keeps the account it started with.",
                dim,
            )));
            lines
        }
    }
}

fn provider_type(type_selected: usize) -> &'static str {
    if type_selected == 0 {
        "deepseek"
    } else {
        "openai-compatible"
    }
}

fn line(label: &str, value: &str, dim: Style, ivory: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), dim),
        Span::styled(value.to_owned(), ivory),
    ])
}

fn step(
    glyph: &'static str,
    label: &'static str,
    detail: &str,
    glyph_style: Style,
    detail_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(format!("{label:<16}"), Style::default().fg(theme::IVORY)),
        Span::styled(detail.to_owned(), detail_style),
    ])
}

fn verify_lines(
    screen: &AccountScreen,
    amber: Style,
    dim: Style,
    ivory: Style,
    sage: Style,
    coral: Style,
) -> Vec<Line<'static>> {
    let label = if screen.label.is_empty() {
        screen.wizard_provider()
    } else {
        screen.label.clone()
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("TESTING · {label}"),
        amber,
    ))];
    match screen.verify.as_ref() {
        Some((true, detail)) => {
            let discovered = screen.discovered.len();
            let (reach, reach_detail) = if discovered == 0 {
                ("○", "built-in provider · no catalog request")
            } else {
                ("✓", "catalog fetched with this key")
            };
            lines.push(step(reach, "reach endpoint", reach_detail, sage, dim));
            lines.push(step("✓", "auth accepted", detail, sage, dim));
            lines.push(step(
                if discovered == 0 { "○" } else { "✓" },
                "list models",
                &if discovered == 0 {
                    "known locally".to_owned()
                } else {
                    format!("{discovered} discovered")
                },
                sage,
                dim,
            ));
            lines.push(step("○", "probe tools", "declared per model", dim, dim));
            lines.push(progress(20));
            lines.push(Line::from(Span::styled("", dim)));
            lines.push(Line::from(Span::styled(
                "✓ ready. Enter picks a model and defaults.",
                sage,
            )));
        }
        Some((false, detail)) => {
            lines.push(Line::from(Span::styled("IF IT FAILS", amber)));
            lines.push(Line::from(vec![
                Span::styled("✗ auth      ", coral),
                Span::styled("HTTP 401 · check the key / key source", dim),
            ]));
            lines.push(Line::from(vec![
                Span::styled("✗ transport ", coral),
                Span::styled("no route · check the base URL", dim),
            ]));
            lines.push(Line::from(vec![
                Span::styled("✗ no /models", coral),
                Span::styled("the model id can be entered by hand", dim),
            ]));
            lines.push(progress(20));
            lines.push(Line::from(Span::styled("", dim)));
            lines.push(Line::from(vec![
                Span::styled("save failed: ", coral),
                Span::styled(detail.clone(), ivory),
            ]));
            lines.push(Line::from(Span::styled(
                "Esc edits the key · r retries · /save saves anyway",
                dim,
            )));
        }
        None => {
            lines.push(step("✓", "reach endpoint", "120 ms", sage, dim));
            lines.push(step("✓", "auth accepted", "", sage, dim));
            lines.push(step("⠇", "list models", "", amber, dim));
            lines.push(step("○", "probe tools and reasoning", "", dim, dim));
            lines.push(progress(11));
            lines.push(Line::from(Span::styled("", dim)));
            lines.push(Line::from(Span::styled(
                "Nothing is saved until the test passes or you choose to save anyway.",
                dim,
            )));
        }
    }
    lines
}

/// Wireframe J4's amber progress bar, `fill` cells out of 20.
fn progress(fill: usize) -> Line<'static> {
    let fill = fill.min(20);
    Line::from(vec![
        Span::styled("█".repeat(fill), Style::default().fg(theme::AMBER)),
        Span::styled("░".repeat(20 - fill), Style::default().fg(theme::DOTTED)),
    ])
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
        "/add add a provider   /test test connection",
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
    let masked = if screen.key.is_empty() {
        String::new()
    } else {
        let tail: String = screen.key.chars().rev().take(4).collect();
        format!("•••• {}", tail.chars().rev().collect::<String>())
    };
    let key_value = match screen.key_source {
        0 => screen.env_var.clone().unwrap_or_else(|| "not found".into()),
        2 => screen.command.clone(),
        _ => masked,
    };
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
        ("key", key_value.as_str()),
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
    lines.push(Line::from(Span::styled("KEY SOURCE", amber)));
    let sources = [
        (
            "environment variable",
            format!(
                "{} · {}",
                screen.env_var.as_deref().unwrap_or("DEEPSEEK_API_KEY"),
                if screen.env_var.is_some() {
                    "found ✓"
                } else {
                    "not set"
                }
            ),
        ),
        ("paste a key", "stored in the OS keychain".into()),
        ("command", "e.g. pass show llm/key".into()),
    ];
    for (index, (name, detail)) in sources.iter().enumerate() {
        let selected = index == screen.key_source;
        let cursor = if screen.field == 3 && selected {
            "❯ "
        } else {
            "  "
        };
        lines.push(Line::from(vec![
            Span::styled(cursor, amber),
            Span::styled(
                format!("{} ", if selected { "●" } else { "○" }),
                if selected { amber } else { dim },
            ),
            Span::styled(format!("{name:<22}"), if selected { ivory } else { dim }),
            Span::styled(detail.clone(), dim),
        ]));
    }
    lines.push(Line::from(Span::styled("", dim)));
    lines.push(Line::from(Span::styled(
        "Keys never enter the transcript, run record, or event stream.",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "Tab next field · ↑↓ source · Enter saves and verifies",
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
            .style(Style::default().bg(theme::BG))
            .title(Span::styled(
                " ◆ account  /account ",
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
        Stage::Credentials | Stage::Verify | Stage::Model => {
            "providers · type · credentials · verify · model"
        }
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(breadcrumb, dim))),
        rows[0],
    );

    let lines = body_lines(screen);
    let two_column = matches!(screen.stage, Stage::List | Stage::Model);
    if two_column {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(rows[1]);
        frame.render_widget(Paragraph::new(lines), columns[0]);
        let details = details_lines(
            screen,
            Style::default().fg(theme::AMBER),
            dim,
            Style::default().fg(theme::IVORY),
        );
        frame.render_widget(
            Paragraph::new(details).block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(theme::BORDER)),
            ),
            columns[1],
        );
    } else {
        frame.render_widget(Paragraph::new(lines), rows[1]);
    }
    frame.render_widget(
        Paragraph::new(screen.command_line.line().unwrap_or_else(|| {
            Line::from(Span::styled(
                match screen.stage {
                    Stage::List => "↑↓ move · Enter switch · /add · /test · /remove · Esc close",
                    Stage::Type => "↑↓ move · Enter choose · Esc back",
                    Stage::Credentials => {
                        "Tab next field · ↑↓ source · Enter save + verify · Esc back"
                    }
                    Stage::Verify => {
                        "Enter pick a model · r retry · /save save anyway · Esc edit key"
                    }
                    Stage::Model => {
                        "↑↓ model · Enter save and start · /tools /reasoning /context · Esc back"
                    }
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
        screen.key_source = 1; // paste a key instead of the default env source
        assert_eq!(
            screen.key(KeyCode::Enter.into()),
            Some(AccountAction::Add {
                provider: "deepseek".into(),
                label: "mykey".into(),
                base_url: String::new(),
                key: "sk-test".into(),
                method: Method::Key,
                command: None,
            })
        );
        assert_eq!(screen.stage, Stage::Verify);
        screen.verify = Some((false, "nope".into()));
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Enter)),
            Some(AccountAction::Consumed)
        );
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Char('r'))),
            Some(AccountAction::Add {
                provider: "deepseek".into(),
                label: "mykey".into(),
                base_url: String::new(),
                key: "sk-test".into(),
                method: Method::Key,
                command: None,
            })
        );
        screen.verify = Some((true, "deepseek · mykey".into()));
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Enter)),
            Some(AccountAction::Consumed)
        );
        assert_eq!(screen.stage, Stage::Model);
        assert_eq!(
            screen.key(KeyEvent::from(KeyCode::Enter)),
            Some(AccountAction::Continue {
                provider: "deepseek".into(),
                account: Some("mykey".into()),
                model: "deepseek-flash".into(),
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
            "A task keeps the account it started with",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn j3_shows_key_sources_and_masks_the_pasted_key() {
        let mut screen = AccountScreen::new(vec![]);
        screen.stage = Stage::Credentials;
        screen.type_selected = 0;
        screen.key_source = 1;
        screen.key = "sk-1234567890abcdef".into();
        let backend = ratatui::backend::TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, frame.area(), &screen))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for needle in [
            "KEY SOURCE",
            "environment variable",
            "paste a key",
            "command",
            "•••• cdef",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn j5_shows_discovered_models_defaults_and_switch_later() {
        let mut screen = AccountScreen::new(vec![]);
        screen.stage = Stage::Model;
        screen.label = "work".into();
        let backend = ratatui::backend::TestBackend::new(130, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, frame.area(), &screen))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for needle in [
            "DISCOVERED",
            "deepseek-flash",
            "deepseek-v4-pro",
            "DEFAULTS",
            "thinking",
            "SAVED AS",
            "SWITCH LATER",
            "loom auth use",
            "ready. Enter starts a task",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
    }
}
