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
        name: "/context",
        blurb: "inspect what the next request will contain",
    },
    Command {
        name: "/tools",
        blurb: "built-in tools and MCP servers",
    },
    Command {
        name: "/account",
        blurb: "providers, keys, and the active account",
    },
    Command {
        name: "/compare",
        blurb: "recent runs side by side",
    },
    Command {
        name: "/sessions",
        blurb: "browse, resume, export, or delete past sessions",
    },
    Command {
        name: "/new",
        blurb: "start a fresh session",
    },
    Command {
        name: "/clear",
        blurb: "clear the current session",
    },
    Command {
        name: "/clear-history",
        blurb: "delete all saved sessions",
    },
    Command {
        name: "/compact",
        blurb: "compact the context",
    },
    Command {
        name: "/retry",
        blurb: "re-run the failed task",
    },
    Command {
        name: "/copy",
        blurb: "copy the final answer",
    },
    Command {
        name: "/export",
        blurb: "export this task as NDJSON + manifest",
    },
    Command {
        name: "/reasoning",
        blurb: "show or hide reasoning",
    },
    Command {
        name: "/sidebar",
        blurb: "focus the sidebar",
    },
    Command {
        name: "/profile",
        blurb: "toggle full or read-only tools",
    },
    Command {
        name: "/help",
        blurb: "keys and help",
    },
    Command {
        name: "/quit",
        blurb: "quit",
    },
];

/// What a slash command means to the app loop. The palette only produces
/// this value; the handlers live in the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Model,
    Effort,
    Account,
    Sessions,
    New,
    Clear,
    ClearHistory,
    Context,
    Tools,
    Copy,
    Export,
    Reasoning,
    Sidebar,
    Profile,
    Help,
    Quit,
    Compare,
    Compact,
    Retry,
}

/// Map a command name to its action. `None` is an unknown command.
pub(crate) fn action_for(name: &str) -> Option<Action> {
    Some(match name {
        "/model" => Action::Model,
        "/effort" => Action::Effort,
        "/account" => Action::Account,
        "/sessions" => Action::Sessions,
        "/new" => Action::New,
        "/clear" => Action::Clear,
        "/clear-history" => Action::ClearHistory,
        "/context" => Action::Context,
        "/tools" => Action::Tools,
        "/copy" => Action::Copy,
        "/export" => Action::Export,
        "/reasoning" => Action::Reasoning,
        "/sidebar" => Action::Sidebar,
        "/profile" => Action::Profile,
        "/help" => Action::Help,
        "/quit" => Action::Quit,
        "/compare" => Action::Compare,
        "/compact" => Action::Compact,
        "/retry" => Action::Retry,
        _ => return None,
    })
}

/// The palette is open whenever the composer text starts with `/`.
pub(crate) fn is_open(input: &str) -> bool {
    input.starts_with('/')
}

/// Commands whose name starts with the filter (case-insensitive). The
/// composer always types a leading `/`, so `/mo` narrows to `/model`.
pub(crate) fn matching(filter: &str) -> Vec<&'static Command> {
    let needle = filter.trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.name.trim_start_matches('/').starts_with(&needle))
        .collect()
}

/// What a keypress means while the palette is open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    /// Run the selected command.
    Run,
    /// Close the palette without running.
    Close,
    /// Fill the composer with the selected command name.
    Complete,
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
    // Shift+Enter still inserts a newline; modified keys are not palette keys.
    if !key.modifiers.is_empty() {
        return None;
    }
    if match_count == 0 {
        // An unmatched `/word` must never fall through to the composer and
        // be submitted to the model as a task.
        if key.code == crossterm::event::KeyCode::Enter {
            return Some(PaletteAction::Consumed);
        }
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
    } else if key.code == crossterm::event::KeyCode::Tab {
        Some(PaletteAction::Complete)
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
    fn every_listed_command_maps_to_an_action() {
        assert_eq!(action_for("/model"), Some(Action::Model));
        assert_eq!(action_for("/help"), Some(Action::Help));
        assert_eq!(action_for("/quit"), Some(Action::Quit));
        assert_eq!(action_for("/nope"), None);
        for command in COMMANDS {
            assert!(
                action_for(command.name).is_some(),
                "{} has no action",
                command.name
            );
        }
    }

    #[test]
    fn clear_is_selected_before_clear_history() {
        assert_eq!(matching("/clear")[0].name, "/clear");
        assert_eq!(matching("/clear-")[0].name, "/clear-history");
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
        // Tab completes the selection instead of toggling the sidebar.
        assert_eq!(
            key(plain(KeyCode::Tab), &mut selected, 3),
            Some(PaletteAction::Complete)
        );
        // Typing falls through to the composer.
        assert_eq!(key(plain(KeyCode::Char('x')), &mut selected, 3), None);
        // Shift+Enter keeps its newline meaning even with the list open.
        assert_eq!(
            key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                &mut selected,
                3
            ),
            None
        );
    }
}
