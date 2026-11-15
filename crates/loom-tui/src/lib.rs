//! Loom's terminal UI. The frontend owns only presentation and task control;
//! engine, provider, tools, and session semantics come from `loom-core`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod account_screen;
mod approval;
mod compare_screen;
mod context_screen;
mod glyphs;
mod input;
mod keys;
mod modal;
mod palette;
mod picker;
mod recent;
mod reducer;
mod scene_views;
mod sessions_screen;
mod terminal;
mod theme;
mod tools_screen;
mod welcome;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use loom_core::kernel::agent::TurnBudget;
use loom_core::kernel::context::PruneConfig;
use loom_core::kernel::event::ChannelEventSink;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::EventStore;
use loom_core::tools::ToolBudgets;
use loom_core::{Engine, EngineConfig, EventSink, ReasoningEffort, TaskControl};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

#[cfg(test)]
use crossterm::event::KeyModifiers;
#[cfg(test)]
use crossterm::event::{KeyEvent, MouseEventKind};
use input::{PromptEditor, mentioned_file_attachments, workspace_files};
#[cfg(test)]
use input::{expand_file_mentions, matching_file_suggestions};
#[cfg(test)]
use keys::scroll_from_mouse;
use keys::{KeyContext, route_event};
#[cfg(test)]
use keys::{handle_key, update_sidebar_scroll, update_thread_scroll};
use picker::{PickerAction, PickerState, handle_picker_key};
#[cfg(test)]
use picker::{
    catalog_credential_provider, catalog_models_url, environment_credential, model_choice,
    navigate_picker,
};
pub use reducer::Reducer;
pub use scene_views::Scene;
#[cfg(test)]
use std::path::Path;
use terminal::{LiveFrame, draw_live, terminal_session, workspace_label};

const FRAME_TICK: Duration = Duration::from_millis(35);

/// Renders one of the six fixed reference scenes and restores the terminal.
///
/// # Errors
///
/// Returns an error if terminal setup, drawing, or restoration fails.
pub fn render_scene(scene: Scene) -> Result<()> {
    terminal_session(|terminal| {
        terminal.draw(|frame| scene_views::draw(frame, scene))?;
        Ok(())
    })
}

/// Open the interactive TUI using the compact settings surface shared by
/// `loom tui` and `loom run --tui`.
///
/// # Errors
///
/// Returns an error if configuration, terminal setup, or runtime initialization fails.
pub fn run_with_options(
    task: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    workspace: Option<PathBuf>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<()> {
    let config = default_config(provider, model, base_url, workspace, reasoning_effort)?;
    run_with_config(task, config)
}

/// Drive the TUI with a fully composed engine configuration. CLI callers
/// should use this to preserve all `loom run` flags.
///
/// # Errors
///
/// Returns an error if runtime initialization or terminal interaction fails.
pub fn run_with_config(task: Option<String>, config: EngineConfig) -> Result<()> {
    run_with_config_and_overrides(task, config, loom_core::EngineOverrides::default())
}

/// Drive the TUI with configuration and engine overrides composed by the CLI.
///
/// # Errors
///
/// Returns an error if runtime initialization or terminal interaction fails.
pub fn run_with_config_and_overrides(
    task: Option<String>,
    config: EngineConfig,
    overrides: loom_core::EngineOverrides,
) -> Result<()> {
    init_tui_logging();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create TUI async runtime")?;
    let task = task.filter(|task| !task.trim().is_empty());
    terminal_session(|terminal| {
        runtime.block_on(run_interactive(terminal, config, Some(overrides), task))
    })
}

fn default_config(
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    workspace: Option<PathBuf>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<EngineConfig> {
    let workspace = workspace
        .or_else(|| std::env::current_dir().ok())
        .context("resolve current workspace")?;
    Ok(EngineConfig {
        provider: provider.unwrap_or_default(),
        model: model.unwrap_or_default(),
        base_url,
        models_url: None,
        headers: Vec::new(),
        reasoning_effort: reasoning_effort.unwrap_or_default(),
        account: None,
        workspace,
        system_prompt: None,
        max_turns: 100,
        turn_budget: TurnBudget::default(),
        context_editing: None,
        db_path: loom_core::paths::db_path(),
        persist: true,
        catalog: None,
        scout: None,
        routing: None,
        stall_escalation_model: None,
        background_commands: true,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        // The `/tools` screen shows live per-server attach status and the
        // wireframe's MCP section, so the TUI turns MCP on. Attach is
        // trust-gated (`loom trust` pins `.loom/mcp.toml`) and fails
        // closed: an unpinned or broken server contributes no tools.
        mcp: true,
        lsp: false,
        budgets: ToolBudgets::default(),
        prune: Some(PruneConfig::default()),
        autofix: false,
    })
}

/// Runs the first-run flow. Returns `false` when the user quits.
fn run_welcome(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &EngineConfig,
) -> Result<bool> {
    let provider = if config.provider.is_empty() {
        loom_core::engine::auth::DEFAULT_PROVIDER
    } else {
        config.provider.as_str()
    };
    let mut flow = welcome::Welcome::new(provider);
    loop {
        terminal.draw(|frame| welcome::draw_welcome(frame, &flow))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(false),
            KeyCode::Enter => match flow.advance() {
                welcome::Action::Stay => {}
                welcome::Action::Finish => {
                    if flow.env_var.is_none() && !flow.key.trim().is_empty() {
                        welcome::save_pasted_key(&config.db_path, &config.provider, &flow.key)
                            .map_err(|error| anyhow::anyhow!(error))?;
                    }
                    loom_core::tools::trust::pin(&config.workspace)?;
                    return Ok(true);
                }
            },
            KeyCode::Backspace if flow.step == welcome::Step::Key => {
                flow.key.pop();
            }
            KeyCode::Char(ch) if flow.step == welcome::Step::Key => flow.key.push(ch),
            KeyCode::Backspace
            | KeyCode::Char(_)
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::F(_)
            | KeyCode::Null
            | KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_) => {}
        }
    }
}

#[allow(clippy::too_many_lines)] // terminal lifecycle and task event loop are intentionally serialized
async fn run_interactive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut config: EngineConfig,
    mut overrides: Option<loom_core::EngineOverrides>,
    mut pending_task: Option<String>,
) -> Result<()> {
    if !loom_core::tools::trust::is_pinned(&config.workspace) && !run_welcome(terminal, &config)? {
        return Ok(());
    }
    let mut reducer = Reducer::default();
    let mut input = PromptEditor::default();
    let file_index = workspace_files(&config.workspace);
    let recent = recent::load(&config.db_path);
    let mut thread_scroll_back = 0_usize;
    let mut sidebar_scroll_back = 0_usize;
    let mut pending_display_prompt = None;
    let mut help = false;
    let mut picker = config.model.is_empty();
    let mut sidebar = true;
    let mut should_quit = false;
    let mut last_elapsed = None;
    let mut picker_state = PickerState::configured(&config);
    let mut picker_loaded = false;
    let mut last_dropped = None;
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<approval::Request>();
    let mut pending_approval: Option<approval::Request> = None;
    let mut palette_selected = 0_usize;
    let mut notice: Option<String> = None;
    // Armed by `/compact`: the next task runs an eager handoff summary
    // before its first compile (the kernel's compaction latch).
    let mut compact_next = false;
    let mut sessions: Option<sessions_screen::SessionsScreen> = None;
    let mut context_open = false;
    let mut context_items: Vec<context_screen::ContextItem> = Vec::new();
    let mut context_selected = 0_usize;
    let mut tools: Option<tools_screen::ToolsScreen> = None;
    let mut compare: Option<compare_screen::CompareScreen> = None;
    let mut accounts: Option<account_screen::AccountScreen> = None;
    let mut event_log: Vec<loom_core::EventEnvelope> = Vec::new();
    if picker {
        picker_state = PickerState::discover(&config).await?;
        picker_state.apply(&mut config);
        picker_loaded = true;
    }
    let mut active: Option<(Engine, Session)> = None;

    if !picker {
        active =
            Some(open_engine(&config, overrides.take().unwrap_or_default(), &approval_tx).await?);
    }

    loop {
        if picker && !picker_loaded {
            picker_state = PickerState::discover(&config).await?;
            picker_loaded = true;
        }
        if picker {
            terminal.draw(|frame| {
                if help {
                    scene_views::draw(frame, Scene::Help);
                } else {
                    scene_views::draw_picker(frame, &picker_state);
                }
            })?;
            if should_quit {
                return Ok(());
            }
            if event::poll(FRAME_TICK)? {
                match event::read()? {
                    Event::Paste(text) if picker_state.searching => {
                        picker_state
                            .search_query
                            .extend(text.chars().filter(|ch| !ch.is_control()));
                        picker_state.refresh_filter_selection();
                    }
                    Event::Key(key) => match handle_picker_key(
                        &mut picker_state,
                        key.code,
                        key.modifiers,
                        active.is_some(),
                    ) {
                        PickerAction::None | PickerAction::LeaveSearch => {}
                        PickerAction::CloseToSession => picker = false,
                        PickerAction::Quit => return Ok(()),
                        PickerAction::ToggleHelp => help = !help,
                        PickerAction::Refresh => {
                            picker_state = PickerState::discover(&config).await?;
                            picker_loaded = true;
                        }
                        PickerAction::Select => {
                            picker_state.searching = false;
                            picker_state.apply(&mut config);
                            let previous_session = active.take().map(|(_, session)| session);
                            if let Some(catalog) = picker_state.catalogs.get(&config.provider) {
                                config.catalog = Some(catalog.clone());
                            }
                            let mut engine_overrides = overrides.take().unwrap_or_default();
                            if let Some(credential) = picker_state.credentials.get(&config.provider)
                            {
                                engine_overrides =
                                    engine_overrides.with_credential(credential.clone());
                            }
                            let (engine, session) =
                                open_engine(&config, engine_overrides, &approval_tx).await?;
                            active = Some((engine, previous_session.unwrap_or(session)));
                            picker = false;
                        }
                    },
                    Event::Paste(_)
                    | Event::Mouse(_)
                    | Event::FocusGained
                    | Event::FocusLost
                    | Event::Resize(..) => {}
                }
            }
            continue;
        }

        if let Some(task) = pending_task.take() {
            let Some((engine, session)) = active.as_mut() else {
                active = Some(
                    open_engine(&config, overrides.take().unwrap_or_default(), &approval_tx)
                        .await?,
                );
                continue;
            };
            let (sink, mut events) = ChannelEventSink::new();
            let task_control = TaskControl::new();
            if std::mem::take(&mut compact_next) {
                task_control.request_compaction();
            }
            reducer = Reducer::default();
            event_log.clear();
            let prompt = pending_display_prompt
                .take()
                .unwrap_or_else(|| task.clone());
            reducer.begin_task(prompt);
            let mentioned = mentioned_file_attachments(
                reducer
                    .task
                    .display_prompts
                    .first()
                    .map_or(&task, String::as_str),
                &config.workspace,
            );
            reducer.set_attachments(mentioned);
            reducer.restore_display_prompt();
            let started_at = std::time::Instant::now();
            let workspace = workspace_label(&config.workspace);
            let session_id = session.id.to_string();
            let effort = format!("{:?}", config.reasoning_effort).to_lowercase();
            let mut run = Box::pin(engine.run_task(session, task, &sink, &task_control));
            let result = loop {
                terminal.draw(|frame| {
                    draw_live(
                        frame,
                        &LiveFrame {
                            reducer: &reducer,
                            input: &input.text,
                            cursor: input.cursor,
                            help,
                            picker,
                            sidebar,
                            elapsed: Some(started_at.elapsed()),
                            dropped: Some(sink.dropped()),
                            thread_scroll_back,
                            sidebar_scroll_back,
                            pricing: picker_state.selected().pricing,
                            input_editor: &input,
                            file_index: &file_index,
                            recent: &recent,
                            approval: pending_approval.as_ref(),
                            notice: None,
                            palette_selected: 0,
                            sessions: None,
                            overlay: None,
                            tools: None,
                            compare: None,
                            accounts: None,
                            context_items: &[],
                            context_selected: 0,
                            config: &config,
                            session_id: Some(session_id.as_str()),
                            capabilities: picker_state.selected().capabilities,
                            workspace: &workspace,
                            effort: &effort,
                        },
                    );
                })?;
                tokio::select! {
                    event = events.recv() => {
                        if let Some(event) = event {
                            event_log.push(event.clone());
                            reducer.reduce(&event, epoch_millis());
                        }
                    }
                    request = approval_rx.recv(), if pending_approval.is_none() => {
                        if let Some(request) = request {
                            reducer.note_event(format!("ApprovalRequested {}", request.path));
                            pending_approval = Some(request);
                        }
                    }
                    result = &mut run => {
                        break result;
                    }
                    () = tokio::time::sleep(FRAME_TICK) => {
                        if event::poll(Duration::ZERO)? {
                            let event = event::read()?;
                            if let Event::Key(crossterm::event::KeyEvent { code: KeyCode::Char('e'), .. }) = &event
                                && let Some(request) = pending_approval.take()
                            {
                                let decision = match approval::edit_proposal(&request.content) {
                                    Some(edited) => loom_core::tools::WriteDecision::Edit(edited),
                                    None => loom_core::tools::WriteDecision::Decline,
                                };
                                reducer.note_event(format!("ApprovalDecided {} edited", request.path));
                                let _ = request.reply.send(decision);
                                continue;
                            }
                            if let Event::Key(crossterm::event::KeyEvent { code: KeyCode::Char(answer), .. }) = &event
                                && let Some(decision) = approval::decision_for_key(*answer)
                                && let Some(request) = pending_approval.take()
                            {
                                if *answer == 'a' {
                                    loom_core::tools::trust::allow_writes(&config.workspace)?;
                                }
                                reducer.note_event(format!(
                                    "ApprovalDecided {} {}",
                                    request.path,
                                    if decision == loom_core::tools::WriteDecision::Decline { "declined" } else { "allowed" }
                                ));
                                let _ = request.reply.send(decision);
                                continue;
                            }
                            route_event(
                                event,
                                || terminal.size().map(|size| size.width.saturating_sub(scene_views::SIDEBAR_WIDTH)),
                                (&mut thread_scroll_back, &mut sidebar_scroll_back),
                                &mut KeyContext {
                                    input: &mut input,
                                    workspace: &config.workspace,
                                    help: &mut help,
                                    picker: &mut picker,
                                    sidebar: &mut sidebar,
                                    quit: &mut should_quit,
                                    pending_task: &mut pending_task,
                                    pending_display_prompt: &mut pending_display_prompt,
                                    control: Some(&task_control),
                                    reducer: &mut reducer,
                                },
                            )?;
                        }
                    }
                }
                if should_quit {
                    task_control.cancel();
                }
            };
            drop(run);
            // A request left unanswered by an abandoned or retried call must
            // not outlive the run: decline it so the box cannot linger.
            if let Some(stale) = pending_approval.take() {
                let _ = stale.reply.send(loom_core::tools::WriteDecision::Decline);
            }
            while let Ok(event) = events.try_recv() {
                reducer.reduce(&event, epoch_millis());
            }
            match result {
                Ok(summary) => reducer.set_summary(summary),
                Err(error) => tracing::warn!(%error, "task finished with an error"),
            }
            last_elapsed = Some(started_at.elapsed());
            last_dropped = Some(sink.dropped());
            reducer.reconcile_session(session);
        }

        terminal.draw(|frame| {
            let session_label = active.as_ref().map(|(_, session)| session.id.to_string());
            draw_live(
                frame,
                &LiveFrame {
                    reducer: &reducer,
                    input: &input.text,
                    cursor: input.cursor,
                    help,
                    picker,
                    sidebar,
                    elapsed: last_elapsed,
                    dropped: last_dropped,
                    thread_scroll_back,
                    sidebar_scroll_back,
                    pricing: picker_state.selected().pricing,
                    input_editor: &input,
                    file_index: &file_index,
                    recent: &recent,
                    approval: pending_approval.as_ref(),
                    notice: notice.as_deref(),
                    palette_selected,
                    sessions: sessions.as_ref(),
                    overlay: context_open.then_some(crate::terminal::Overlay::Context),
                    tools: tools.as_ref(),
                    compare: compare.as_ref(),
                    accounts: accounts.as_ref(),
                    context_items: &context_items,
                    context_selected,
                    config: &config,
                    session_id: session_label.as_deref(),
                    capabilities: picker_state.selected().capabilities,
                    workspace: &workspace_label(&config.workspace),
                    effort: &format!("{:?}", config.reasoning_effort).to_lowercase(),
                },
            );
        })?;
        if should_quit {
            return Ok(());
        }
        if event::poll(FRAME_TICK)? {
            let event = event::read()?;
            let key_pressed = matches!(event, Event::Key(_));
            // The sessions screen owns every key while it is open.
            let sessions_action = match (&event, sessions.as_mut()) {
                (Event::Key(key), Some(screen)) => screen.key(*key),
                _ => None,
            };
            let handled_by_sessions = if let Some(action) = sessions_action {
                use sessions_screen::SessionsAction;
                match action {
                    SessionsAction::Consumed => {
                        if let Some(screen) = sessions.as_mut() {
                            screen.refresh_preview(&config.db_path);
                        }
                        true
                    }
                    SessionsAction::Close => {
                        sessions = None;
                        true
                    }
                    SessionsAction::Resume(raw) => {
                        sessions = None;
                        notice = Some(
                            resume_session(
                                &config,
                                overrides.take().unwrap_or_default(),
                                &approval_tx,
                                &mut active,
                                &mut reducer,
                                &raw,
                            )
                            .await,
                        );
                        true
                    }
                    SessionsAction::Export(raw) => {
                        notice = Some(export_session_events(&config, &raw));
                        if let Some(screen) = sessions.as_mut() {
                            screen.refresh_preview(&config.db_path);
                        }
                        true
                    }
                    SessionsAction::Delete(raw) => {
                        notice = Some(delete_session(&config.db_path, &raw));
                        sessions = Some(sessions_screen::SessionsScreen::load(&config.db_path));
                        if let Some(screen) = sessions.as_mut() {
                            screen.refresh_preview(&config.db_path);
                        }
                        true
                    }
                    SessionsAction::NewSession => {
                        active = None;
                        sessions = None;
                        notice = Some("started a new session".into());
                        true
                    }
                }
            } else {
                false
            };
            // The context inspector owns every key while it is open.
            let handled_by_context = if context_open && let Event::Key(key) = &event {
                use crossterm::event::KeyCode;
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => context_open = false,
                    (KeyCode::Up, _) => {
                        context_selected = context_selected.saturating_sub(1);
                    }
                    (KeyCode::Down, _) => {
                        context_selected =
                            (context_selected + 1).min(context_items.len().saturating_sub(1));
                    }
                    (KeyCode::Char('c'), modifiers) if modifiers.is_empty() => {
                        compact_next = true;
                        notice = Some("context will be compacted before the next task".into());
                    }
                    (KeyCode::Char(status @ ('p' | 'x')), modifiers) if modifiers.is_empty() => {
                        if let Some(item) = context_items.get(context_selected) {
                            let call_id = item.call_id.clone();
                            if let Some((engine, _)) = active.as_ref() {
                                if status == 'p' {
                                    engine.pin_context_call(&call_id);
                                    notice = Some(format!("pinned {call_id} to the prefix"));
                                } else {
                                    engine.drop_context_call(&call_id);
                                    notice =
                                        Some(format!("dropped {call_id} from the next request"));
                                }
                                let ops = engine.context_ops().unwrap_or_default();
                                for item in &mut context_items {
                                    item.pinned = ops.pinned.contains(&item.call_id);
                                    item.dropped = ops.dropped.contains(&item.call_id);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                true
            } else {
                false
            };
            // The tools screen owns its keys while it is open.
            let tools_action = tools.as_mut().and_then(|screen| {
                let Event::Key(key) = &event else {
                    return None;
                };
                screen.key(*key)
            });
            let handled_by_tools = match tools_action {
                Some(tools_screen::ToolsAction::Consumed) => true,
                Some(tools_screen::ToolsAction::Close) => {
                    tools = None;
                    true
                }
                Some(tools_screen::ToolsAction::ToggleWrites) => {
                    match tools.as_mut() {
                        Some(screen) if screen.writes_allowed => {
                            notice =
                                Some("writes are already always allowed for this folder".into());
                        }
                        Some(screen) => {
                            if let Err(error) =
                                loom_core::tools::trust::allow_writes(&config.workspace)
                            {
                                notice = Some(format!("could not save the writes choice: {error}"));
                            } else {
                                screen.writes_allowed = true;
                                notice =
                                    Some("writes are now always allowed for this folder".into());
                            }
                        }
                        None => {}
                    }
                    true
                }
                Some(tools_screen::ToolsAction::Retry) => {
                    let live = active
                        .as_ref()
                        .map_or(&[][..], |(engine, _)| engine.mcp_servers());
                    tools = Some(tools_screen::ToolsScreen::load(&config.workspace, live));
                    notice = Some("MCP servers attach when a session opens".into());
                    true
                }
                None => false,
            };
            // The account screen owns its keys while it is open.
            let account_action = accounts.as_mut().and_then(|screen| {
                let Event::Key(key) = &event else {
                    return None;
                };
                screen.key(*key)
            });
            let handled_by_accounts = match account_action {
                Some(account_screen::AccountAction::Consumed) => true,
                Some(account_screen::AccountAction::Close) => {
                    accounts = None;
                    true
                }
                Some(account_screen::AccountAction::Activate { provider, label }) => {
                    match loom_core::engine::auth::Auth::open(&config.db_path) {
                        Ok(auth) => match auth.activate(&provider, &label) {
                            Ok(_) => {
                                notice = Some(format!(
                                    "switched {provider} to {label} (next task uses it)"
                                ));
                            }
                            Err(error) => notice = Some(format!("switch failed: {error}")),
                        },
                        Err(error) => notice = Some(format!("switch failed: {error}")),
                    }
                    if let Ok(auth) = loom_core::engine::auth::Auth::open(&config.db_path)
                        && let Some(screen) = accounts.as_mut()
                    {
                        screen.accounts = auth.list(None).unwrap_or_default();
                    }
                    true
                }
                Some(account_screen::AccountAction::Remove { provider, label }) => {
                    match loom_core::engine::auth::Auth::open(&config.db_path) {
                        Ok(auth) => match auth.remove(&provider, &label) {
                            Ok(_) => notice = Some(format!("removed {label} from {provider}")),
                            Err(error) => notice = Some(format!("remove failed: {error}")),
                        },
                        Err(error) => notice = Some(format!("remove failed: {error}")),
                    }
                    if let Ok(auth) = loom_core::engine::auth::Auth::open(&config.db_path)
                        && let Some(screen) = accounts.as_mut()
                    {
                        screen.accounts = auth.list(None).unwrap_or_default();
                        screen.selected = 0;
                    }
                    true
                }
                Some(account_screen::AccountAction::Test { provider }) => {
                    accounts = None;
                    config.provider = provider;
                    picker = true;
                    true
                }
                Some(account_screen::AccountAction::Add {
                    provider,
                    label,
                    base_url,
                    key,
                }) => {
                    let spec = loom_core::engine::auth::LoginSpec {
                        provider: provider.clone(),
                        label: Some(label.clone()),
                        method: loom_core::engine::auth::Method::Key,
                        command: None,
                        secret: Some(key),
                    };
                    match loom_core::engine::auth::Auth::open(&config.db_path) {
                        Ok(auth) => match auth.login(spec) {
                            Ok(_) => {
                                let registry_error = if base_url.is_empty() {
                                    None
                                } else {
                                    let mut registry =
                                        loom_core::engine::registry::ProviderRegistry::load(
                                            &config.db_path,
                                        );
                                    registry.upsert(
                                        &provider,
                                        loom_core::engine::registry::RegistryEntry {
                                            base_url,
                                            models_url: None,
                                            headers: std::collections::BTreeMap::new(),
                                        },
                                    );
                                    registry.save(&config.db_path).err()
                                };
                                accounts = None;
                                config.provider.clone_from(&provider);
                                config.model.clear();
                                picker = true;
                                notice = Some(match registry_error {
                                    Some(error) => format!(
                                        "saved {provider} · {label}, but the registry write failed: {error}"
                                    ),
                                    None => {
                                        format!("saved {provider} · {label}; picking a model")
                                    }
                                });
                            }
                            Err(error) => notice = Some(format!("save failed: {error}")),
                        },
                        Err(error) => notice = Some(format!("save failed: {error}")),
                    }
                    true
                }
                None => false,
            };
            // The compare screen owns its keys while it is open.
            let compare_action = compare.as_mut().and_then(|screen| {
                let Event::Key(key) = &event else {
                    return None;
                };
                screen.key(*key)
            });
            let handled_by_compare = match compare_action {
                Some(compare_screen::CompareAction::Consumed) => true,
                Some(compare_screen::CompareAction::Close) => {
                    compare = None;
                    true
                }
                Some(compare_screen::CompareAction::Open(session)) => {
                    compare = None;
                    notice = Some(
                        resume_session(
                            &config,
                            overrides.take().unwrap_or_default(),
                            &approval_tx,
                            &mut active,
                            &mut reducer,
                            &session,
                        )
                        .await,
                    );
                    true
                }
                Some(compare_screen::CompareAction::ExportNdjson) => {
                    if let Some(screen) = compare.as_ref() {
                        notice = Some(export_runs_ndjson(&config, screen));
                    }
                    true
                }
                Some(compare_screen::CompareAction::ExportCsv) => {
                    if let Some(screen) = compare.as_ref() {
                        notice = Some(export_runs_csv(&config, screen));
                    }
                    true
                }
                None => false,
            };
            // The palette owns ↑/↓/Enter/Esc while the composer holds a
            // `/` command; every other key falls through and keeps typing.
            let handled_by_palette = if let (Event::Key(key), true) =
                (&event, palette::is_open(&input.text))
            {
                let matches = palette::matching(&input.text);
                match palette::key(*key, &mut palette_selected, matches.len()) {
                    Some(palette::PaletteAction::Run) => {
                        if let Some(command) = matches.get(palette_selected) {
                            match command.name {
                                "/model" | "/effort" => picker = true,
                                "/help" => help = true,
                                "/sessions" => {
                                    sessions = Some(sessions_screen::SessionsScreen::load(
                                        &config.db_path,
                                    ));
                                    if let Some(screen) = sessions.as_mut() {
                                        screen.refresh_preview(&config.db_path);
                                    }
                                }
                                "/context" => {
                                    context_open = true;
                                    context_selected = 0;
                                    context_items = active.as_ref().map_or_else(
                                        Vec::new,
                                        |(engine, session)| {
                                            let ops = engine.context_ops().unwrap_or_default();
                                            context_screen::items(session, &ops, 6)
                                        },
                                    );
                                }
                                "/tools" => {
                                    let live = active
                                        .as_ref()
                                        .map_or(&[][..], |(engine, _)| engine.mcp_servers());
                                    tools = Some(tools_screen::ToolsScreen::load(
                                        &config.workspace,
                                        live,
                                    ));
                                }
                                "/account" => {
                                    let list = loom_core::engine::auth::Auth::open(&config.db_path)
                                        .and_then(|auth| auth.list(None))
                                        .unwrap_or_default();
                                    accounts = Some(account_screen::AccountScreen::new(list));
                                }
                                "/compare" => {
                                    compare =
                                        Some(compare_screen::CompareScreen::load(&config.db_path));
                                }
                                "/compact" => {
                                    compact_next = true;
                                    notice = Some(
                                        "context will be compacted before the next task".into(),
                                    );
                                }
                                "/export" => {
                                    notice = Some(export_task(
                                        &config.workspace.join(".loom").join("exports"),
                                        &event_log,
                                        active.as_ref().map(|(_, s)| s),
                                        &config,
                                        reducer.summary.as_ref(),
                                    ));
                                }
                                other => notice = Some(format!("{other} is not wired yet")),
                            }
                        }
                        input.text.clear();
                        input.cursor = 0;
                        palette_selected = 0;
                        true
                    }
                    Some(palette::PaletteAction::Close) => {
                        input.text.clear();
                        input.cursor = 0;
                        palette_selected = 0;
                        true
                    }
                    Some(palette::PaletteAction::Consumed) => true,
                    None => false,
                }
            } else {
                false
            };
            if !handled_by_palette
                && !handled_by_sessions
                && !handled_by_context
                && !handled_by_tools
                && !handled_by_compare
                && !handled_by_accounts
            {
                // A plain key press dismisses the footer notice; keys the
                // palette or sessions screen consumed (like Esc closing a
                // modal) leave it visible so the user can read the result.
                notice = None;
                route_event(
                    event,
                    || {
                        terminal
                            .size()
                            .map(|size| size.width.saturating_sub(scene_views::SIDEBAR_WIDTH))
                    },
                    (&mut thread_scroll_back, &mut sidebar_scroll_back),
                    &mut KeyContext {
                        input: &mut input,
                        workspace: &config.workspace,
                        help: &mut help,
                        picker: &mut picker,
                        sidebar: &mut sidebar,
                        quit: &mut should_quit,
                        pending_task: &mut pending_task,
                        pending_display_prompt: &mut pending_display_prompt,
                        control: None,
                        reducer: &mut reducer,
                    },
                )?;
            }
            if key_pressed && picker {
                picker_loaded = false;
            }
        }
        if let Some(task) = pending_task.take() {
            pending_task = Some(task);
            picker = false;
        }
    }
}

/// Replays the buffered run events through a [`TraceWriter`] so `/export`
/// produces the same NDJSON + manifest layout as `loom run --trace`.
fn export_task(
    base: &std::path::Path,
    log: &[loom_core::EventEnvelope],
    session: Option<&Session>,
    config: &EngineConfig,
    summary: Option<&loom_core::TaskSummary>,
) -> String {
    let Some(session) = session else {
        return "nothing to export: no active session".into();
    };
    if log.is_empty() {
        return "nothing to export: no events recorded yet, run a task first".into();
    }
    let mut writer = loom_core::engine::trace::TraceWriter::new(base.to_path_buf());
    for envelope in log {
        writer.record(envelope);
    }
    let meta = loom_core::engine::trace::TraceMeta {
        provider: &config.provider,
        model: &config.model,
        reasoning_effort: config.reasoning_effort,
        max_turns: config.max_turns,
        persisted: true,
    };
    let outcome = if summary.is_some() {
        loom_core::engine::trace::Outcome::Completed
    } else {
        loom_core::engine::trace::Outcome::Ended {
            status: "failed",
            end_reason: "task ended without a summary".into(),
        }
    };
    match writer.finish(session, meta, outcome, summary) {
        Ok(Some(dir)) => format!("exported to {}", dir.display()),
        Ok(None) => "nothing to export: no events were recorded".into(),
        Err(error) => format!("export failed: {error}"),
    }
}

/// Writes one session's recorded events as NDJSON, the same layout
/// `TraceWriter` produces, for the sessions screen's `e` key.
fn export_session_events(config: &EngineConfig, raw: &str) -> String {
    let Ok(id) = raw.parse::<loom_core::SessionId>() else {
        return "export failed: bad session id".into();
    };
    if !config.db_path.exists() {
        return "export failed: no store found".into();
    }
    let Ok(store) = loom_core::store::SqliteStore::open(&config.db_path) else {
        return "export failed: cannot open the store".into();
    };
    let events = store.load_events(&id).unwrap_or_default();
    if events.is_empty() {
        return "nothing to export: no events for that session".into();
    }
    let dir = config.workspace.join(".loom").join("exports");
    let path = dir.join(format!("{raw}.ndjson"));
    let mut body = String::new();
    for envelope in &events {
        match serde_json::to_string(envelope) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(error) => return format!("export failed: {error}"),
        }
    }
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body)) {
        Ok(()) => format!("exported to {}", path.display()),
        Err(error) => format!("export failed: {error}"),
    }
}

/// Writes the compare screen's visible rows as NDJSON.
fn export_runs_ndjson(config: &EngineConfig, screen: &compare_screen::CompareScreen) -> String {
    let body = compare_screen::ndjson(screen);
    if body.is_empty() {
        return "nothing to export: no runs match".into();
    }
    let dir = config.workspace.join(".loom").join("exports");
    let path = dir.join("runs.ndjson");
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body)) {
        Ok(()) => format!("exported to {}", path.display()),
        Err(error) => format!("export failed: {error}"),
    }
}

/// Writes the compare screen's visible rows as CSV.
fn export_runs_csv(config: &EngineConfig, screen: &compare_screen::CompareScreen) -> String {
    let body = compare_screen::csv(screen);
    let dir = config.workspace.join(".loom").join("exports");
    let path = dir.join("runs.csv");
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, body)) {
        Ok(()) => format!("exported to {}", path.display()),
        Err(error) => format!("export failed: {error}"),
    }
}

/// Deletes a session and its children from the store, then reloads.
fn delete_session(db_path: &std::path::Path, raw: &str) -> String {
    let Ok(id) = raw.parse::<loom_core::SessionId>() else {
        return "delete failed: bad session id".into();
    };
    if !db_path.exists() {
        return "delete failed: no store found".into();
    }
    let Ok(store) = loom_core::store::SqliteStore::open(db_path) else {
        return "delete failed: cannot open the store".into();
    };
    match store.delete_session(&id) {
        Ok(()) => format!("deleted {raw}"),
        Err(error) => format!("delete failed: {error}"),
    }
}

/// Swaps the live session for a recorded one so the next task continues it.
async fn resume_session(
    config: &EngineConfig,
    overrides: loom_core::EngineOverrides,
    requests: &tokio::sync::mpsc::UnboundedSender<approval::Request>,
    active: &mut Option<(Engine, Session)>,
    reducer: &mut reducer::Reducer,
    raw: &str,
) -> String {
    let Ok(id) = raw.parse::<loom_core::SessionId>() else {
        return "resume failed: bad session id".into();
    };
    if active.is_none() {
        match open_engine(config, overrides, requests).await {
            Ok(pair) => *active = Some(pair),
            Err(error) => return format!("resume failed: {error}"),
        }
    }
    let Some((engine, session_slot)) = active.as_mut() else {
        return "resume failed: no engine open".into();
    };
    match engine.load_session(&id) {
        Ok(Some(session)) => {
            *reducer = crate::reducer::Reducer::default();
            reducer.reconcile_session(&session);
            *session_slot = session;
            format!("resumed session {id}")
        }
        Ok(None) => format!("resume failed: unknown session {id}"),
        Err(error) => format!("resume failed: {error}"),
    }
}

async fn open_engine(
    config: &EngineConfig,
    overrides: loom_core::EngineOverrides,
    requests: &tokio::sync::mpsc::UnboundedSender<approval::Request>,
) -> Result<(Engine, Session)> {
    let overrides =
        overrides.with_write_approval(approval::hook(config.workspace.clone(), requests.clone()));
    let mut config = config.clone();
    if config.provider.is_empty() {
        loom_core::engine::auth::DEFAULT_PROVIDER.clone_into(&mut config.provider);
    }
    let engine = Engine::open_async_with(config, overrides)
        .await
        .context("open Loom engine")?;
    let session = engine.new_session();
    Ok((engine, session))
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn init_tui_logging() {
    use std::fs::{self, OpenOptions};
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    if let Some(path) = state_home.map(|path| path.join("loom"))
        && fs::create_dir_all(&path).is_ok()
        && let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.join("tui.log"))
    {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(file)
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_model_capability_resolver_does_not_offer_unknown_efforts() {
        let caps = loom_core::providers::model_capabilities("deepseek-flash").unwrap();
        assert!(
            caps.supported_efforts
                .contains(&loom_core::kernel::model::ReasoningEffort::High)
        );
        assert!(caps.can_disable_thinking);
    }

    #[test]
    fn picker_cycles_only_catalog_reported_efforts() {
        let mut config = default_config(None, None, None, None, None).unwrap();
        config.provider = "catalog-provider".to_owned();
        config.model = "catalog-model".to_owned();
        let mut picker = PickerState::configured(&config);
        assert_eq!(
            picker.selected().efforts,
            [] as [loom_core::ReasoningEffort; 0]
        );
        picker.next_effort();
        assert_eq!(
            picker.selected().efforts,
            [] as [loom_core::ReasoningEffort; 0]
        );
    }

    fn picker_with_models() -> PickerState {
        let choices = [
            ("anthropic", "claude-sonnet"),
            ("deepseek", "deepseek-chat"),
            ("openrouter", "deepseek/deepseek-chat"),
        ]
        .into_iter()
        .map(|(provider, model)| model_choice(provider.into(), model.into(), None, None))
        .collect();
        PickerState {
            choices,
            selected: 0,
            search_query: String::new(),
            searching: false,
            credentials: std::collections::HashMap::new(),
            catalogs: std::collections::HashMap::new(),
            effort: ReasoningEffort::default(),
            notice: None,
        }
    }

    #[test]
    fn picker_search_filters_provider_and_model_case_insensitively() {
        let mut picker = picker_with_models();
        picker.set_query("DEEPSEEK".into());
        assert_eq!(picker.filtered_indices(), vec![1, 2]);
        assert_eq!(picker.selected().provider, "deepseek");
        picker.move_model(1);
        assert_eq!(picker.selected().provider, "openrouter");
    }

    #[test]
    fn picker_search_results_remain_navigable_while_searching() {
        let mut picker = picker_with_models();
        picker.searching = true;
        picker.set_query("deepseek".into());

        assert!(navigate_picker(&mut picker, KeyCode::Down));
        assert_eq!(picker.selected().provider, "openrouter");
        assert!(navigate_picker(&mut picker, KeyCode::Up));
        assert_eq!(picker.selected().provider, "deepseek");
    }

    #[test]
    fn picker_navigation_shortcuts_do_not_edit_search_query() {
        let mut picker = picker_with_models();
        picker.searching = true;
        picker.set_query("deepseek".into());

        assert!(!navigate_picker(&mut picker, KeyCode::Char('j')));
        assert!(!navigate_picker(&mut picker, KeyCode::Char('k')));
        assert_eq!(picker.search_query, "deepseek");
        assert_eq!(picker.selected().provider, "deepseek");
    }

    #[test]
    fn custom_cache_uses_derived_models_url_when_registry_omits_one() {
        let base_url = "https://gateway.example/v1";
        let entry = loom_core::engine::registry::RegistryEntry {
            base_url: base_url.to_owned(),
            models_url: None,
            headers: std::collections::BTreeMap::default(),
        };
        let derived = catalog_models_url(&entry);
        assert_eq!(derived, "https://gateway.example/v1/models");
    }

    #[test]
    fn picker_discovery_selects_only_configured_or_unambiguous_custom_credential() {
        let mut registry = loom_core::engine::registry::ProviderRegistry::default();
        registry.upsert(
            "bkash",
            loom_core::engine::registry::RegistryEntry {
                base_url: "https://gateway.example/v1".to_owned(),
                models_url: None,
                headers: std::collections::BTreeMap::default(),
            },
        );
        assert_eq!(catalog_credential_provider("", &registry), Some("bkash"));
        assert_eq!(catalog_credential_provider("deepseek", &registry), None);
        assert_eq!(
            catalog_credential_provider("bkash", &registry),
            Some("bkash")
        );

        registry.upsert(
            "other-gateway",
            loom_core::engine::registry::RegistryEntry {
                base_url: "https://other.example/v1".to_owned(),
                models_url: None,
                headers: std::collections::BTreeMap::default(),
            },
        );
        assert_eq!(catalog_credential_provider("", &registry), None);
    }

    #[test]
    fn picker_search_with_no_matches_is_safe_and_cannot_apply() {
        let mut picker = picker_with_models();
        picker.set_query("not-a-model".into());
        assert_eq!(picker.filtered_indices(), [] as [usize; 0]);
        picker.move_model(1);
        let mut config = default_config(None, None, None, None, None).unwrap();
        picker.apply(&mut config);
        assert_eq!(config.provider, "");
        assert_eq!(config.model, "");
    }

    #[test]
    fn environment_credential_ignores_empty_values_and_does_not_resolve_accounts() {
        // The helper is deliberately environment-only; account/keychain
        // resolution belongs to engine opening after model selection.
        assert!(environment_credential("provider-with-no-env-credential").is_none());
    }

    #[test]
    fn default_config_leaves_model_selection_to_discovery() {
        let config = default_config(None, None, None, None, None).unwrap();
        assert_eq!(config.provider, "");
        assert_eq!(config.model, "");
        assert!(config.mcp, "the /tools screen reports live MCP status");
        assert!(!config.lsp);
        assert!(!config.autofix);
    }

    #[test]
    fn key_input_queues_follow_up_without_spawning_threads() {
        let mut text = PromptEditor {
            text: "fix the test".to_owned(),
            cursor: 12,
        };
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut next = None;
        let mut display_prompt = None;
        let mut reducer = Reducer::default();
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                input: &mut text,
                workspace: Path::new("."),
                help: &mut help,
                picker: &mut picker,
                sidebar: &mut sidebar,
                quit: &mut quit,
                pending_task: &mut next,
                pending_display_prompt: &mut display_prompt,
                control: None,
                reducer: &mut reducer,
            },
        );
        assert_eq!(next.as_deref(), Some("fix the test"));
    }

    #[test]
    fn ordinary_shortcut_characters_are_inserted_as_prompt_text() {
        let mut input = PromptEditor::default();
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut next = None;
        let mut display_prompt = None;
        let mut reducer = Reducer::default();
        handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            &mut KeyContext {
                input: &mut input,
                workspace: Path::new("."),
                help: &mut help,
                picker: &mut picker,
                sidebar: &mut sidebar,
                quit: &mut quit,
                pending_task: &mut next,
                pending_display_prompt: &mut display_prompt,
                control: None,
                reducer: &mut reducer,
            },
        );
        assert_eq!(input.text, "p");
        assert!(!quit);
        for character in ['m', '?', 'q'] {
            handle_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut KeyContext {
                    input: &mut input,
                    workspace: Path::new("."),
                    help: &mut help,
                    picker: &mut picker,
                    sidebar: &mut sidebar,
                    quit: &mut quit,
                    pending_task: &mut next,
                    pending_display_prompt: &mut display_prompt,
                    control: None,
                    reducer: &mut reducer,
                },
            );
        }
        assert_eq!(input.text, "pm?q");
        assert!(!help && !picker && !quit);
    }

    #[test]
    fn prompt_editor_edits_at_cursor_and_completes_workspace_mentions() {
        let root = std::env::temp_dir().join(format!("loom-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
        let mut editor = PromptEditor {
            text: "inspect @src/li".into(),
            cursor: 15,
        };
        editor.complete_file(&root);
        assert_eq!(editor.text, "inspect @src/lib.rs");
        editor.insert(' ');
        assert_eq!(editor.text, "inspect @src/lib.rs ");
        let expanded = expand_file_mentions(&editor.text, &root);
        assert!(expanded.0.contains("pub fn hello() {}"));
        assert!(expanded.0.contains("<attached file: src/lib.rs>"));
        assert_eq!(expanded.1.len(), 1);
        assert_eq!(expanded.1[0].path, "src/lib.rs");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn page_navigation_scrolls_thread_without_editing_prompt() {
        let mut scroll_back = 0;
        assert!(update_thread_scroll(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut scroll_back
        ));
        assert_eq!(scroll_back, 8);
        assert!(update_thread_scroll(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut scroll_back
        ));
        assert_eq!(scroll_back, 0);
        assert!(update_thread_scroll(
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            &mut scroll_back
        ));
    }

    #[test]
    fn shift_page_navigation_scrolls_sidebar_independently() {
        let mut sidebar_scroll = 0;
        assert!(update_sidebar_scroll(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
            &mut sidebar_scroll
        ));
        assert_eq!(sidebar_scroll, 3);
        assert!(update_sidebar_scroll(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT),
            &mut sidebar_scroll
        ));
        assert_eq!(sidebar_scroll, 0);
        assert!(update_sidebar_scroll(
            KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT),
            &mut sidebar_scroll
        ));
        assert!(!update_sidebar_scroll(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut sidebar_scroll
        ));
    }

    #[test]
    fn trackpad_mouse_wheel_scrolls_thread() {
        let mut scroll_back = 0;
        scroll_from_mouse(MouseEventKind::ScrollUp, &mut scroll_back);
        assert_eq!(scroll_back, 3);
        scroll_from_mouse(MouseEventKind::ScrollDown, &mut scroll_back);
        assert_eq!(scroll_back, 0);
    }

    #[test]
    fn file_suggestions_filter_as_the_mention_is_typed() {
        let files = vec![
            "README.md".to_owned(),
            "src/lib.rs".to_owned(),
            "src/main.rs".to_owned(),
        ];
        assert_eq!(matching_file_suggestions(&files, ""), files);
        assert_eq!(
            matching_file_suggestions(&files, "src/m"),
            vec!["src/main.rs"]
        );
        assert_eq!(
            matching_file_suggestions(&files, "missing"),
            [] as [std::string::String; 0]
        );
    }

    #[test]
    fn shifted_and_alt_enter_insert_a_newline() {
        let mut input = PromptEditor {
            text: "first".into(),
            cursor: 5,
        };
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut next = None;
        let mut display_prompt = None;
        let mut reducer = Reducer::default();
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            handle_key(
                KeyEvent::new(KeyCode::Enter, modifiers),
                &mut KeyContext {
                    input: &mut input,
                    workspace: Path::new("."),
                    help: &mut help,
                    picker: &mut picker,
                    sidebar: &mut sidebar,
                    quit: &mut quit,
                    pending_task: &mut next,
                    pending_display_prompt: &mut display_prompt,
                    control: None,
                    reducer: &mut reducer,
                },
            );
        }
        assert_eq!(input.text, "first\n\n");
        assert_eq!(input.cursor, input.text.len());
        assert!(next.is_none());
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text_without_submitting() {
        let mut input = PromptEditor {
            text: "beforeafter".into(),
            cursor: "before".len(),
        };

        input.insert_paste("one\r\ntwo\rthree\u{1b}four");

        assert_eq!(input.text, "beforeone\ntwo\nthreefourafter");
        assert_eq!(input.cursor, "beforeone\ntwo\nthreefour".len());
    }
}
