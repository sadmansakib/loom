//! Model-catalog caches.
//!
//! The engine resolves model capabilities from a catalog, so obtaining
//! one is part of opening an engine, not a frontend chore: [`load`]
//! and [`load_custom`] reuse a fresh cache, refresh a stale one through
//! the credential the provider itself would use, and fall back to a
//! stale cache when the network is unreachable — a transient outage
//! cannot block a run that already has a usable catalog.
//! [`Engine::open_async`] drives this before the synchronous open; a
//! frontend that already holds a snapshot can pass it in
//! [`EngineConfig::catalog`] and use [`Engine::open`].
//!
//! [`load`] serves the OpenRouter adapter; [`load_custom`] serves the
//! `custom` adapter, whose cache is keyed to the models URL it was
//! fetched from (see [`is_custom_fresh`]) so switching endpoints never
//! serves another gateway's model list.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_providers::{self, CustomCatalog, OpenRouterCatalog};

use crate::auth::Auth;
use crate::engine::EngineError;

/// How long a fetched catalog is considered fresh.
const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The cache file beside the database.
///
/// Kept next to `loom.db` so all durable state lives in one directory the
/// user can inspect or delete.
pub fn catalog_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("openrouter-models.json")
}

/// The `custom` adapter's cache file beside the database, one per provider
/// name (`custom`, or a name from the provider registry) so multiple named
/// gateways do not evict each other's catalogs.
pub fn custom_catalog_path(db_path: &Path, provider: &str) -> PathBuf {
    db_path.with_file_name(format!("{provider}-models.json"))
}

/// Returns the credential-resolved, fresh-if-possible catalog.
///
/// Order: a fresh cache wins without touching the network; otherwise the
/// credential is resolved through the engine's account store (the same
/// implementation the provider uses) and the catalog is fetched. A fetch
/// failure falls back to a stale cache when one exists.
pub async fn load(
    auth: &Auth,
    provider: &str,
    account: Option<&str>,
    base_url: Option<&str>,
    db_path: &Path,
) -> Result<OpenRouterCatalog, EngineError> {
    let path = catalog_path(db_path);
    let cached = read_cache(&path);

    if let Some(cached) = &cached
        && is_fresh(cached, loom_providers::now_epoch_secs())
    {
        return Ok(cached.clone());
    }

    let credential = auth.resolve(provider, account)?;
    let base = base_url.unwrap_or(loom_providers::OPENROUTER_DEFAULT_BASE_URL);

    match loom_providers::fetch_catalog(
        base,
        credential.secret.expose(),
        loom_providers::TimeoutConfig::default(),
    )
    .await
    {
        Ok(catalog) => {
            if let Err(error) = write_cache(&path, &catalog) {
                // A cache we cannot write is not a failed run: the
                // snapshot in hand is enough.
                tracing::warn!(%error, path = %path.display(), "could not cache the OpenRouter catalog");
            }
            Ok(catalog)
        }
        Err(error) => match cached {
            Some(cached) => {
                tracing::warn!(%error, "using the stale OpenRouter catalog");
                Ok(cached)
            }
            None => Err(EngineError::Provider(Box::new(error))),
        },
    }
}

/// Whether `catalog` was fetched within [`CATALOG_TTL`].
fn is_fresh(catalog: &OpenRouterCatalog, now: u64) -> bool {
    now.saturating_sub(catalog.fetched_at) < CATALOG_TTL.as_secs()
}

/// Returns the credential-resolved, fresh-if-possible catalog for the
/// `custom` adapter, serving both `--provider custom` and the named
/// providers registered through `loom provider add` (`provider` selects
/// the credential and the cache file).
///
/// Same discipline as [`load`], with one addition: the cache is only
/// usable when it was fetched from *this* `models_url`. A custom cache
/// is shared state across every endpoint the user points at, and
/// serving gateway A's model list to gateway B would turn a
/// configuration change into a wrong-capabilities run. Both the fresh
/// short-circuit and the stale fallback require the URL to match.
pub async fn load_custom(
    auth: &Auth,
    provider: &str,
    account: Option<&str>,
    models_url: &str,
    db_path: &Path,
) -> Result<CustomCatalog, EngineError> {
    let path = custom_catalog_path(db_path, provider);
    let cached = read_custom_cache(&path);

    if let Some(cached) = &cached
        && is_custom_fresh(cached, models_url, loom_providers::now_epoch_secs())
    {
        return Ok(cached.clone());
    }

    let credential = auth.resolve(provider, account)?;

    match loom_providers::fetch_custom_catalog(
        models_url,
        credential.secret.expose(),
        loom_providers::TimeoutConfig::default(),
    )
    .await
    {
        Ok(catalog) => {
            if let Err(error) = write_custom_cache(&path, &catalog) {
                // A cache we cannot write is not a failed run: the
                // snapshot in hand is enough.
                tracing::warn!(%error, path = %path.display(), "could not cache the custom catalog");
            }
            Ok(catalog)
        }
        Err(error) => match cached {
            Some(cached) if cached.source == models_url => {
                tracing::warn!(%error, "using the stale custom catalog");
                Ok(cached)
            }
            _ => Err(EngineError::Provider(Box::new(error))),
        },
    }
}

/// Whether `catalog` was fetched from `models_url` within
/// [`CATALOG_TTL`]: fresh means both recent and for this endpoint.
fn is_custom_fresh(catalog: &CustomCatalog, models_url: &str, now: u64) -> bool {
    catalog.source == models_url && now.saturating_sub(catalog.fetched_at) < CATALOG_TTL.as_secs()
}

/// Reads the cached custom catalog; an unreadable or unparseable cache
/// is treated as absent (it will be replaced on the next successful
/// fetch).
fn read_custom_cache(path: &Path) -> Option<CustomCatalog> {
    let body = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&body) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "ignoring an unreadable custom catalog cache");
            None
        }
    }
}

/// Writes the custom catalog cache, creating the parent directory as
/// needed.
fn write_custom_cache(path: &Path, catalog: &CustomCatalog) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(catalog).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// Reads the cached catalog; an unreadable or unparseable cache is
/// treated as absent (it will be replaced on the next successful fetch).
fn read_cache(path: &Path) -> Option<OpenRouterCatalog> {
    let body = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&body) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "ignoring an unreadable OpenRouter catalog cache");
            None
        }
    }
}

/// Writes the catalog cache, creating the parent directory as needed.
fn write_cache(path: &Path, catalog: &OpenRouterCatalog) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(catalog).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(fetched_at: u64) -> OpenRouterCatalog {
        OpenRouterCatalog {
            fetched_at,
            models: Vec::new(),
        }
    }

    #[test]
    fn cache_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join("loom-engine-catalog-test");
        let path = catalog_path(&dir.join("loom.db"));
        let original = catalog(1_700_000_000);
        write_cache(&path, &original).expect("writes");
        assert_eq!(read_cache(&path), Some(original));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_or_corrupt_cache_reads_as_absent() {
        let path = std::env::temp_dir().join("loom-engine-catalog-missing.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_cache(&path), None);

        let corrupt = std::env::temp_dir().join("loom-engine-catalog-corrupt.json");
        std::fs::write(&corrupt, b"not json").expect("writes");
        assert_eq!(read_cache(&corrupt), None);
        let _ = std::fs::remove_file(&corrupt);
    }

    #[test]
    fn freshness_follows_the_ttl() {
        let now = 1_700_000_000;
        assert!(is_fresh(&catalog(now), now));
        assert!(is_fresh(&catalog(now - CATALOG_TTL.as_secs() + 1), now));
        assert!(!is_fresh(&catalog(now - CATALOG_TTL.as_secs()), now));
        // A future-dated snapshot (clock skew) reads as fresh rather than
        // forcing a refetch on every run.
        assert!(is_fresh(&catalog(now + 100), now));
    }

    #[test]
    fn cache_lives_beside_the_database() {
        let path = catalog_path(Path::new("/var/data/loom.db"));
        assert_eq!(path, Path::new("/var/data/openrouter-models.json"));
        assert_eq!(
            custom_catalog_path(Path::new("/var/data/loom.db"), "custom"),
            Path::new("/var/data/custom-models.json")
        );
        assert_eq!(
            custom_catalog_path(Path::new("/var/data/loom.db"), "orcarouter"),
            Path::new("/var/data/orcarouter-models.json")
        );
    }

    fn custom_catalog(source: &str, fetched_at: u64) -> CustomCatalog {
        CustomCatalog {
            source: source.to_owned(),
            fetched_at,
            models: Vec::new(),
        }
    }

    #[test]
    fn custom_cache_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join("loom-engine-custom-catalog-test");
        let path = custom_catalog_path(&dir.join("loom.db"), "custom");
        let original = custom_catalog("http://gateway.test/models", 1_700_000_000);
        write_custom_cache(&path, &original).expect("writes");
        assert_eq!(read_custom_cache(&path), Some(original));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn custom_freshness_requires_both_ttl_and_matching_source() {
        let url = "http://gateway.test/models";
        let now = 1_700_000_000;
        assert!(is_custom_fresh(&custom_catalog(url, now), url, now));
        assert!(is_custom_fresh(
            &custom_catalog(url, now - CATALOG_TTL.as_secs() + 1),
            url,
            now
        ));
        // Stale.
        assert!(!is_custom_fresh(
            &custom_catalog(url, now - CATALOG_TTL.as_secs()),
            url,
            now
        ));
        // Fresh but fetched from a different endpoint.
        assert!(!is_custom_fresh(
            &custom_catalog("http://other.test/models", now),
            url,
            now
        ));
        // Clock skew reads as fresh rather than forcing a refetch.
        assert!(is_custom_fresh(&custom_catalog(url, now + 100), url, now));
    }
}
