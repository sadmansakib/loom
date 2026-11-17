//! Slash commands typed inside a modal screen (DESIGN.md "Commands").
//!
//! A modal that owns the keyboard still runs its actions as `/commands`;
//! this tracks the typed line and leaves ordinary keys to the screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme;

/// What a keypress meant to the modal's command line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandKey {
    /// Not a command key; the screen handles it normally.
    Ignore,
    /// The key edited the command line and was consumed.
    Consumed,
    /// Enter: run this command (leading slash and spaces trimmed).
    Run(String),
    /// Esc: the command line closed without running anything.
    Cancel,
}

#[derive(Debug, Default)]
pub(crate) struct CommandLine {
    text: String,
    active: bool,
}

impl CommandLine {
    #[cfg(test)]
    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// Routes a key. `/` starts the line only when it is closed, so keys
    /// the modal already owns keep working and a filter can still see `/`.
    #[allow(clippy::wildcard_enum_match_arm)] // only editing keys are special; the rest are ignored
    pub(crate) fn key(&mut self, key: KeyEvent) -> CommandKey {
        if !self.active {
            if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
                self.active = true;
                self.text.clear();
                return CommandKey::Consumed;
            }
            return CommandKey::Ignore;
        }
        match key.code {
            KeyCode::Esc => {
                self.close();
                CommandKey::Cancel
            }
            KeyCode::Enter => {
                let command = self.text.trim().to_owned();
                self.close();
                if command.is_empty() {
                    CommandKey::Cancel
                } else {
                    CommandKey::Run(command)
                }
            }
            KeyCode::Backspace => {
                self.text.pop();
                CommandKey::Consumed
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.text.push(character);
                CommandKey::Consumed
            }
            _ => CommandKey::Consumed,
        }
    }

    fn close(&mut self) {
        self.active = false;
        self.text.clear();
    }

    /// The `❯ /word` line while a command is being typed.
    pub(crate) fn line(&self) -> Option<Line<'static>> {
        if !self.active {
            return None;
        }
        Some(Line::from(vec![
            Span::styled("❯ /", Style::default().fg(theme::AMBER)),
            Span::styled(self.text.clone(), Style::default().fg(theme::IVORY)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn slash_opens_and_enter_runs_without_the_slash() {
        let mut line = CommandLine::default();
        assert_eq!(line.key(plain(KeyCode::Char('e'))), CommandKey::Ignore);
        assert_eq!(
            line.key(plain(KeyCode::Char('/'))),
            CommandKey::Consumed,
            "slash opens the command line instead of reaching the screen"
        );
        for character in "export".chars() {
            line.key(plain(KeyCode::Char(character)));
        }
        assert_eq!(
            line.key(plain(KeyCode::Enter)),
            CommandKey::Run("export".into())
        );
        assert!(!line.active());
    }

    #[test]
    fn esc_cancels_and_restores_normal_keys() {
        let mut line = CommandLine::default();
        line.key(plain(KeyCode::Char('/')));
        line.key(plain(KeyCode::Char('x')));
        assert_eq!(line.key(plain(KeyCode::Esc)), CommandKey::Cancel);
        assert_eq!(line.line(), None);
        assert_eq!(line.key(plain(KeyCode::Char('x'))), CommandKey::Ignore);
    }
}
