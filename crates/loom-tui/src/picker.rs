//! Model picker state: discovery of providers and models, search, and selection.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use loom_core::kernel::model::ModelCapabilities;
use loom_core::{Engine, EngineConfig, ReasoningEffort};

#[derive(Debug, Clone)]
pub(crate) struct ModelChoice {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) capabilities: Option<ModelCapabilities>,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) efforts: Vec<ReasoningEffort>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ModelPricing {
    /// USD per million input tokens.
    pub(crate) input_per_million: Option<f64>,
    /// USD per million output tokens.
    pub(crate) output_per_million: Option<f64>,
}

impl ModelPricing {
    pub(crate) fn from_per_token(input: Option<f64>, output: Option<f64>) -> Option<Self> {
        (input.is_some() || output.is_some()).then(|| Self {
            input_per_million: input.map(|price| price * 1_000_000.0),
            output_per_million: output.map(|price| price * 1_000_000.0),
        })
    }

    #[allow(clippy::cast_precision_loss)] // estimates are inherently floating-point
    pub(crate) fn estimate(self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let input_cost = match (self.input_per_million, input_tokens) {
            (Some(rate), tokens) => rate * tokens as f64 / 1_000_000.0,
            (None, 0) => 0.0,
            (None, _) => return None,
        };
        let output_cost = match (self.output_per_million, output_tokens) {
            (Some(rate), tokens) => rate * tokens as f64 / 1_000_000.0,
            (None, 0) => 0.0,
            (None, _) => return None,
        };
        Some(input_cost + output_cost)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PickerState {
    pub(crate) choices: Vec<ModelChoice>,
    pub(crate) selected: usize,
    pub(crate) search_query: String,
    pub(crate) searching: bool,
    pub(crate) credentials: std::collections::HashMap<String, loom_core::providers::SecretString>,
    pub(crate) catalogs: std::collections::HashMap<String, loom_core::providers::ProviderCatalog>,
    pub(crate) effort: ReasoningEffort,
    pub(crate) notice: Option<String>,
}

/// What a key press in the model picker asks the run loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerAction {
    /// The key changed picker state (or was ignored) with no loop-level effect.
    None,
    /// Leave search mode and clear the query.
    LeaveSearch,
    /// Leave the picker and return to the active session.
    CloseToSession,
    /// Quit the TUI.
    Quit,
    /// Apply the selected model and open an engine.
    Select,
    /// Rediscover providers and models.
    Refresh,
    /// Toggle the help overlay.
    ToggleHelp,
}

/// Maps one picker key to its loop-level action, applying the picker's
/// own state edits first. Guard order matches the original `match`.
pub(crate) fn handle_picker_key(
    picker: &mut PickerState,
    code: KeyCode,
    modifiers: KeyModifiers,
    has_active_session: bool,
) -> PickerAction {
    if navigate_picker(picker, code) {
        return PickerAction::None;
    }
    match code {
        KeyCode::Esc if picker.searching => {
            picker.searching = false;
            picker.set_query(String::new());
            PickerAction::LeaveSearch
        }
        KeyCode::Esc if has_active_session => PickerAction::CloseToSession,
        KeyCode::Esc => PickerAction::Quit,
        KeyCode::Char('/') if !picker.searching => {
            picker.searching = true;
            PickerAction::None
        }
        KeyCode::Backspace if picker.searching => {
            picker.search_query.pop();
            picker.refresh_filter_selection();
            PickerAction::None
        }
        KeyCode::Char(ch)
            if picker.searching
                && !modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
        {
            picker.search_query.push(ch);
            picker.refresh_filter_selection();
            PickerAction::None
        }
        KeyCode::Tab if picker.has_supported_effort() => {
            picker.next_effort();
            PickerAction::None
        }
        KeyCode::Enter if !picker.filtered_indices().is_empty() => PickerAction::Select,
        KeyCode::Char('r') if !picker.searching => PickerAction::Refresh,
        KeyCode::F(1) => PickerAction::ToggleHelp,
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
        | KeyCode::Enter => PickerAction::None,
    }
}

pub(crate) fn navigate_picker(picker: &mut PickerState, code: KeyCode) -> bool {
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
    pub(crate) fn empty() -> Self {
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

    pub(crate) fn configured(config: &EngineConfig) -> Self {
        let choice = model_choice(
            config.provider.clone(),
            config.model.clone(),
            capabilities_for(&config.provider, &config.model),
            None,
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
    pub(crate) async fn discover(config: &EngineConfig) -> Result<Self> {
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
                                ModelPricing::from_per_token(
                                    model.prompt_price,
                                    model.completion_price,
                                ),
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
                                ModelPricing::from_per_token(
                                    model.prompt_price,
                                    model.completion_price,
                                ),
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
                        model_choice(provider.clone(), model, capabilities, None)
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
                None,
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

    pub(crate) fn selected(&self) -> &ModelChoice {
        &self.choices[self.selected.min(self.choices.len().saturating_sub(1))]
    }

    pub(crate) fn move_model(&mut self, delta: isize) {
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

    pub(crate) fn filtered_indices(&self) -> Vec<usize> {
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

    pub(crate) fn set_query(&mut self, query: String) {
        self.search_query = query;
        self.refresh_filter_selection();
    }

    pub(crate) fn refresh_filter_selection(&mut self) {
        let indices = self.filtered_indices();
        if !indices.contains(&self.selected)
            && let Some(index) = indices.first()
        {
            self.selected = *index;
        }
        self.normalize_effort();
    }

    pub(crate) fn next_effort(&mut self) {
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

    pub(crate) fn has_supported_effort(&self) -> bool {
        self.choices
            .get(self.selected)
            .is_some_and(|choice| !choice.efforts.is_empty())
    }

    pub(crate) fn normalize_effort(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        let efforts = &self.selected().efforts;
        if !efforts.is_empty() && !efforts.contains(&self.effort) {
            self.effort = efforts[0];
        }
    }

    pub(crate) fn apply(&self, config: &mut EngineConfig) {
        if !self.filtered_indices().contains(&self.selected) {
            return;
        }
        let choice = self.selected();
        if config.provider != choice.provider {
            config.account = None;
        }
        config.provider.clone_from(&choice.provider);
        config.model.clone_from(&choice.model);
        if choice.efforts.contains(&self.effort) {
            config.reasoning_effort = self.effort;
        }
    }
}

pub(crate) fn model_choice(
    provider: String,
    model: String,
    capabilities: Option<ModelCapabilities>,
    pricing: Option<ModelPricing>,
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
        pricing,
        efforts,
    }
}

pub(crate) fn capabilities_for(provider: &str, model: &str) -> Option<ModelCapabilities> {
    match provider {
        "deepseek" => loom_core::providers::model_capabilities(model),
        "anthropic" => loom_core::providers::anthropic_model_capabilities(model),
        "glm" => loom_core::providers::glm_model_capabilities(model),
        "groq" => loom_core::providers::groq_model_capabilities(model),
        "nvidia" => loom_core::providers::nvidia_model_capabilities(model),
        _ => None,
    }
}

pub(crate) fn environment_credential(provider: &str) -> Option<loom_core::providers::SecretString> {
    loom_core::engine::auth::provider_env_names(provider)
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(loom_core::providers::SecretString::new)
        })
}

pub(crate) fn catalog_credential_provider<'a>(
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

pub(crate) fn catalog_models_url(entry: &loom_core::engine::registry::RegistryEntry) -> String {
    entry
        .models_url
        .clone()
        .unwrap_or_else(|| format!("{}/models", entry.base_url.trim_end_matches('/')))
}
