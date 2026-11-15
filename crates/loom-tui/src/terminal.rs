//! Terminal lifecycle and frame drawing: raw-mode setup and restoration,
//! and the live frame composed from the reducer, picker, and input state.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event;
use loom_core::EngineConfig;
use loom_core::kernel::model::ModelCapabilities;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::input::{PromptEditor, matching_file_suggestions};
use crate::picker::{ModelPricing, PickerState};
use crate::recent::RecentSession;
use crate::reducer::Reducer;
use crate::{Scene, scene_views};

/// Everything the live frame needs, grouped so `draw_live` takes one argument
/// instead of a long positional list.
/// Full-screen modal scenes drawn instead of the live view.
#[derive(Clone, Copy)]
pub(crate) enum Overlay {
    Context,
}

pub(crate) struct LiveFrame<'a> {
    pub(crate) reducer: &'a Reducer,
    pub(crate) input: &'a str,
    pub(crate) cursor: usize,
    pub(crate) help: bool,
    pub(crate) picker: bool,
    pub(crate) sidebar: bool,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) dropped: Option<u64>,
    pub(crate) thread_scroll_back: usize,
    pub(crate) sidebar_scroll_back: usize,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) input_editor: &'a PromptEditor,
    pub(crate) file_index: &'a [String],
    pub(crate) recent: &'a [RecentSession],
    /// Write awaiting the operator, shown as the amber approval prompt.
    pub(crate) approval: Option<&'a crate::approval::Request>,
    /// One-line status message (`/export` result, `/compact` note).
    pub(crate) notice: Option<&'a str>,
    /// Highlighted row while the `/` command palette is open.
    pub(crate) palette_selected: usize,
    /// The `/sessions` screen, drawn over the live view while open.
    pub(crate) sessions: Option<&'a crate::sessions_screen::SessionsScreen>,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) tools: Option<&'a crate::tools_screen::ToolsScreen>,
    pub(crate) compare: Option<&'a crate::compare_screen::CompareScreen>,
    pub(crate) config: &'a EngineConfig,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) capabilities: Option<ModelCapabilities>,
    pub(crate) workspace: &'a str,
    pub(crate) effort: &'a str,
}

pub(crate) fn draw_live(frame: &mut ratatui::Frame<'_>, view: &LiveFrame<'_>) {
    let LiveFrame {
        reducer,
        input,
        cursor,
        help,
        picker,
        sidebar,
        elapsed,
        dropped,
        thread_scroll_back,
        sidebar_scroll_back,
        pricing,
        input_editor,
        file_index,
        recent,
        approval,
        notice,
        palette_selected,
        sessions,
        overlay,
        tools,
        compare,
        config,
        session_id,
        capabilities,
        workspace,
        effort,
    } = *view;
    if help {
        scene_views::draw(frame, Scene::Help);
    } else if let Some(screen) = sessions {
        crate::sessions_screen::draw(frame, frame.area(), screen);
    } else if let Some(Overlay::Context) = overlay {
        crate::context_screen::draw(frame, frame.area(), reducer);
    } else if let Some(screen) = tools {
        crate::tools_screen::draw(frame, frame.area(), screen);
    } else if let Some(screen) = compare {
        crate::compare_screen::draw(frame, frame.area(), screen, pricing);
    } else if picker {
        scene_views::draw_picker(frame, &PickerState::empty());
    } else {
        let suggestions = input_editor
            .mention_query()
            .map_or_default(|query| matching_file_suggestions(file_index, query));
        scene_views::draw_live_scrolled(
            frame,
            reducer,
            input,
            cursor,
            scene_views::LiveViewOptions {
                sidebar_visible: sidebar,
                elapsed,
                dropped,
                thread_scroll_back,
                sidebar_scroll_back,
                file_suggestions: &suggestions,
                recent,
                approval,
                notice,
                palette_selected,
                pricing,
                workspace: Some(workspace),
                session_id,
                model: Some(&config.model),
                effort: Some(effort),
                thinking: capabilities.map(
                    |capabilities| {
                        if capabilities.reasoning { "on" } else { "off" }
                    },
                ),
                turn_limit: Some(config.max_turns),
                scene: None,
            },
        );
    }
}

pub(crate) fn workspace_label(workspace: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.as_deref()
        .and_then(|home| workspace.strip_prefix(home).ok())
        .map_or_else(
            || workspace.display().to_string(),
            |relative| format!("~/{}", relative.display()),
        )
}

pub(crate) fn terminal_session<T>(
    f: impl FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<T>,
) -> Result<T> {
    let mut guard = TerminalGuard::default();
    crossterm::terminal::enable_raw_mode().context("enable terminal raw mode")?;
    guard.raw_mode = true;
    crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)
        .context("enter alternate screen")?;
    guard.alternate_screen = true;
    crossterm::execute!(io::stdout(), event::EnableMouseCapture).context("enable mouse capture")?;
    guard.mouse_capture = true;
    crossterm::execute!(io::stdout(), event::EnableBracketedPaste)
        .context("enable bracketed paste")?;
    guard.bracketed_paste = true;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize terminal")?;
    let result = f(&mut terminal);
    let restore = guard.restore(terminal.backend_mut());
    match (result, restore) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // independent terminal modes must be restored after partial setup
pub(crate) struct TerminalGuard {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
}

impl TerminalGuard {
    pub(crate) fn restore(&mut self, output: &mut impl io::Write) -> Result<()> {
        if self.alternate_screen {
            crossterm::execute!(output, crossterm::terminal::LeaveAlternateScreen)
                .context("restore terminal screen")?;
            self.alternate_screen = false;
        }
        if self.mouse_capture {
            crossterm::execute!(output, event::DisableMouseCapture)
                .context("disable mouse capture")?;
            self.mouse_capture = false;
        }
        if self.bracketed_paste {
            crossterm::execute!(output, event::DisableBracketedPaste)
                .context("disable bracketed paste")?;
            self.bracketed_paste = false;
        }
        if self.raw_mode {
            crossterm::terminal::disable_raw_mode().context("disable terminal raw mode")?;
            self.raw_mode = false;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.alternate_screen {
            let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        }
        if self.mouse_capture {
            let _ = crossterm::execute!(io::stdout(), event::DisableMouseCapture);
        }
        if self.bracketed_paste {
            let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste);
        }
        if self.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
