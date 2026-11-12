use super::SCRUB_FLOOR;

/// Suffix patterns that mark an environment variable as a credential.
/// `loom provider add` mints `<NAME>_API_KEY` names dynamically, so a
/// fixed list can never cover them; any *present* variable matching
/// one of these suffixes is scrubbed alongside the floor.
pub(crate) const SCRUB_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD"];

/// Whether `name` is a credential by the floor list or a suffix
/// pattern. Pure, so it is testable without touching the live
/// environment.
pub(super) fn matches_scrub(name: &str) -> bool {
    SCRUB_FLOOR.contains(&name) || SCRUB_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The names to remove from a child environment for one spawn: every
/// variable currently in *this* process's environment that matches the
/// floor or a credential suffix, minus the rule's explicit
/// `env_allow` opt-ins. Computed from `std::env::vars_os()` at spawn
/// time so `loom provider add <name>` credentials added after startup
/// are still caught.
pub(crate) fn scrub_names(allow: &[String]) -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| matches_scrub(name) && !allow.contains(name))
        .collect()
}
