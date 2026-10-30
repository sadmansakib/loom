//! Named custom providers: a small registry file beside the state database.
//!
//! `--provider custom` points the generic adapter at one endpoint at a
//! time. A team with several OpenAI-compatible gateways needs each under
//! its own name (`loom run --provider orcarouter …`), so `loom provider
//! add` records name → URLs here and the engine routes unknown names
//! through the same custom adapter with the recorded endpoint. The auth
//! store was already name-generic; this registry adds the name → URL leg
//! the adapter needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One named gateway: the chat endpoint, plus the catalog endpoint when
/// it differs from `{base_url}/models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Chat API base URL; `chat/completions` is appended.
    pub base_url: String,
    /// Model-catalog URL; `None` means `{base_url}/models`.
    pub models_url: Option<String>,
}

/// The registry file beside the database, alongside the catalog caches.
pub fn registry_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("providers.json")
}

/// The registered custom providers, name-ordered.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, RegistryEntry>,
}

impl ProviderRegistry {
    /// Reads the registry; an absent or unreadable file reads as empty —
    /// the same tolerance the catalog caches apply.
    pub fn load(db_path: &Path) -> Self {
        let Ok(body) = std::fs::read_to_string(registry_path(db_path)) else {
            return Self::default();
        };
        serde_json::from_str(&body).unwrap_or_else(|error| {
            tracing::warn!(%error, "ignoring an unreadable provider registry");
            Self::default()
        })
    }

    /// Writes the registry, creating the parent directory as needed.
    pub fn save(&self, db_path: &Path) -> std::io::Result<()> {
        let path = registry_path(db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, body)
    }

    /// The entry registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&RegistryEntry> {
        self.providers.get(name)
    }

    /// All entries in name order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &RegistryEntry)> {
        self.providers
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
    }

    /// Inserts or replaces the entry under `name`.
    pub fn upsert(&mut self, name: &str, entry: RegistryEntry) {
        self.providers.insert(name.to_owned(), entry);
    }

    /// Removes the entry under `name`; reports whether one existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.providers.remove(name).is_some()
    }
}

/// Built-in adapter names a registry entry must not shadow — a shadowed
/// adapter would be unreachable, or the registry entry ignored.
pub const BUILTIN_PROVIDERS: &[&str] = &[
    "deepseek",
    "anthropic",
    "openrouter",
    "glm",
    "nvidia",
    "groq",
    "custom",
];

/// A registry name must be lowercase kebab (`[a-z0-9-]`, starting with a
/// letter) and not built-in, so [`env_var_name`] yields one valid,
/// unambiguous variable per name.
pub fn validate_name(name: &str) -> Result<(), String> {
    let well_formed = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !well_formed {
        return Err(format!(
            "`{name}`: names are lowercase letters, digits, and hyphens, starting with a letter"
        ));
    }
    if BUILTIN_PROVIDERS.contains(&name) {
        return Err(format!("`{name}` is a built-in provider"));
    }
    Ok(())
}

/// The environment variable a named provider falls back to:
/// `orcarouter` → `ORCAROUTER_API_KEY`, `my-gateway` → `MY_GATEWAY_API_KEY`.
pub fn env_var_name(name: &str) -> String {
    format!("{}_API_KEY", name.to_ascii_uppercase().replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-engine-registry-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join("loom.db")
    }

    #[test]
    fn round_trips_through_the_file() {
        let db_path = scratch("roundtrip");
        let mut registry = ProviderRegistry::default();
        registry.upsert(
            "orcarouter",
            RegistryEntry {
                base_url: "https://api.orcarouter.ai/v1".to_owned(),
                models_url: None,
            },
        );
        registry.save(&db_path).expect("saves");

        let loaded = ProviderRegistry::load(&db_path);
        let entry = loaded.get("orcarouter").expect("registered");
        assert_eq!(entry.base_url, "https://api.orcarouter.ai/v1");
        assert!(loaded.get("other").is_none());
        let _ = std::fs::remove_dir_all(db_path.parent().expect("parent"));
    }

    #[test]
    fn absent_or_corrupt_file_reads_as_empty() {
        let db_path = scratch("absent");
        assert!(ProviderRegistry::load(&db_path).entries().next().is_none());

        let path = registry_path(&db_path);
        std::fs::write(&path, b"not json").expect("writes");
        assert!(ProviderRegistry::load(&db_path).entries().next().is_none());
        let _ = std::fs::remove_dir_all(db_path.parent().expect("parent"));
    }

    #[test]
    fn remove_reports_whether_an_entry_existed() {
        let mut registry = ProviderRegistry::default();
        registry.upsert(
            "one",
            RegistryEntry {
                base_url: String::new(),
                models_url: None,
            },
        );
        assert!(registry.remove("one"));
        assert!(!registry.remove("one"));
    }

    #[test]
    fn names_reject_builtins_and_bad_characters() {
        assert!(validate_name("orcarouter").is_ok());
        assert!(validate_name("my-gateway").is_ok());
        assert!(validate_name("gpt2").is_ok());
        for bad in ["", "Custom", "custom", "deepseek", "-x", "a_b", "a b", "1a"] {
            assert!(validate_name(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn env_var_names_follow_the_provider_name() {
        assert_eq!(env_var_name("orcarouter"), "ORCAROUTER_API_KEY");
        assert_eq!(env_var_name("my-gateway"), "MY_GATEWAY_API_KEY");
    }
}
