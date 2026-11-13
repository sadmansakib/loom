//! Provider naming: the default provider and the ordered environment
//! fallback names (ADR-009).

use loom_providers::{
    ANTHROPIC_PROVIDER_NAME, CUSTOM_PROVIDER_NAME, DEEPSEEK_PROVIDER_NAME, GLM_PROVIDER_NAME,
    GROQ_PROVIDER_NAME, NVIDIA_PROVIDER_NAME, OPENROUTER_PROVIDER_NAME,
};
/// The default provider for Iteration 1.
pub const DEFAULT_PROVIDER: &str = DEEPSEEK_PROVIDER_NAME;

#[must_use]
/// Ordered environment-variable fallback names for a provider (ADR-009).
///
/// Built-ins declare theirs statically; any other name (the named custom
/// providers from `loom provider add`, or a typo) falls back to the
/// derived `{NAME}_API_KEY` spelling, matching the registry's naming
/// rule. Auth is name-generic — the adapter check happens later, at
/// provider build.
pub fn provider_env_names(provider: &str) -> Vec<String> {
    match provider {
        DEEPSEEK_PROVIDER_NAME => vec!["DEEPSEEK_API_KEY".to_owned()],
        ANTHROPIC_PROVIDER_NAME => vec!["ANTHROPIC_API_KEY".to_owned()],
        OPENROUTER_PROVIDER_NAME => vec!["OPENROUTER_API_KEY".to_owned()],
        GLM_PROVIDER_NAME => vec!["ZAI_API_KEY".to_owned(), "Z_AI_API_KEY".to_owned()],
        NVIDIA_PROVIDER_NAME => vec!["NVIDIA_API_KEY".to_owned(), "NIM_API_KEY".to_owned()],
        GROQ_PROVIDER_NAME => vec!["GROQ_API_KEY".to_owned()],
        CUSTOM_PROVIDER_NAME => vec!["CUSTOM_API_KEY".to_owned()],
        other => vec![crate::registry::env_var_name(other)],
    }
}

/// One environment fallback: its name and whether it currently holds a
/// non-empty value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVarState {
    /// The variable name, as [`provider_env_names`] lists it.
    pub name: String,
    /// Whether the variable is set to a non-empty value.
    pub set: bool,
}

#[must_use]
/// Which of `provider`'s environment fallbacks are currently live.
///
/// Resolution consults the environment only after saved accounts (ADR-009),
/// so a frontend reports this beside its account table (`auth list`).
/// Computing it here, next to [`provider_env_names`], keeps the
/// environment knowledge in one place.
pub fn provider_env_state(provider: &str) -> Vec<EnvVarState> {
    provider_env_state_with(provider, |name| std::env::var(name).ok())
}

/// [`provider_env_state`] over an injected environment, for callers and
/// tests that must not touch the process environment.
pub(super) fn provider_env_state_with(
    provider: &str,
    get_env: impl Fn(&str) -> Option<String>,
) -> Vec<EnvVarState> {
    provider_env_names(provider)
        .into_iter()
        .map(|name| EnvVarState {
            set: get_env(&name).is_some_and(|value| !value.is_empty()),
            name,
        })
        .collect()
}
