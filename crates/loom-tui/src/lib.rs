//! Loom's terminal UI. The frontend owns only presentation and task control;
//! engine, provider, tools, and session semantics come from `loom-core`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod approval;
mod glyphs;
mod input;
mod keys;
mod modal;
mod picker;
mod recent;
mod reducer;
mod scene_views;
mod terminal;
mod theme;
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
        mcp: false,
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
    let mut picker = pending_task.is_none();
    let mut sidebar = true;
    let mut should_quit = false;
    let mut last_elapsed = None;
    let mut picker_state = PickerState::configured(&config);
    let mut picker_loaded = false;
    let mut last_dropped = None;
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<approval::Request>();
    let mut pending_approval: Option<approval::Request> = None;
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
            reducer = Reducer::default();
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

async fn open_engine(
    config: &EngineConfig,
    overrides: loom_core::EngineOverrides,
    requests: &tokio::sync::mpsc::UnboundedSender<approval::Request>,
) -> Result<(Engine, Session)> {
    let overrides =
        overrides.with_write_approval(approval::hook(config.workspace.clone(), requests.clone()));
    let engine = Engine::open_async_with(config.clone(), overrides)
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
        assert!(!config.mcp);
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
