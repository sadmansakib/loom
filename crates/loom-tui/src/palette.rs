//! The `/` command palette: filtered command list anchored above the
//! composer, driven purely by the prompt text.

/// One palette row: the command as typed and a one-line blurb.
pub struct Command {
    pub name: &'static str,
    pub blurb: &'static str,
}

/// Commands the TUI can run today. Grows as screens land.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "/model",
        blurb: "pick a model and capabilities",
    },
    Command {
        name: "/effort",
        blurb: "set reasoning effort",
    },
    Command {
        name: "/compact",
        blurb: "compact the context",
    },
    Command {
        name: "/export",
        blurb: "export this task as NDJSON + manifest",
    },
    Command {
        name: "/help",
        blurb: "keys and help",
    },
];

/// The palette is open whenever the composer text starts with `/`.
pub(crate) fn is_open(input: &str) -> bool {
    input.starts_with('/')
}

/// Commands whose name or blurb contains the filter (case-insensitive).
pub(crate) fn matching(filter: &str) -> Vec<&'static Command> {
    let needle = filter.trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| {
            needle.is_empty()
                || command.name.to_lowercase().contains(&needle)
                || command.blurb.to_lowercase().contains(&needle)
        })
        .collect()
}

/// What a keypress means while the palette is open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    /// Run the selected command.
    Run,
    /// Close the palette without running.
    Close,
    /// The key was consumed (selection moved); keep typing.
    Consumed,
}

/// Route one keypress while the palette is open. `selected` is clamped
/// to the match count so a shrinking filter cannot point past the list.
pub(crate) fn key(
    key: crossterm::event::KeyEvent,
    selected: &mut usize,
    match_count: usize,
) -> Option<PaletteAction> {
    if key.code == crossterm::event::KeyCode::Esc {
        return Some(PaletteAction::Close);
    }
    if match_count == 0 {
        // Nothing to run or move; everything else types.
        return None;
    }
    *selected = (*selected).min(match_count - 1);
    if key.code == crossterm::event::KeyCode::Up {
        *selected = selected.saturating_sub(1);
        Some(PaletteAction::Consumed)
    } else if key.code == crossterm::event::KeyCode::Down {
        *selected = (*selected + 1).min(match_count - 1);
        Some(PaletteAction::Consumed)
    } else if key.code == crossterm::event::KeyCode::Enter {
        Some(PaletteAction::Run)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn filter_narrows_by_name_or_blurb() {
        let matches = matching("/mod");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "/model");
        assert!(matching("/export").iter().all(|c| c.name != "/help"));
        assert_eq!(matching("/").len(), COMMANDS.len());
    }

    #[test]
    fn keys_move_run_and_close() {
        let mut selected = 5; // past the end after a filter shrank
        let action = key(plain(KeyCode::Down), &mut selected, 3);
        assert_eq!(action, Some(PaletteAction::Consumed));
        assert_eq!(selected, 2);
        assert_eq!(
            key(plain(KeyCode::Up), &mut selected, 3),
            Some(PaletteAction::Consumed)
        );
        assert_eq!(selected, 1);
        assert_eq!(
            key(plain(KeyCode::Enter), &mut selected, 3),
            Some(PaletteAction::Run)
        );
        assert_eq!(
            key(plain(KeyCode::Esc), &mut selected, 3),
            Some(PaletteAction::Close)
        );
        // Typing falls through to the composer.
        assert_eq!(key(plain(KeyCode::Char('x')), &mut selected, 3), None);
    }
}
