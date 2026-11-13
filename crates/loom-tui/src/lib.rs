//! Loom's terminal UI. The frontend owns only presentation and task control;
//! engine, provider, tools, and session semantics come from `loom-core`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod glyphs;
mod reducer;
mod scene_views;
mod theme;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
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

#[allow(clippy::too_many_lines)] // terminal lifecycle and task event loop are intentionally serialized
async fn run_interactive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut config: EngineConfig,
    mut overrides: Option<loom_core::EngineOverrides>,
    mut pending_task: Option<String>,
) -> Result<()> {
    let mut reducer = Reducer::default();
    let mut input = PromptEditor::default();
    let file_index = workspace_files(&config.workspace);
    let mut thread_scroll_back = 0_usize;
    let mut pending_display_prompt = None;
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
                    code if navigate_picker(&mut picker_state, code) => {}
                    KeyCode::Esc if picker_state.searching => {
                        picker_state.searching = false;
                        picker_state.set_query(String::new());
                    }
                    KeyCode::Esc if active.is_some() => picker = false,
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Char('/') if !picker_state.searching => {
                        picker_state.searching = true;
                    }
                    KeyCode::Backspace if picker_state.searching => {
                        picker_state.search_query.pop();
                        picker_state.refresh_filter_selection();
                    }
                    KeyCode::Char(ch)
                        if picker_state.searching
                            && !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) =>
                    {
                        picker_state.search_query.push(ch);
                        picker_state.refresh_filter_selection();
                    }
                    KeyCode::Tab if picker_state.has_supported_effort() => {
                        picker_state.next_effort();
                    }
                    KeyCode::Enter if !picker_state.filtered_indices().is_empty() => {
                        picker_state.searching = false;
                        picker_state.apply(&mut config);
                        let previous_session = active.take().map(|(_, session)| session);
                        if let Some(catalog) = picker_state.catalogs.get(&config.provider) {
                            config.catalog = Some(catalog.clone());
                        }
                        let mut engine_overrides = overrides.take().unwrap_or_default();
                        if let Some(credential) = picker_state.credentials.get(&config.provider) {
                            engine_overrides = engine_overrides.with_credential(credential.clone());
                        }
                        let (engine, session) = open_engine(&config, engine_overrides).await?;
                        active = Some((engine, previous_session.unwrap_or(session)));
                        picker = false;
                    }
                    KeyCode::Char('r') if !picker_state.searching => {
                        picker_state = PickerState::discover(&config).await?;
                        picker_loaded = true;
                    }
                    KeyCode::F(1) => help = !help,
                    KeyCode::Backspace
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Up
                    | KeyCode::Down
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
                    | KeyCode::Modifier(_)
                    | KeyCode::Enter => {}
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
            let mut run = Box::pin(engine.run_task(session, task, &sink, &task_control));
            let result = loop {
                terminal.draw(|frame| {
                    draw_live(
                        frame,
                        &reducer,
                        &input.text,
                        input.cursor,
                        help,
                        picker,
                        sidebar,
                        Some(started_at.elapsed()),
                        Some(sink.dropped()),
                        thread_scroll_back,
                        &input,
                        &file_index,
                    );
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
                    () = tokio::time::sleep(FRAME_TICK) => {
                        if event::poll(Duration::ZERO)? {
                            match event::read()? {
                                Event::Key(key) => {
                                    if !update_thread_scroll(key, &mut thread_scroll_back) {
                                        handle_key(key, &mut KeyContext {
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
                                        });
                                    }
                                }
                                Event::Mouse(mouse) => update_thread_mouse(mouse, &mut thread_scroll_back),
                                Event::FocusGained
                                | Event::FocusLost
                                | Event::Paste(_)
                                | Event::Resize(..) => {}
                            }
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
                &input.text,
                input.cursor,
                help,
                picker,
                sidebar,
                last_elapsed,
                last_dropped,
                thread_scroll_back,
                &input,
                &file_index,
            );
        })?;
        if should_quit {
            return Ok(());
        }
        if event::poll(FRAME_TICK)? {
            match event::read()? {
                Event::Key(key) => {
                    if !update_thread_scroll(key, &mut thread_scroll_back) {
                        handle_key(
                            key,
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
                        );
                    }
                    if picker {
                        picker_loaded = false;
                    }
                }
                Event::Mouse(mouse) => update_thread_mouse(mouse, &mut thread_scroll_back),
                Event::FocusGained | Event::FocusLost | Event::Paste(_) | Event::Resize(..) => {}
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
    pub(crate) search_query: String,
    pub(crate) searching: bool,
    credentials: std::collections::HashMap<String, loom_core::providers::SecretString>,
    catalogs: std::collections::HashMap<String, loom_core::providers::ProviderCatalog>,
    pub(crate) effort: ReasoningEffort,
    pub(crate) notice: Option<String>,
}

fn navigate_picker(picker: &mut PickerState, code: KeyCode) -> bool {
    let delta = match code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        KeyCode::Char('k') if !picker.searching => Some(-1),
        KeyCode::Char('j') if !picker.searching => Some(1),
        KeyCode::Backspace
        | KeyCode::Enter
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
        | KeyCode::Esc
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => return false,
    };
    if let Some(delta) = delta {
        picker.move_model(delta);
    }
    true
}

impl PickerState {
    fn empty() -> Self {
        Self {
            choices: Vec::new(),
            selected: 0,
            search_query: String::new(),
            searching: false,
            credentials: std::collections::HashMap::new(),
            catalogs: std::collections::HashMap::new(),
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
            search_query: String::new(),
            searching: false,
            credentials: std::collections::HashMap::new(),
            catalogs: std::collections::HashMap::new(),
            effort: config.reasoning_effort,
            notice: None,
        };
        state.normalize_effort();
        state
    }

    #[allow(clippy::too_many_lines)] // discovery is a sequential provider/catalog reconciliation pipeline
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
        let mut catalogs = std::collections::HashMap::new();
        let mut credentials = std::collections::HashMap::new();
        let mut failures = Vec::new();
        let credential_provider = catalog_credential_provider(&config.provider, &registry);
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

            let catalog_driven = provider == "openrouter"
                || provider == "custom"
                || registry.get(&provider).is_some();
            let mut credential = environment_credential(&provider);
            // Resolve only the selected/configured custom provider, or the
            // sole registered gateway when the current provider is built in.
            // This supports the common single-gateway setup (such as bKash)
            // without prompting for every provider during picker discovery.
            let should_resolve_saved_credential = credential_provider == Some(provider.as_str());
            let mut saved_credential_attempted = false;
            let mut has_saved_account = false;
            if credential.is_none() && catalog_driven && should_resolve_saved_credential {
                saved_credential_attempted = true;
                match loom_core::engine::auth::Auth::open(&config.db_path) {
                    Ok(auth) => {
                        has_saved_account = auth
                            .list(Some(&provider))
                            .is_ok_and(|accounts| !accounts.is_empty());
                        match auth.resolve(&provider, config.account.as_deref()) {
                            Ok(resolved) => credential = Some(resolved.secret),
                            Err(error) => {
                                tracing::debug!(%provider, %error, "saved provider credential unavailable during picker discovery");
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%provider, %error, "could not open provider auth store during picker discovery");
                    }
                }
            }
            if catalog_driven {
                // Show a cached catalog even when its credential is not
                // available. This path never reads the keychain.
                let models_url = registry.get(&provider).map(catalog_models_url).or_else(|| {
                    provider_config.models_url.clone().or_else(|| {
                        provider_config
                            .base_url
                            .as_deref()
                            .map(|base_url| format!("{}/models", base_url.trim_end_matches('/')))
                    })
                });
                let cached = match provider.as_str() {
                    "openrouter" => loom_core::engine::catalog::cached_openrouter(&config.db_path)
                        .map(ProviderCatalog::OpenRouter),
                    _ => models_url
                        .as_deref()
                        .and_then(|url| {
                            loom_core::engine::catalog::cached_custom(
                                &config.db_path,
                                &provider,
                                url,
                            )
                        })
                        .map(ProviderCatalog::Custom),
                };

                let resolved_catalog = if credential.is_some() {
                    match Engine::resolve_catalog_with_credential(
                        provider_config.clone(),
                        credential.as_ref(),
                    )
                    .await
                    {
                        Ok(resolved) => resolved.catalog.or(cached.clone()),
                        Err(error) => {
                            if cached.is_none() {
                                failures.push(format!("{provider}: {error}"));
                            }
                            cached.clone()
                        }
                    }
                } else if saved_credential_attempted && has_saved_account {
                    // Saved accounts can have keychain secrets even when
                    // the initial lookup failed transiently. Avoid a
                    // second keychain access; keep the cache instead.
                    cached.clone()
                } else if should_resolve_saved_credential {
                    // No saved secret can be prompted for. Allow public or
                    // anonymous custom model endpoints to populate the
                    // catalog; if the registry endpoint needs auth, its
                    // failure is shown in the picker notice.
                    match Engine::resolve_catalog_with_credential(provider_config.clone(), None)
                        .await
                    {
                        Ok(resolved) => resolved.catalog.or(cached.clone()),
                        Err(error) => {
                            if cached.is_none() {
                                failures.push(format!("{provider}: {error}"));
                            }
                            cached.clone()
                        }
                    }
                } else {
                    cached.clone()
                };

                match resolved_catalog {
                    Some(ProviderCatalog::OpenRouter(catalog)) => {
                        if let Some(secret) = credential.clone() {
                            credentials.insert(provider.clone(), secret);
                        }
                        catalogs.insert(
                            provider.clone(),
                            ProviderCatalog::OpenRouter(catalog.clone()),
                        );
                        choices.extend(catalog.models.iter().map(|model| {
                            model_choice(
                                provider.clone(),
                                model.id.clone(),
                                catalog.capabilities(&model.id),
                            )
                        }));
                    }
                    Some(ProviderCatalog::Custom(catalog)) => {
                        if let Some(secret) = credential.clone() {
                            credentials.insert(provider.clone(), secret);
                        }
                        catalogs.insert(provider.clone(), ProviderCatalog::Custom(catalog.clone()));
                        choices.extend(catalog.models.iter().map(|model| {
                            model_choice(
                                provider.clone(),
                                model.id.clone(),
                                catalog.capabilities(&model.id),
                            )
                        }));
                    }
                    None => {
                        if cached.is_none() {
                            failures.push(format!("{provider}: catalog unavailable"));
                        }
                    }
                }
                continue;
            }

            let Some(credential) = credential else {
                continue;
            };

            match loom_core::providers::discover_builtin_models(
                &provider,
                provider_config.base_url.as_deref(),
                credential.expose(),
            )
            .await
            {
                Ok(models) => {
                    choices.extend(models.into_iter().map(|model| {
                        let capabilities = capabilities_for(&provider, &model);
                        model_choice(provider.clone(), model, capabilities)
                    }));
                }
                Err(error) => failures.push(format!("{provider}: {error}")),
            }
        }
        // Keep the currently configured model selectable even when its
        // provider has no cached catalog or environment credential. The
        // engine resolves the saved credential only after model selection.
        if !config.model.is_empty()
            && !choices
                .iter()
                .any(|choice| choice.provider == config.provider)
        {
            let capabilities = capabilities_for(&config.provider, &config.model);
            choices.push(model_choice(
                config.provider.clone(),
                config.model.clone(),
                capabilities,
            ));
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
            search_query: String::new(),
            searching: false,
            credentials,
            catalogs,
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
        let indices = self.filtered_indices();
        let len = indices.len();
        if len == 0 {
            return;
        }
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = current.wrapping_add_signed(delta) % len;
        self.selected = indices[next];
        self.normalize_effort();
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim().to_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                (query.is_empty()
                    || choice.provider.to_lowercase().contains(&query)
                    || choice.model.to_lowercase().contains(&query))
                .then_some(index)
            })
            .collect()
    }

    fn set_query(&mut self, query: String) {
        self.search_query = query;
        self.refresh_filter_selection();
    }

    fn refresh_filter_selection(&mut self) {
        let indices = self.filtered_indices();
        if !indices.contains(&self.selected)
            && let Some(index) = indices.first()
        {
            self.selected = *index;
        }
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
        if !self.filtered_indices().contains(&self.selected) {
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
fn update_thread_scroll(key: KeyEvent, scroll_back: &mut usize) -> bool {
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

fn update_thread_mouse(mouse: MouseEvent, scroll_back: &mut usize) {
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

fn environment_credential(provider: &str) -> Option<loom_core::providers::SecretString> {
    loom_core::engine::auth::provider_env_names(provider)
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(loom_core::providers::SecretString::new)
        })
}

fn catalog_credential_provider<'a>(
    configured_provider: &'a str,
    registry: &'a loom_core::engine::registry::ProviderRegistry,
) -> Option<&'a str> {
    if configured_provider == "custom" || registry.get(configured_provider).is_some() {
        return Some(configured_provider);
    }

    if configured_provider.is_empty() && registry.entries().count() == 1 {
        return registry.entries().next().map(|(name, _)| name);
    }

    None
}

fn catalog_models_url(entry: &loom_core::engine::registry::RegistryEntry) -> String {
    entry
        .models_url
        .clone()
        .unwrap_or_else(|| format!("{}/models", entry.base_url.trim_end_matches('/')))
}

#[cfg(test)]
fn scroll_from_mouse(kind: MouseEventKind, scroll_back: &mut usize) {
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

fn handle_key(key: KeyEvent, context: &mut KeyContext<'_>) {
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
    } = context;
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            if let Some(control) = control {
                control.cancel();
            } else {
                **quit = true;
            }
        }
        (KeyCode::F(1), _) => **help = !**help,
        (KeyCode::F(2), _) if control.is_none() => {
            **picker = true;
        }
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => **sidebar = !**sidebar,
        (KeyCode::Tab, _) if input.has_file_mention() => input.complete_file(workspace),
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

struct KeyContext<'a> {
    input: &'a mut PromptEditor,
    workspace: &'a Path,
    help: &'a mut bool,
    picker: &'a mut bool,
    sidebar: &'a mut bool,
    quit: &'a mut bool,
    pending_task: &'a mut Option<String>,
    pending_display_prompt: &'a mut Option<String>,
    control: Option<&'a TaskControl>,
    reducer: &'a mut Reducer,
}

#[derive(Debug, Default)]
struct PromptEditor {
    text: String,
    cursor: usize,
}

impl PromptEditor {
    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }

    fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn line_home(&mut self) {
        self.cursor -= self.text[..self.cursor]
            .rfind('\n')
            .map_or(self.cursor, |index| self.cursor - index - 1);
    }

    fn line_end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }

    fn has_file_mention(&self) -> bool {
        self.mention_query().is_some()
    }

    fn mention_query(&self) -> Option<&str> {
        self.text[..self.cursor]
            .rfind('@')
            .filter(|at| !self.text[*at + 1..self.cursor].contains(char::is_whitespace))
            .map(|at| &self.text[at + 1..self.cursor])
    }

    fn complete_file(&mut self, workspace: &Path) {
        let Some(at) = self.text[..self.cursor].rfind('@') else {
            return;
        };
        let query = &self.text[at + 1..self.cursor];
        let Some(candidate) = workspace_files(workspace)
            .into_iter()
            .find(|path| path.starts_with(query))
        else {
            return;
        };
        self.text.replace_range(at + 1..self.cursor, &candidate);
        self.cursor = at + 1 + candidate.len();
    }
}

fn workspace_files(workspace: &Path) -> Vec<String> {
    fn visit(root: &Path, dir: &Path, files: &mut Vec<String>, depth: usize) {
        if depth > 8 || files.len() >= 2_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.')
                || matches!(name.to_str(), Some("target" | "node_modules" | "vendor"))
            {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files, depth + 1);
            } else if path.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut files = Vec::new();
    visit(workspace, workspace, &mut files, 0);
    files.sort();
    files
}

fn expand_file_mentions(
    message: &str,
    workspace: &Path,
) -> (String, Vec<crate::reducer::FileAttachment>) {
    let attachments = mentioned_file_attachments(message, workspace);
    let Ok(workspace) = workspace.canonicalize() else {
        return (message.to_owned(), attachments);
    };
    let mut expanded = message.to_owned();
    for token in message.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ':', ';', ')', ']']);
        if path.is_empty() || path.contains(['*', '?']) {
            continue;
        }
        let candidate = workspace.join(path);
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&workspace) || !canonical.is_file() {
            continue;
        }
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if metadata.len() > 128 * 1024 {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&canonical) else {
            continue;
        };
        let reference = format!("@{path}");
        let attachment = format!(
            "{reference}\n\n<attached file: {path}>\n```\n{contents}\n```\n</attached file>"
        );
        expanded = expanded.replacen(&reference, &attachment, 1);
    }
    (expanded, attachments)
}

fn mentioned_file_attachments(
    message: &str,
    workspace: &Path,
) -> Vec<crate::reducer::FileAttachment> {
    let Ok(workspace) = workspace.canonicalize() else {
        return Vec::new();
    };
    let mut attachments = Vec::new();
    for token in message.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ':', ';', ')', ']']);
        if path.is_empty() || path.contains(['*', '?']) {
            continue;
        }
        let candidate = workspace.join(path);
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&workspace) || !canonical.is_file() {
            continue;
        }
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if metadata.len() > 128 * 1024 {
            continue;
        }
        if std::fs::read_to_string(&canonical).is_err() {
            continue;
        }
        attachments.push(crate::reducer::FileAttachment {
            path: path.to_owned(),
            bytes: metadata.len(),
        });
    }
    attachments
}

#[allow(clippy::too_many_arguments)]
fn draw_live(
    frame: &mut ratatui::Frame<'_>,
    reducer: &Reducer,
    input: &str,
    cursor: usize,
    help: bool,
    picker: bool,
    sidebar: bool,
    elapsed: Option<Duration>,
    dropped: Option<u64>,
    thread_scroll_back: usize,
    input_editor: &PromptEditor,
    file_index: &[String],
) {
    if help {
        scene_views::draw(frame, Scene::Help);
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
                file_suggestions: &suggestions,
                scene: None,
            },
        );
    }
}

fn matching_file_suggestions(file_index: &[String], query: &str) -> Vec<String> {
    file_index
        .iter()
        .filter(|path| path.starts_with(query))
        .take(5)
        .cloned()
        .collect()
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
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
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
        .map(|(provider, model)| model_choice(provider.into(), model.into(), None))
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
}
