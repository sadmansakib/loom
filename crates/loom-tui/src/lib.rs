//! Loom's terminal UI. The frontend owns only presentation and task control;
//! engine, provider, tools, and session semantics come from `loom-core`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod glyphs;
mod reducer;
mod scene_views;
mod theme;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use loom_core::kernel::agent::TurnBudget;
use loom_core::kernel::context::PruneConfig;
use loom_core::kernel::event::ChannelEventSink;
use loom_core::kernel::model::ModelCapabilities;
use loom_core::kernel::session::Session;
use loom_core::tools::ToolBudgets;
use loom_core::{Engine, EngineConfig, EventSink, ReasoningEffort, TaskControl};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use reducer::Reducer;
pub use scene_views::Scene;

const FRAME_TICK: Duration = Duration::from_millis(35);

/// Renders one of the six fixed reference scenes and restores the terminal.
pub fn render_scene(scene: Scene) -> Result<()> {
    terminal_session(|terminal| {
        terminal.draw(|frame| scene_views::draw(frame, scene))?;
        Ok(())
    })
}

/// Open the interactive TUI using the compact settings surface shared by
/// `loom tui` and `loom run --tui`.
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
pub fn run_with_config(task: Option<String>, config: EngineConfig) -> Result<()> {
    run_with_config_and_overrides(task, config, loom_core::EngineOverrides::default())
}

/// Drive the TUI with configuration and engine overrides composed by the CLI.
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

async fn run_interactive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut config: EngineConfig,
    mut overrides: Option<loom_core::EngineOverrides>,
    mut pending_task: Option<String>,
) -> Result<()> {
    let mut reducer = Reducer::default();
    let mut input = String::new();
    let mut help = false;
    let mut picker = pending_task.is_none();
    let mut sidebar = true;
    let mut should_quit = false;
    let mut last_elapsed = None;
    let mut picker_state = PickerState::configured(&config);
    let mut picker_loaded = false;
    let mut last_dropped = None;
    if picker {
        picker_state = PickerState::discover(&config).await?;
        picker_state.apply(&mut config);
        picker_loaded = true;
    }
    let mut active: Option<(Engine, Session)> = None;

    if !picker {
        active = Some(open_engine(&config, overrides.take().unwrap_or_default()).await?);
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
            if event::poll(FRAME_TICK)?
                && let Event::Key(key) = event::read()?
            {
                match key.code {
                    KeyCode::Esc if active.is_some() => picker = false,
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                    KeyCode::Up | KeyCode::Char('k') => picker_state.move_model(-1),
                    KeyCode::Down | KeyCode::Char('j') => picker_state.move_model(1),
                    KeyCode::Tab if picker_state.has_supported_effort() => {
                        picker_state.next_effort()
                    }
                    KeyCode::Enter if !picker_state.choices.is_empty() => {
                        picker_state.apply(&mut config);
                        let previous_session = active.take().map(|(_, session)| session);
                        let (engine, session) =
                            open_engine(&config, overrides.take().unwrap_or_default()).await?;
                        active = Some((engine, previous_session.unwrap_or(session)));
                        picker = false;
                    }
                    KeyCode::Enter => {}
                    KeyCode::Char('r') => {
                        picker_state = PickerState::discover(&config).await?;
                        picker_loaded = true;
                    }
                    KeyCode::Char('?') => help = !help,
                    KeyCode::Backspace
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
                    | KeyCode::Char(_)
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
            continue;
        }

        if let Some(task) = pending_task.take() {
            let Some((engine, session)) = active.as_mut() else {
                active = Some(open_engine(&config, overrides.take().unwrap_or_default()).await?);
                continue;
            };
            let (sink, mut events) = ChannelEventSink::new();
            let task_control = TaskControl::new();
            reducer = Reducer::default();
            reducer.begin_task(task.clone());
            let started_at = std::time::Instant::now();
            let mut run = Box::pin(engine.run_task(session, task, &sink, &task_control));
            let result = loop {
                terminal.draw(|frame| {
                    draw_live(
                        frame,
                        &reducer,
                        &input,
                        help,
                        picker,
                        sidebar,
                        Some(started_at.elapsed()),
                        Some(sink.dropped()),
                    )
                })?;
                tokio::select! {
                    event = events.recv() => {
                        if let Some(event) = event {
                            reducer.reduce(&event, epoch_millis());
                        }
                    }
                    result = &mut run => {
                        break result;
                    }
                    _ = tokio::time::sleep(FRAME_TICK) => {
                        if event::poll(Duration::ZERO)?
                            && let Event::Key(key) = event::read()?
                        {
                            handle_key(key, &mut input, &mut help, &mut picker, &mut sidebar, &mut should_quit, &mut pending_task, Some(&task_control), &mut reducer);
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
            draw_live(
                frame,
                &reducer,
                &input,
                help,
                picker,
                sidebar,
                last_elapsed,
                last_dropped,
            )
        })?;
        if should_quit {
            return Ok(());
        }
        if event::poll(FRAME_TICK)?
            && let Event::Key(key) = event::read()?
        {
            handle_key(
                key,
                &mut input,
                &mut help,
                &mut picker,
                &mut sidebar,
                &mut should_quit,
                &mut pending_task,
                None,
                &mut reducer,
            );
            if picker {
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
) -> Result<(Engine, Session)> {
    let engine = Engine::open_async_with(config.clone(), overrides)
        .await
        .context("open Loom engine")?;
    let session = engine.new_session();
    Ok((engine, session))
}

#[derive(Debug, Clone)]
pub(crate) struct ModelChoice {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) capabilities: Option<ModelCapabilities>,
    pub(crate) efforts: Vec<ReasoningEffort>,
}

#[derive(Debug, Clone)]
pub(crate) struct PickerState {
    pub(crate) choices: Vec<ModelChoice>,
    pub(crate) selected: usize,
    pub(crate) effort: ReasoningEffort,
    pub(crate) notice: Option<String>,
}

impl PickerState {
    fn empty() -> Self {
        Self {
            choices: Vec::new(),
            selected: 0,
            effort: ReasoningEffort::default(),
            notice: None,
        }
    }

    fn configured(config: &EngineConfig) -> Self {
        let choice = model_choice(
            config.provider.clone(),
            config.model.clone(),
            capabilities_for(&config.provider, &config.model),
        );
        let mut state = Self {
            choices: vec![choice],
            selected: 0,
            effort: config.reasoning_effort,
            notice: None,
        };
        state.normalize_effort();
        state
    }

    async fn discover(config: &EngineConfig) -> Result<Self> {
        use loom_core::engine::registry::{BUILTIN_PROVIDERS, ProviderRegistry};
        use loom_core::providers::ProviderCatalog;

        let registry = ProviderRegistry::load(&config.db_path);
        let mut providers = BUILTIN_PROVIDERS
            .iter()
            .map(|provider| (*provider).to_owned())
            .collect::<Vec<_>>();
        providers.extend(registry.entries().map(|(name, _)| name.to_owned()));
        providers.sort();
        providers.dedup();

        let mut choices = Vec::new();
        let mut failures = Vec::new();
        for provider in providers {
            let mut provider_config = config.clone();
            provider_config.provider.clone_from(&provider);
            provider_config.model.clear();
            provider_config.catalog = None;
            if let Some(entry) = registry.get(&provider) {
                provider_config.base_url = Some(entry.base_url.clone());
                provider_config.models_url.clone_from(&entry.models_url);
                provider_config.headers = entry
                    .headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
            }

            if provider == "openrouter" || provider == "custom" || registry.get(&provider).is_some()
            {
                match Engine::resolve_catalog(provider_config).await {
                    Ok(resolved) => match resolved.catalog {
                        Some(ProviderCatalog::OpenRouter(catalog)) => {
                            choices.extend(catalog.models.iter().map(|model| {
                                model_choice(
                                    provider.clone(),
                                    model.id.clone(),
                                    catalog.capabilities(&model.id),
                                )
                            }));
                        }
                        Some(ProviderCatalog::Custom(catalog)) => {
                            choices.extend(catalog.models.iter().map(|model| {
                                model_choice(
                                    provider.clone(),
                                    model.id.clone(),
                                    catalog.capabilities(&model.id),
                                )
                            }));
                        }
                        None => failures.push(format!("{provider}: catalog unavailable")),
                    },
                    Err(error) => failures.push(format!("{provider}: {error}")),
                }
                continue;
            }

            let api_key = {
                let auth = loom_core::engine::auth::Auth::open(&config.db_path)
                    .context("open account store for model discovery")?;
                match auth.resolve(&provider, config.account.as_deref()) {
                    Ok(credential) => Some(credential.secret.expose().to_owned()),
                    Err(_) => None,
                }
            };
            let Some(api_key) = api_key else {
                continue;
            };

            match loom_core::providers::discover_builtin_models(
                &provider,
                config.base_url.as_deref(),
                &api_key,
            )
            .await
            {
                Ok(models) => choices.extend(models.into_iter().map(|model| {
                    let capabilities = capabilities_for(&provider, &model);
                    model_choice(provider.clone(), model, capabilities)
                })),
                Err(error) => failures.push(format!("{provider}: {error}")),
            }
        }
        choices.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| left.model.cmp(&right.model))
        });
        choices
            .dedup_by(|left, right| left.provider == right.provider && left.model == right.model);
        let selected = choices
            .iter()
            .position(|choice| choice.provider == config.provider && choice.model == config.model)
            .unwrap_or(0);
        let mut state = Self {
            choices,
            selected,
            effort: config.reasoning_effort,
            notice: if failures.is_empty() {
                None
            } else {
                Some(failures.join(" · "))
            },
        };
        if state.choices.is_empty() && state.notice.is_none() {
            state.notice = Some(
                "No models discovered. Configure provider credentials or a model catalog.".into(),
            );
        }
        state.normalize_effort();
        Ok(state)
    }

    fn selected(&self) -> &ModelChoice {
        &self.choices[self.selected.min(self.choices.len().saturating_sub(1))]
    }

    fn move_model(&mut self, delta: isize) {
        let len = self.choices.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.normalize_effort();
    }

    fn next_effort(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        if self.selected().capabilities.is_none() {
            return;
        }
        let choices = &self.selected().efforts;
        if choices.is_empty() {
            return;
        }
        let index = choices
            .iter()
            .position(|effort| *effort == self.effort)
            .unwrap_or(0);
        self.effort = choices[(index + 1) % choices.len()];
    }

    fn has_supported_effort(&self) -> bool {
        self.choices
            .get(self.selected)
            .is_some_and(|choice| !choice.efforts.is_empty())
    }

    fn normalize_effort(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        let efforts = &self.selected().efforts;
        if !efforts.is_empty() && !efforts.contains(&self.effort) {
            self.effort = efforts[0];
        }
    }

    fn apply(&self, config: &mut EngineConfig) {
        if self.choices.is_empty() {
            return;
        }
        let choice = self.selected();
        config.provider.clone_from(&choice.provider);
        config.model.clone_from(&choice.model);
        if choice.efforts.contains(&self.effort) {
            config.reasoning_effort = self.effort;
        }
    }
}

fn model_choice(
    provider: String,
    model: String,
    capabilities: Option<ModelCapabilities>,
) -> ModelChoice {
    let efforts = capabilities.map_or_else(Vec::new, |capabilities| {
        capabilities
            .supported_efforts
            .iter()
            .filter_map(|effort| match effort {
                loom_core::kernel::model::ReasoningEffort::None => Some(ReasoningEffort::None),
                loom_core::kernel::model::ReasoningEffort::Low => Some(ReasoningEffort::Low),
                loom_core::kernel::model::ReasoningEffort::High => Some(ReasoningEffort::High),
                loom_core::kernel::model::ReasoningEffort::Max => Some(ReasoningEffort::Max),
                loom_core::kernel::model::ReasoningEffort::Medium => None,
            })
            .collect()
    });
    ModelChoice {
        provider,
        model,
        capabilities,
        efforts,
    }
}

fn capabilities_for(provider: &str, model: &str) -> Option<ModelCapabilities> {
    match provider {
        "deepseek" => loom_core::providers::model_capabilities(model),
        "anthropic" => loom_core::providers::anthropic_model_capabilities(model),
        "glm" => loom_core::providers::glm_model_capabilities(model),
        "groq" => loom_core::providers::groq_model_capabilities(model),
        "nvidia" => loom_core::providers::nvidia_model_capabilities(model),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    input: &mut String,
    help: &mut bool,
    picker: &mut bool,
    sidebar: &mut bool,
    quit: &mut bool,
    pending_task: &mut Option<String>,
    control: Option<&TaskControl>,
    reducer: &mut Reducer,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            if let Some(control) = control {
                control.cancel();
            } else {
                *quit = true;
            }
        }
        (KeyCode::Char('q'), _) => {
            if let Some(control) = control {
                control.cancel();
            }
            *quit = true;
        }
        (KeyCode::Char('?'), _) => *help = !*help,
        (KeyCode::Tab, _) => *sidebar = !*sidebar,
        (KeyCode::Char('m'), _) if control.is_none() => {
            *picker = true;
        }
        (KeyCode::Enter, _) => {
            let message = std::mem::take(input);
            if !message.trim().is_empty() {
                if let Some(control) = control {
                    control.steer(message);
                    reducer.set_follow_up_queued(true);
                } else {
                    *pending_task = Some(message);
                    *picker = false;
                }
            }
        }
        (KeyCode::Backspace, _) => {
            input.pop();
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) if !c.is_control() => {
            input.push(c);
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_live(
    frame: &mut ratatui::Frame<'_>,
    reducer: &Reducer,
    input: &str,
    help: bool,
    picker: bool,
    sidebar: bool,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
) {
    if help {
        scene_views::draw(frame, Scene::Help);
    } else if picker {
        scene_views::draw_picker(frame, &PickerState::empty());
    } else {
        scene_views::draw_live(frame, reducer, input, sidebar, elapsed, dropped, None);
    }
}

fn terminal_session<T>(
    f: impl FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<T>,
) -> Result<T> {
    let mut guard = TerminalGuard::default();
    crossterm::terminal::enable_raw_mode().context("enable terminal raw mode")?;
    guard.raw_mode = true;
    crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)
        .context("enter alternate screen")?;
    guard.alternate_screen = true;
    crossterm::execute!(io::stdout(), event::EnableMouseCapture).context("enable mouse capture")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize terminal")?;
    let result = f(&mut terminal);
    let restore = guard.restore(terminal.backend_mut());
    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

#[derive(Default)]
struct TerminalGuard {
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalGuard {
    fn restore(&mut self, output: &mut impl io::Write) -> Result<()> {
        if self.alternate_screen {
            crossterm::execute!(
                output,
                crossterm::terminal::LeaveAlternateScreen,
                event::DisableMouseCapture
            )
            .context("restore terminal screen")?;
            self.alternate_screen = false;
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
            let _ = crossterm::execute!(
                io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                event::DisableMouseCapture
            );
        }
        if self.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
        assert!(picker.selected().efforts.is_empty());
        picker.next_effort();
        assert!(picker.selected().efforts.is_empty());
    }

    #[test]
    fn default_config_leaves_model_selection_to_discovery() {
        let config = default_config(None, None, None, None, None).unwrap();
        assert!(config.provider.is_empty());
        assert!(config.model.is_empty());
        assert!(!config.mcp);
        assert!(!config.lsp);
        assert!(!config.autofix);
    }

    #[test]
    fn key_input_queues_follow_up_without_spawning_threads() {
        let mut text = "fix the test".to_owned();
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut next = None;
        let mut reducer = Reducer::default();
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut text,
            &mut help,
            &mut picker,
            &mut sidebar,
            &mut quit,
            &mut next,
            None,
            &mut reducer,
        );
        assert_eq!(next.as_deref(), Some("fix the test"));
    }

    #[test]
    fn pause_shortcut_is_not_advertised_or_bound() {
        let mut input = String::new();
        let mut help = false;
        let mut picker = false;
        let mut sidebar = false;
        let mut quit = false;
        let mut next = None;
        let mut reducer = Reducer::default();
        handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            &mut input,
            &mut help,
            &mut picker,
            &mut sidebar,
            &mut quit,
            &mut next,
            None,
            &mut reducer,
        );
        assert_eq!(input, "p");
        assert!(!quit);
    }
}
