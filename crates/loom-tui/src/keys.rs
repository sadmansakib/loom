//! Keyboard and mouse routing for the TUI: maps key and wheel events to
//! prompt editing, scrolling, panel toggles, and task control.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use loom_core::TaskControl;

use crate::input::{PromptEditor, expand_file_mentions};
use crate::reducer::Reducer;

#[allow(clippy::too_many_arguments)]
pub(crate) fn update_thread_scroll(key: KeyEvent, scroll_back: &mut usize) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            *scroll_back = scroll_back.saturating_add(8);
            true
        }
        (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            *scroll_back = scroll_back.saturating_sub(8);
            true
        }
        (KeyCode::End, KeyModifiers::CONTROL) => {
            *scroll_back = 0;
            true
        }
        _ => false,
    }
}

pub(crate) fn update_sidebar_scroll(key: KeyEvent, scroll_back: &mut usize) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::PageUp, KeyModifiers::SHIFT) => {
            *scroll_back = scroll_back.saturating_add(3);
            true
        }
        (KeyCode::PageDown, KeyModifiers::SHIFT) => {
            *scroll_back = scroll_back.saturating_sub(3);
            true
        }
        (KeyCode::Home, KeyModifiers::SHIFT) => {
            *scroll_back = 0;
            true
        }
        _ => false,
    }
}

pub(crate) fn update_sidebar_mouse(mouse: MouseEvent, scroll_back: &mut usize) {
    match mouse.kind {
        MouseEventKind::ScrollUp => *scroll_back = scroll_back.saturating_add(3),
        MouseEventKind::ScrollDown => *scroll_back = scroll_back.saturating_sub(3),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {}
    }
}

pub(crate) fn update_thread_mouse(mouse: MouseEvent, scroll_back: &mut usize) {
    match mouse.kind {
        MouseEventKind::ScrollUp => *scroll_back = scroll_back.saturating_add(3),
        MouseEventKind::ScrollDown => *scroll_back = scroll_back.saturating_sub(3),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {}
    }
}

#[cfg(test)]
pub(crate) fn scroll_from_mouse(kind: MouseEventKind, scroll_back: &mut usize) {
    update_thread_mouse(
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
        scroll_back,
    );
}

pub(crate) fn handle_key(key: KeyEvent, context: &mut KeyContext<'_>) {
    let KeyContext {
        input,
        workspace,
        help,
        picker,
        sidebar,
        quit,
        pending_task,
        pending_display_prompt,
        control,
        reducer,
        redraw,
    } = context;
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if let Some(control) = control {
                control.cancel();
            } else {
                **quit = true;
            }
        }
        // Idle has no agent to cancel: Esc is reserved for the palette and
        // does nothing once the palette is closed. Quit is `/quit` or ^C.
        (KeyCode::Esc, _) => {
            if let Some(control) = control {
                control.cancel();
            }
        }
        (KeyCode::Char('l'), KeyModifiers::CONTROL) => **redraw = true,
        (KeyCode::Up, _) if control.is_none() && input.text.is_empty() => {
            if let Some(last) = reducer
                .task
                .display_prompts
                .last()
                .or(reducer.original_instruction.as_ref())
            {
                input.text.clone_from(last);
                input.cursor = input.text.len();
            }
        }
        (KeyCode::F(1), _) => **help = !**help,
        (KeyCode::F(2), _) if control.is_none() => {
            **picker = true;
        }
        (KeyCode::Tab, _) if input.has_file_mention() => input.complete_file(workspace),
        (KeyCode::Char('b'), KeyModifiers::CONTROL) | (KeyCode::Tab, _) => **sidebar = !**sidebar,
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => reducer.toggle_reasoning(),
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => reducer.toggle_selected_result(),
        (KeyCode::Enter, modifiers)
            if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            input.insert('\n');
        }
        (KeyCode::Enter, _) => {
            let message = std::mem::take(&mut input.text);
            input.cursor = 0;
            if !message.trim().is_empty() {
                let original_message = message;
                let (message, attachments) = expand_file_mentions(&original_message, workspace);
                let display_prompt = original_message.clone();
                if let Some(control) = control {
                    control.steer(message);
                    reducer.set_follow_up_queued(true);
                    reducer.push_display_prompt(display_prompt);
                    reducer.task.attachments.extend(attachments);
                } else {
                    **pending_display_prompt = Some(original_message);
                    reducer.set_attachments(attachments);
                    **pending_task = Some(message);
                    **picker = false;
                }
            }
        }
        (KeyCode::Backspace, _) => {
            input.backspace();
        }
        (KeyCode::Delete, _) => input.delete(),
        (KeyCode::Left, _) => input.left(),
        (KeyCode::Right, _) => input.right(),
        (KeyCode::Home, _) => input.line_home(),
        (KeyCode::End, _) => input.line_end(),
        (KeyCode::Char(c), modifiers)
            if !c.is_control()
                && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            input.insert(c);
        }
        _ => {}
    }
}

/// Routes one terminal event that arrived while the UI is not in the picker:
/// scroll keys first, then prompt/command keys, mouse by column, and paste.
/// `mouse_split_column` is read only for mouse events, matching the original
/// timing of the terminal size lookup.
pub(crate) fn route_event(
    event: crossterm::event::Event,
    mouse_split_column: impl FnOnce() -> std::io::Result<u16>,
    scroll: (&mut usize, &mut usize),
    context: &mut KeyContext<'_>,
) -> std::io::Result<()> {
    use crossterm::event::Event;
    let (thread_scroll_back, sidebar_scroll_back) = scroll;
    match event {
        Event::Key(key) => {
            let selecting = matches!(key.code, KeyCode::Up | KeyCode::Down)
                && context.control.is_some()
                && context.input.text.is_empty();
            if selecting {
                context.reducer.move_selection(key.code == KeyCode::Down);
                *thread_scroll_back = 0;
                return Ok(());
            }
            if !update_sidebar_scroll(key, sidebar_scroll_back)
                && !update_thread_scroll(key, thread_scroll_back)
            {
                handle_key(key, context);
            } else if key.code == KeyCode::End && key.modifiers == KeyModifiers::CONTROL {
                // Jump to latest also drops the selection, restoring follow mode.
                context.reducer.selected = None;
            }
        }
        Event::Mouse(mouse) => {
            if mouse.column >= mouse_split_column()? {
                update_sidebar_mouse(mouse, sidebar_scroll_back);
            } else {
                update_thread_mouse(mouse, thread_scroll_back);
            }
        }
        Event::Paste(text) => context.input.insert_paste(&text),
        Event::FocusGained | Event::FocusLost | Event::Resize(..) => {}
    }
    Ok(())
}

pub(crate) struct KeyContext<'a> {
    pub(crate) input: &'a mut PromptEditor,
    pub(crate) workspace: &'a Path,
    pub(crate) help: &'a mut bool,
    pub(crate) picker: &'a mut bool,
    pub(crate) sidebar: &'a mut bool,
    pub(crate) quit: &'a mut bool,
    pub(crate) pending_task: &'a mut Option<String>,
    pub(crate) pending_display_prompt: &'a mut Option<String>,
    pub(crate) control: Option<&'a TaskControl>,
    pub(crate) reducer: &'a mut Reducer,
    /// Set by ^L; the app loop clears and redraws the terminal.
    pub(crate) redraw: &'a mut bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::PromptEditor;
    use crate::reducer::Reducer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press_tab(input: &mut PromptEditor, sidebar: &mut bool) {
        let mut help = false;
        let mut picker = false;
        let mut quit = false;
        let mut pending_task = None;
        let mut pending_display_prompt = None;
        let mut reducer = Reducer::default();
        let mut redraw = false;
        let mut context = KeyContext {
            input,
            workspace: Path::new("."),
            help: &mut help,
            picker: &mut picker,
            sidebar,
            quit: &mut quit,
            pending_task: &mut pending_task,
            pending_display_prompt: &mut pending_display_prompt,
            control: None,
            reducer: &mut reducer,
            redraw: &mut redraw,
        };
        handle_key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut context,
        );
    }

    #[test]
    fn tab_toggles_sidebar_when_no_mention_is_typed() {
        let mut input = PromptEditor::default();
        let mut sidebar = false;
        press_tab(&mut input, &mut sidebar);
        assert!(sidebar);
        press_tab(&mut input, &mut sidebar);
        assert!(!sidebar);
    }

    #[test]
    fn tab_completes_mention_instead_of_toggling_sidebar() {
        let mut input = PromptEditor::default();
        input.insert('@');
        let mut sidebar = false;
        press_tab(&mut input, &mut sidebar);
        assert!(!sidebar);
    }

    #[test]
    fn idle_esc_does_not_quit_and_up_recalls_the_last_task() {
        let mut input = PromptEditor::default();
        let mut reducer = Reducer::default();
        reducer.begin_task("inspect the retry path");
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut pending_task = None;
        let mut pending_display_prompt = None;
        let mut redraw = false;
        let mut context = KeyContext {
            input: &mut input,
            workspace: Path::new("."),
            help: &mut help,
            picker: &mut picker,
            sidebar: &mut sidebar,
            quit: &mut quit,
            pending_task: &mut pending_task,
            pending_display_prompt: &mut pending_display_prompt,
            control: None,
            reducer: &mut reducer,
            redraw: &mut redraw,
        };
        {
            handle_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &mut context,
            );
            handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut context);
        }
        assert!(!quit, "idle Esc must leave the composer, not quit");
        assert_eq!(input.text, "inspect the retry path");
        assert_eq!(input.cursor, input.text.len());
    }

    #[test]
    fn enter_on_an_empty_composer_sends_nothing() {
        let mut input = PromptEditor::default();
        let mut reducer = Reducer::default();
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut pending_task = None;
        let mut pending_display_prompt = None;
        let mut redraw = false;
        let mut context = KeyContext {
            input: &mut input,
            workspace: Path::new("."),
            help: &mut help,
            picker: &mut picker,
            sidebar: &mut sidebar,
            quit: &mut quit,
            pending_task: &mut pending_task,
            pending_display_prompt: &mut pending_display_prompt,
            control: None,
            reducer: &mut reducer,
            redraw: &mut redraw,
        };
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut context,
        );
        assert!(pending_task.is_none());
        assert!(pending_display_prompt.is_none());
        assert!(!quit);
    }

    #[test]
    fn ctrl_l_requests_a_redraw_and_resize_keeps_the_buffer() {
        let mut input = PromptEditor::default();
        input.insert('a');
        input.insert('b');
        let mut reducer = Reducer::default();
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut pending_task = None;
        let mut pending_display_prompt = None;
        let mut redraw = false;
        let mut context = KeyContext {
            input: &mut input,
            workspace: Path::new("."),
            help: &mut help,
            picker: &mut picker,
            sidebar: &mut sidebar,
            quit: &mut quit,
            pending_task: &mut pending_task,
            pending_display_prompt: &mut pending_display_prompt,
            control: None,
            reducer: &mut reducer,
            redraw: &mut redraw,
        };
        {
            handle_key(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                &mut context,
            );
            let mut thread_scroll = 0;
            let mut sidebar_scroll = 0;
            route_event(
                crossterm::event::Event::Resize(70, 24),
                || Ok(0),
                (&mut thread_scroll, &mut sidebar_scroll),
                &mut context,
            )
            .unwrap();
        }
        assert!(redraw);
        assert_eq!(input.text, "ab");
        assert_eq!(input.cursor, 2);
    }
}
