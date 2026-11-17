//! Loom's terminal UI. The frontend owns only presentation and task control;
//! engine, provider, tools, and session semantics come from `loom-core`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod account_screen;
mod approval;
mod command_line;
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
mod wireframe_scenes;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use loom_core::kernel::agent::TurnBudget;
use loom_core::kernel::context::PruneConfig;
use loom_core::kernel::event::ChannelEventSink;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::{EventStore, SessionStore};
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
use terminal::{LiveFrame, draw_live, terminal_session, workspace_label};

const FRAME_TICK: Duration = Duration::from_millis(35);
/// DESIGN.md "Compact notice": one slot, latest wins, 8 s once the status
/// bar is uncovered.
const NOTICE_TTL: Duration = Duration::from_secs(8);

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
    let use_prefs = provider.is_none() && model.is_none();
    let effort_explicit = reasoning_effort.is_some();
    let mut config = default_config(provider, model, base_url, workspace, reasoning_effort)?;
    if use_prefs {
        let prefs = load_prefs();
        config.provider = prefs.provider.unwrap_or(config.provider);
        config.model = prefs.model.unwrap_or(config.model);
        config.account = prefs.account;
        if !effort_explicit && let Some(effort) = prefs.effort {
            config.reasoning_effort = effort;
        }
    }
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

#[derive(Default)]
struct TuiPrefs {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    account: Option<String>,
}

fn prefs_path() -> PathBuf {
    loom_core::paths::config_dir().join("tui.json")
}

fn parse_prefs(text: &str) -> TuiPrefs {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return TuiPrefs::default();
    };
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    TuiPrefs {
        provider: string("provider"),
        model: string("model"),
        effort: string("effort").as_deref().and_then(|effort| match effort {
            "none" => Some(ReasoningEffort::None),
            "low" => Some(ReasoningEffort::Low),
            "high" => Some(ReasoningEffort::High),
            "max" => Some(ReasoningEffort::Max),
            _ => None,
        }),
        account: string("account"),
    }
}

fn load_prefs() -> TuiPrefs {
    std::fs::read_to_string(prefs_path())
        .map_or_else(|_| TuiPrefs::default(), |text| parse_prefs(&text))
}

/// Remembers the picker selection so the next `loom tui` skips it.
fn save_prefs(config: &EngineConfig) {
    if config.model.is_empty() {
        return;
    }
    let path = prefs_path();
    let mut value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert("provider".into(), serde_json::json!(config.provider));
    object.insert("model".into(), serde_json::json!(config.model));
    object.insert(
        "effort".into(),
        serde_json::json!(config.reasoning_effort.as_str()),
    );
    if let Some(account) = &config.account {
        object.insert("account".into(), serde_json::json!(account));
    } else {
        object.remove("account");
    }
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(%error, "create TUI config directory");
        return;
    }
    match serde_json::to_vec_pretty(&value) {
        Ok(text) => {
            if let Err(error) = std::fs::write(&path, text) {
                tracing::warn!(%error, "write TUI preferences");
            }
        }
        Err(error) => tracing::warn!(%error, "serialize TUI preferences"),
    }
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
        terminal.draw(|frame| {
            welcome::draw_welcome(frame, &flow, &workspace_label(&config.workspace));
        })?;
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
    // The header shows the folder identity; `loom run` passes "." by default.
    if let Ok(workspace) = config.workspace.canonicalize() {
        config.workspace = workspace;
    }
    if !loom_core::tools::trust::is_pinned(&config.workspace) && !run_welcome(terminal, &config)? {
        return Ok(());
    }
    let mut reducer = Reducer::default();
    let mut input = PromptEditor::default();
    let file_index = workspace_files(&config.workspace);
    let mut recent = recent::load(&config.db_path);
    let mut thread_scroll_back = 0_usize;
    let mut sidebar_scroll_back = 0_usize;
    let mut pending_display_prompt = None;
    let mut help = false;
    let mut picker = config.model.is_empty();
    // Wide terminals own a permanent sidebar; medium ones open it with Tab.
    let mut sidebar = matches!(crossterm::terminal::size(), Ok((width, _)) if width >= 120);
    let mut should_quit = false;
    let mut last_elapsed = None;
    let mut picker_state = PickerState::configured(&config);
    let mut picker_loaded = false;
    let mut last_dropped = None;
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<approval::Request>();
    // `a` at the approval prompt allows writes until this TUI exits.
    let session_allow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pending_approval: Option<approval::Request> = None;
    let mut palette_selected = 0_usize;
    let mut redraw = false;
    let mut notice: Option<String> = None;
    // (last seen text, when the status bar first showed it uncovered)
    let mut notice_clock: Option<(String, Option<std::time::Instant>)> = None;
    // Armed by `/compact`: the next task runs an eager handoff summary
    // before its first compile (the kernel's compaction latch).
    let mut compact_next = false;
    let mut sessions: Option<sessions_screen::SessionsScreen> = None;
    let mut context_open = false;
    let mut context_items: Vec<context_screen::ContextItem> = Vec::new();
    let mut context_selected = 0_usize;
    let mut context_command = command_line::CommandLine::default();
    let mut tools: Option<tools_screen::ToolsScreen> = None;
    let mut compare: Option<compare_screen::CompareScreen> = None;
    let mut accounts: Option<account_screen::AccountScreen> = None;
    let mut event_log: Vec<loom_core::EventEnvelope> = Vec::new();
    // Catalog discovery walks every provider sequentially with 30 s network
    // timeouts. Never block the event loop on it before the first draw: that
    // leaves the alternate screen blank and looks like a frozen app (most
    // obvious on Ghostty, which swaps in the empty alt screen immediately).
    // Run it as a task and let the picker keep drawing/reading keys.
    let mut discovery: Option<tokio::task::JoinHandle<Result<PickerState>>> = None;
    if picker {
        let discover_config = config.clone();
        discovery = Some(tokio::spawn(async move {
            PickerState::discover(&discover_config).await
        }));
        picker_state.notice = Some("discovering models…".into());
    }
    let mut active: Option<(Engine, Session)> = None;

    if !picker {
        // Same rule for `loom run --tui`: show a frame before engine open.
        terminal.draw(|frame| {
            scene_views::draw_live(
                frame,
                &Reducer::default(),
                "",
                0,
                scene_views::LiveViewOptions {
                    notice: Some("starting task…"),
                    workspace: Some(&workspace_label(&config.workspace)),
                    model: Some(&config.model),
                    ..scene_views::LiveViewOptions::default()
                },
            );
        })?;
        active = Some(
            open_engine(
                &config,
                overrides.take().unwrap_or_default(),
                &approval_tx,
                &session_allow,
            )
            .await?,
        );
    }

    loop {
        // Opening the picker later (for example with `/model`) starts from the
        // configured choice. Discover the full catalog on first open just as
        // the launch-time picker does.
        if picker && !picker_loaded && discovery.is_none() {
            let discover_config = config.clone();
            discovery = Some(tokio::spawn(async move {
                PickerState::discover(&discover_config).await
            }));
            picker_state.notice = Some("discovering models…".into());
        }
        if picker
            && !picker_loaded
            && discovery
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(task) = discovery.take()
        {
            let discovered = task.await.context("join model discovery")??;
            picker_state = discovered;
            picker_state.apply(&mut config);
            picker_loaded = true;
        }
        if picker {
            // Yield so the discovery task can progress between frames; without
            // this the current-thread runtime would starve it entirely.
            tokio::task::yield_now().await;
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
                            let discover_config = config.clone();
                            discovery = Some(tokio::spawn(async move {
                                PickerState::discover(&discover_config).await
                            }));
                            picker_state = PickerState::empty();
                            picker_state.notice = Some("discovering models…".into());
                            picker_loaded = false;
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
                            let (engine, session) = open_engine(
                                &config,
                                engine_overrides,
                                &approval_tx,
                                &session_allow,
                            )
                            .await?;
                            active = Some((engine, previous_session.unwrap_or(session)));
                            picker = false;
                            save_prefs(&config);
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
            if active.is_none() {
                active = Some(
                    open_engine(
                        &config,
                        overrides.take().unwrap_or_default(),
                        &approval_tx,
                        &session_allow,
                    )
                    .await?,
                );
            }
            let Some((engine, session)) = active.as_mut() else {
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
            let mut last_ctrl_c: Option<std::time::Instant> = None;
            let result = loop {
                if should_quit {
                    break None;
                }
                tick_notice(&mut notice, &mut notice_clock, false);
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
                            notice: notice.as_deref(),
                            palette_selected: 0,
                            sessions: None,
                            overlay: None,
                            tools: None,
                            compare: None,
                            accounts: None,
                            context_items: &[],
                            context_selected: 0,
                            context_command: None,
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
                            if let loom_core::EventKind::ContextHandoff { turn, chars } = &event.kind
                            {
                                notice = Some(format!(
                                    "✓ compacted turn {turn} into a handoff summary · {chars} chars"
                                ));
                            }
                            event_log.push(event.clone());
                            let timestamp =
                                event.timestamp_millis.unwrap_or_else(epoch_millis);
                            reducer.reduce(&event, timestamp);
                        }
                    }
                    request = approval_rx.recv(), if pending_approval.is_none() => {
                        if let Some(request) = request {
                            reducer.note_event(format!("ApprovalRequested {}", request.path));
                            pending_approval = Some(request);
                        }
                    }
                    result = &mut run => {
                        break Some(result);
                    }
                    () = tokio::time::sleep(FRAME_TICK) => {
                        reducer.tick(epoch_millis());
                        if event::poll(Duration::ZERO)? {
                            let event = event::read()?;
                            // Esc and ^C cancel through the token. A second
                            // ^C within two seconds leaves the TUI at once.
                            if let Event::Key(crossterm::event::KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: crossterm::event::KeyModifiers::CONTROL,
                                ..
                            }) = &event
                            {
                                task_control.cancel();
                                let now = std::time::Instant::now();
                                if last_ctrl_c
                                    .is_some_and(|last| now.duration_since(last) <= Duration::from_secs(2))
                                {
                                    should_quit = true;
                                } else {
                                    last_ctrl_c = Some(now);
                                    notice = Some("cancelling… · ^C again to force quit".into());
                                }
                                continue;
                            }
                            // While a task runs, Enter on a slash command must
                            // run it instead of queueing the command as a
                            // follow-up for the model.
                            if let Event::Key(crossterm::event::KeyEvent {
                                code: KeyCode::Enter,
                                ..
                            }) = &event
                                && input.text.starts_with('/')
                            {
                                match input.text.trim() {
                                    "/help" => help = true,
                                    "/cancel" => {
                                        task_control.cancel();
                                        notice = Some("cancelling…".into());
                                    }
                                    "/reasoning" => reducer.toggle_reasoning(),
                                    "/sidebar" => sidebar = !sidebar,
                                    "/retry" => {
                                        if reducer.is_retrying() {
                                            task_control.retry_now();
                                            notice = Some("retrying now…".into());
                                        } else {
                                            notice =
                                                Some("nothing is waiting to retry · Esc cancels".into());
                                        }
                                    }
                                    "/quit" => should_quit = true,
                                    other => {
                                        notice = Some(format!(
                                            "{other} is not available while a task runs · Esc cancels"
                                        ));
                                    }
                                }
                                input.text.clear();
                                input.cursor = 0;
                                continue;
                            }
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
                                    session_allow.store(
                                        true,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
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
                                    redraw: &mut redraw,
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
            if result.is_none() {
                return Ok(());
            }
            // A request left unanswered by an abandoned or retried call must
            // not outlive the run: decline it so the box cannot linger.
            if let Some(stale) = pending_approval.take() {
                let _ = stale.reply.send(loom_core::tools::WriteDecision::Decline);
            }
            while let Ok(event) = events.try_recv() {
                let timestamp = event.timestamp_millis.unwrap_or_else(epoch_millis);
                reducer.reduce(&event, timestamp);
            }
            match result {
                Some(Ok(summary)) => reducer.set_summary(summary),
                Some(Err(error)) => tracing::warn!(%error, "task finished with an error"),
                None => unreachable!("returned above"),
            }
            last_elapsed = Some(started_at.elapsed());
            last_dropped = Some(sink.dropped());
            reducer.reconcile_session(session);
        }

        let covered = help
            || sessions.is_some()
            || context_open
            || tools.is_some()
            || compare.is_some()
            || accounts.is_some();
        tick_notice(&mut notice, &mut notice_clock, covered);
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
                    context_command: context_command.line(),
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
                                &session_allow,
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
                        notice = Some(match delete_session(&config.db_path, &raw) {
                            Ok(()) => format!("deleted {raw}"),
                            Err(error) => error,
                        });
                        let mut screen = sessions_screen::SessionsScreen::load(&config.db_path);
                        screen.workspace = workspace_label(&config.workspace);
                        sessions = Some(screen);
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
            // The context inspector owns every key while it is open; its
            // actions are typed as `/commands` like every other screen.
            let handled_by_context = if context_open && let Event::Key(key) = &event {
                use crossterm::event::KeyCode;
                match context_command.key(*key) {
                    command_line::CommandKey::Consumed | command_line::CommandKey::Cancel => true,
                    command_line::CommandKey::Run(command) => {
                        let selected_call = context_items
                            .get(context_selected)
                            .map(|item| item.call_id.clone());
                        match command.split_whitespace().collect::<Vec<_>>().as_slice() {
                            ["compact"] => {
                                compact_next = true;
                                notice =
                                    Some("context will be compacted before the next task".into());
                            }
                            ["pin"] => {
                                if let (Some(call_id), Some((engine, _))) =
                                    (selected_call, active.as_ref())
                                {
                                    engine.pin_context_call(&call_id);
                                    notice = Some(format!("pinned {call_id} to the prefix"));
                                    let ops = engine.context_ops().unwrap_or_default();
                                    for item in &mut context_items {
                                        item.pinned = ops.pinned.contains(&item.call_id);
                                        item.dropped = ops.dropped.contains(&item.call_id);
                                    }
                                }
                            }
                            ["drop"] => {
                                if let (Some(call_id), Some((engine, _))) =
                                    (selected_call, active.as_ref())
                                {
                                    engine.drop_context_call(&call_id);
                                    notice =
                                        Some(format!("dropped {call_id} from the next request"));
                                    let ops = engine.context_ops().unwrap_or_default();
                                    for item in &mut context_items {
                                        item.pinned = ops.pinned.contains(&item.call_id);
                                        item.dropped = ops.dropped.contains(&item.call_id);
                                    }
                                }
                            }
                            ["close"] => context_open = false,
                            _ => {}
                        }
                        true
                    }
                    command_line::CommandKey::Ignore => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Esc, _) => context_open = false,
                            (KeyCode::Up, _) => {
                                context_selected = context_selected.saturating_sub(1);
                            }
                            (KeyCode::Down, _) => {
                                context_selected = (context_selected + 1)
                                    .min(context_items.len().saturating_sub(1));
                            }
                            _ => {}
                        }
                        true
                    }
                }
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
                    tools = Some(
                        tools_screen::ToolsScreen::load(&config.workspace, live)
                            .with_calls(&event_log),
                    );
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
                    method,
                    command,
                }) => {
                    // Environment credentials are never stored (ADR-009): the
                    // key already exists in the process, so only verify.
                    let saved = if method == loom_core::engine::auth::Method::Environment {
                        Ok(())
                    } else {
                        let spec = loom_core::engine::auth::LoginSpec {
                            provider: provider.clone(),
                            label: Some(label.clone()),
                            method,
                            command,
                            secret: (method == loom_core::engine::auth::Method::Key).then_some(key),
                        };
                        loom_core::engine::auth::Auth::open(&config.db_path)
                            .and_then(|auth| auth.login(spec))
                            .map(|_| ())
                    };
                    match saved {
                        Ok(()) => {
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
                                        base_url: base_url.clone(),
                                        models_url: None,
                                        headers: std::collections::BTreeMap::new(),
                                    },
                                );
                                registry.save(&config.db_path).err()
                            };
                            // J4 asks the endpoint for its catalog for real:
                            // this is the same request the first task will make.
                            let mut probe = config.clone();
                            probe.provider.clone_from(&provider);
                            probe.account = (method
                                != loom_core::engine::auth::Method::Environment)
                                .then(|| label.clone());
                            probe.catalog = None;
                            probe.base_url = (!base_url.is_empty()).then(|| base_url.clone());
                            let verified = loom_core::Engine::resolve_catalog(probe).await;
                            if let Some(screen) = accounts.as_mut() {
                                match verified {
                                    Ok(probed) => {
                                        screen.discovered =
                                            discovered_models(probed.catalog.as_ref());
                                        screen.verify = Some((
                                            true,
                                            registry_error.map_or_else(
                                                || format!("{provider} · {label}"),
                                                |error| {
                                                    format!("saved, registry write failed: {error}")
                                                },
                                            ),
                                        ));
                                    }
                                    Err(error) => {
                                        screen.verify = Some((false, error.to_string()));
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            if let Some(screen) = accounts.as_mut() {
                                screen.verify = Some((false, error.to_string()));
                            }
                        }
                    }
                    true
                }
                Some(account_screen::AccountAction::Continue {
                    provider,
                    account,
                    model,
                }) => {
                    accounts = None;
                    config.provider.clone_from(&provider);
                    config.account = account;
                    if model.starts_with("declare") {
                        config.model.clear();
                        picker = true;
                    } else {
                        config.model.clone_from(&model);
                        picker = false;
                        notice = Some(format!(
                            "saved {provider} · {model} — describe a task to start"
                        ));
                    }
                    save_prefs(&config);
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
                            &session_allow,
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
            // Help is an overlay, not a screen: Esc closes it instead of
            // reaching the global Esc-quits binding.
            #[allow(clippy::wildcard_enum_match_arm)] // Event is non_exhaustive: only keys matter
            // Help is modal: only Esc/F1 leave it; anything else typed behind
            // it (including Enter) must not reach the composer or a task.
            let handled_by_help = if help {
                match &event {
                    Event::Key(key) => {
                        if matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
                            help = false;
                        }
                        true
                    }
                    _ => false,
                }
            } else {
                false
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
                            if let Some(action) = palette::action_for(command.name) {
                                match action {
                                    palette::Action::Model | palette::Action::Effort => {
                                        picker = true;
                                    }
                                    palette::Action::Help => help = true,
                                    palette::Action::Sessions => {
                                        let mut screen =
                                            sessions_screen::SessionsScreen::load(&config.db_path);
                                        screen.workspace = workspace_label(&config.workspace);
                                        sessions = Some(screen);
                                        if let Some(screen) = sessions.as_mut() {
                                            screen.refresh_preview(&config.db_path);
                                        }
                                    }
                                    palette::Action::Context => {
                                        context_open = true;
                                        context_selected = 0;
                                        context_items = active.as_ref().map_or_else(
                                            Vec::new,
                                            |(engine, session)| {
                                                let ops = engine.context_ops().unwrap_or_default();
                                                context_screen::items(session, &ops, &reducer, 6)
                                            },
                                        );
                                    }
                                    palette::Action::Tools => {
                                        let live = active
                                            .as_ref()
                                            .map_or(&[][..], |(engine, _)| engine.mcp_servers());
                                        tools = Some(
                                            tools_screen::ToolsScreen::load(
                                                &config.workspace,
                                                live,
                                            )
                                            .with_calls(&event_log),
                                        );
                                    }
                                    palette::Action::Account => {
                                        let list =
                                            loom_core::engine::auth::Auth::open(&config.db_path)
                                                .and_then(|auth| auth.list(None))
                                                .unwrap_or_default();
                                        accounts = Some(account_screen::AccountScreen::new(list));
                                    }
                                    palette::Action::Compare => {
                                        compare = Some(compare_screen::CompareScreen::load(
                                            &config.db_path,
                                        ));
                                    }
                                    palette::Action::New => {
                                        clear_live_view(
                                            &mut active,
                                            &mut reducer,
                                            &mut event_log,
                                            &mut last_elapsed,
                                            &mut last_dropped,
                                            &mut recent,
                                            &config.db_path,
                                        );
                                        notice = Some("started a new session".into());
                                    }
                                    palette::Action::Clear => {
                                        let failure = active.as_ref().and_then(|(_, session)| {
                                            delete_session(&config.db_path, &session.id.to_string())
                                                .err()
                                        });
                                        if let Some(error) = failure {
                                            notice = Some(error);
                                        } else {
                                            clear_live_view(
                                                &mut active,
                                                &mut reducer,
                                                &mut event_log,
                                                &mut last_elapsed,
                                                &mut last_dropped,
                                                &mut recent,
                                                &config.db_path,
                                            );
                                            notice = Some("cleared the current session".into());
                                        }
                                    }
                                    palette::Action::ClearHistory => {
                                        match clear_all_sessions(&config.db_path) {
                                            Ok(count) => {
                                                clear_live_view(
                                                    &mut active,
                                                    &mut reducer,
                                                    &mut event_log,
                                                    &mut last_elapsed,
                                                    &mut last_dropped,
                                                    &mut recent,
                                                    &config.db_path,
                                                );
                                                notice = Some(match count {
                                                    0 => "no saved sessions to clear".into(),
                                                    1 => "cleared 1 saved session".into(),
                                                    count => {
                                                        format!("cleared {count} saved sessions")
                                                    }
                                                });
                                            }
                                            Err(error) => notice = Some(error),
                                        }
                                    }
                                    palette::Action::Compact => {
                                        compact_next = true;
                                        notice = Some(
                                            "context will be compacted before the next task".into(),
                                        );
                                    }
                                    palette::Action::Export => {
                                        notice = Some(export_task(
                                            &config.workspace.join(".loom").join("exports"),
                                            &event_log,
                                            active.as_ref().map(|(_, s)| s),
                                            &config,
                                            reducer.summary.as_ref(),
                                        ));
                                    }
                                    palette::Action::Retry => {
                                        if reducer.failure.is_some() {
                                            if let Some(task) = reducer.original_instruction.clone()
                                            {
                                                pending_display_prompt = Some(task.clone());
                                                pending_task = Some(task);
                                                notice = Some("re-running the task…".into());
                                            }
                                        } else {
                                            notice = Some("no failed task to retry".to_owned());
                                        }
                                    }
                                    palette::Action::Copy => {
                                        notice = Some(
                                            match final_answer(&reducer)
                                                .filter(|answer| !answer.trim().is_empty())
                                            {
                                                Some(answer) => {
                                                    if copy_answer(&answer).is_ok() {
                                                        "answer copied to clipboard".to_owned()
                                                    } else {
                                                        "clipboard unavailable in this terminal"
                                                            .to_owned()
                                                    }
                                                }
                                                None => "no answer to copy yet".to_owned(),
                                            },
                                        );
                                    }
                                    palette::Action::Reasoning => reducer.toggle_reasoning(),
                                    palette::Action::Sidebar => sidebar = !sidebar,
                                    palette::Action::Quit => should_quit = true,
                                }
                            } else {
                                // An unknown future command is a notice, never
                                // a silent no-op and never a task.
                                notice = Some(format!("{} is not implemented yet", command.name));
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
                    Some(palette::PaletteAction::Complete) => {
                        if let Some(command) = matches.get(palette_selected) {
                            input.text = command.name.to_owned();
                            input.cursor = input.text.len();
                        }
                        true
                    }
                    Some(palette::PaletteAction::Consumed) => true,
                    None => false,
                }
            } else {
                false
            };
            if !handled_by_help
                && !handled_by_palette
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
                        redraw: &mut redraw,
                    },
                )?;
            }
            if key_pressed && picker {
                picker_loaded = false;
            }
        }
        if redraw {
            terminal.clear()?;
            redraw = false;
        }
        if let Some(task) = pending_task.take() {
            pending_task = Some(task);
            picker = false;
        }
    }
}

/// The models an endpoint actually advertised, with a one-line capability
/// summary for J5's DISCOVERED list.
fn discovered_models(
    catalog: Option<&loom_core::providers::ProviderCatalog>,
) -> Vec<(String, String)> {
    use loom_core::providers::ProviderCatalog;

    let capability = |context: Option<u32>, tools: bool, reasoning: bool| {
        let mut parts = vec![context.map_or_else(
            || "context unknown".to_owned(),
            |tokens| format!("{}K context", tokens / 1000),
        )];
        parts.push(if tools {
            "tools".into()
        } else {
            "no tools".into()
        });
        if reasoning {
            parts.push("reasoning".into());
        }
        parts.join(" · ")
    };
    match catalog {
        Some(ProviderCatalog::Custom(catalog)) => catalog
            .models
            .iter()
            .map(|model| {
                (
                    model.id.clone(),
                    capability(
                        model.context_length,
                        model.tool_capable,
                        model.reasoning_capable,
                    ),
                )
            })
            .collect(),
        Some(ProviderCatalog::OpenRouter(catalog)) => catalog
            .models
            .iter()
            .map(|model| {
                (
                    model.id.clone(),
                    capability(
                        model.context_length,
                        model.tool_capable,
                        model.reasoning_capable,
                    ),
                )
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Replays the buffered run events through a [`TraceWriter`] so `/export`
/// produces the same NDJSON + manifest layout as `loom run --trace`.
/// The last committed assistant prose, or the streaming provisional text.
#[allow(clippy::wildcard_enum_match_arm)] // Item is non_exhaustive: only assistant prose is an answer
fn final_answer(reducer: &Reducer) -> Option<String> {
    reducer
        .committed_transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            loom_core::kernel::transcript::Item::Assistant(message)
                if !message.content.trim().is_empty() =>
            {
                Some(message.content.clone())
            }
            _ => None,
        })
        .or_else(|| {
            (!reducer.provisional.text.is_empty()).then(|| reducer.provisional.text.clone())
        })
}

/// Standard base64 with padding; OSC 52 carries clipboard bytes as base64.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// OSC 52 copy: the terminal sets its own clipboard, so this works locally
/// and over SSH with no clipboard dependency.
// shortcut: terminals cap OSC 52 near 100 KB; answers far under that are fine.
fn copy_answer(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    stdout.flush()
}

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

/// Deletes a session and its children from the store.
fn delete_session(db_path: &Path, raw: &str) -> std::result::Result<(), String> {
    let Ok(id) = raw.parse::<loom_core::SessionId>() else {
        return Err("delete failed: bad session id".into());
    };
    if !db_path.exists() {
        return Err("delete failed: no store found".into());
    }
    let Ok(store) = loom_core::store::SqliteStore::open(db_path) else {
        return Err("delete failed: cannot open the store".into());
    };
    store
        .delete_session(&id)
        .map_err(|error| format!("delete failed: {error}"))
}

/// Deletes every saved session, returning how many were removed.
fn clear_all_sessions(db_path: &Path) -> std::result::Result<usize, String> {
    if !db_path.exists() {
        return Ok(0);
    }
    let store = loom_core::store::SqliteStore::open(db_path)
        .map_err(|error| format!("clear failed: {error}"))?;
    let ids = store
        .session_ids()
        .map_err(|error| format!("clear failed: {error}"))?;
    for id in &ids {
        store
            .delete_session(id)
            .map_err(|error| format!("clear failed: {error}"))?;
    }
    Ok(ids.len())
}

/// Drops the live session and its view, then reloads the recent list.
fn clear_live_view(
    active: &mut Option<(Engine, Session)>,
    reducer: &mut Reducer,
    event_log: &mut Vec<loom_core::EventEnvelope>,
    last_elapsed: &mut Option<Duration>,
    last_dropped: &mut Option<u64>,
    recent: &mut Vec<recent::RecentSession>,
    db_path: &Path,
) {
    *active = None;
    *reducer = Reducer::default();
    event_log.clear();
    *last_elapsed = None;
    *last_dropped = None;
    *recent = recent::load(db_path);
}

/// Swaps the live session for a recorded one so the next task continues it.
async fn resume_session(
    config: &EngineConfig,
    overrides: loom_core::EngineOverrides,
    requests: &tokio::sync::mpsc::UnboundedSender<approval::Request>,
    session_allow: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    active: &mut Option<(Engine, Session)>,
    reducer: &mut reducer::Reducer,
    raw: &str,
) -> String {
    let Ok(id) = raw.parse::<loom_core::SessionId>() else {
        return "resume failed: bad session id".into();
    };
    if active.is_none() {
        match open_engine(config, overrides, requests, session_allow).await {
            Ok(pair) => *active = Some(pair),
            Err(error) => return format!("resume failed: {error}"),
        }
    }
    let Some((engine, session_slot)) = active.as_mut() else {
        return "resume failed: no engine open".into();
    };
    match engine.load_session(&id) {
        Ok(Some(session)) => {
            let events = loom_core::store::SqliteStore::open(&config.db_path)
                .and_then(|store| store.load_events(&id))
                .unwrap_or_default();
            *reducer = crate::reducer::Reducer::default();
            reducer.restore_session_with_events(&session, &events);
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
    session_allow: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Engine, Session)> {
    let overrides = overrides.with_write_approval(approval::hook(
        config.workspace.clone(),
        requests.clone(),
        session_allow.clone(),
    ));
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

/// Applies the compact-notice rule: latest text wins, the 8 s timer only
/// runs while the status bar is uncovered.
fn tick_notice(
    notice: &mut Option<String>,
    clock: &mut Option<(String, Option<std::time::Instant>)>,
    covered: bool,
) {
    let Some(text) = notice.as_ref() else {
        *clock = None;
        return;
    };
    if clock.as_ref().is_none_or(|(seen, _)| seen != text) {
        *clock = Some((text.clone(), None));
    }
    let Some((_, since)) = clock.as_mut() else {
        return;
    };
    if !covered && since.get_or_insert_with(std::time::Instant::now).elapsed() >= NOTICE_TTL {
        *notice = None;
        *clock = None;
    }
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
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hello world 123"), "aGVsbG8gd29ybGQgMTIz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn palette_carries_the_wireframe_command_set() {
        let names: Vec<&str> = palette::COMMANDS.iter().map(|c| c.name).collect();
        for command in [
            "/model",
            "/effort",
            "/account",
            "/sessions",
            "/new",
            "/clear",
            "/clear-history",
            "/context",
            "/tools",
            "/retry",
            "/copy",
            "/export",
            "/reasoning",
            "/sidebar",
            "/help",
            "/quit",
        ] {
            assert!(names.contains(&command), "{command} missing from palette");
        }
    }

    #[test]
    fn clear_all_sessions_removes_every_saved_session() {
        let path = std::env::temp_dir().join(format!("loom-clear-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let store = loom_core::store::SqliteStore::open(&path).unwrap();
            for _ in 0..2 {
                store.ensure_session(&loom_core::SessionId::new()).unwrap();
            }
        }
        assert_eq!(clear_all_sessions(&path), Ok(2));
        let store = loom_core::store::SqliteStore::open(&path).unwrap();
        assert_eq!(store.session_ids().unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

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
    fn tui_preferences_parse_known_fields_and_ignore_garbage() {
        let prefs = parse_prefs(
            r#"{"provider":"deepseek","model":"deepseek-flash","effort":"max","account":"work","other":1}"#,
        );
        assert_eq!(prefs.provider.as_deref(), Some("deepseek"));
        assert_eq!(prefs.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(prefs.effort, Some(ReasoningEffort::Max));
        assert_eq!(prefs.account.as_deref(), Some("work"));

        let empty = parse_prefs("not json");
        assert!(empty.provider.is_none());
        assert!(empty.model.is_none());
        assert!(empty.effort.is_none());
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
        let mut redraw = false;
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
                redraw: &mut redraw,
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
        let mut redraw = false;
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
                redraw: &mut redraw,
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
                    redraw: &mut redraw,
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
        let mut redraw = false;
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
                    redraw: &mut redraw,
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
