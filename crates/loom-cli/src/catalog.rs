//! The OpenRouter model-catalog cache.
//!
//! Fetching the catalog is a frontend responsibility: the engine receives
//! the parsed snapshot as data and stays synchronous. This module owns the
//! CLI's policy for obtaining one — reuse a fresh cache, refresh when it is
//! stale, and fall back to a stale cache when the network is unreachable, so
//! a transient outage cannot block a run that already has a usable catalog.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use loom_core::engine::auth::Auth;
use loom_core::providers::{self, OpenRouterCatalog};

/// How long a fetched catalog is considered fresh.
const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The cache file beside the database.
///
/// Kept next to `loom.db` so all durable state lives in one directory the
/// user can inspect or delete.
pub fn catalog_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("openrouter-models.json")
}

/// Returns the credential-resolved, fresh-if-possible catalog.
///
/// Order: a fresh cache wins without touching the network; otherwise the
/// credential is resolved through the engine's account store (the same
/// implementation the engine uses) and the catalog is fetched. A fetch
/// failure falls back to a stale cache when one exists.
pub async fn load(
    auth: &Auth,
    provider: &str,
    account: Option<&str>,
    base_url: Option<&str>,
    db_path: &Path,
) -> anyhow::Result<OpenRouterCatalog> {
    let path = catalog_path(db_path);
    let cached = read_cache(&path);

    if let Some(cached) = &cached
        && is_fresh(cached, now_epoch_secs())
    {
        return Ok(cached.clone());
    }

    let credential = auth
        .resolve(provider, account)
        .context("resolving the OpenRouter credential")?;
    let base = base_url.unwrap_or(providers::OPENROUTER_DEFAULT_BASE_URL);

    match providers::fetch_catalog(
        base,
        credential.secret.expose(),
        providers::TimeoutConfig::default(),
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
            None => Err(error).context("fetching the OpenRouter model catalog"),
        },
    }
}

/// Whether `catalog` was fetched within [`CATALOG_TTL`].
fn is_fresh(catalog: &OpenRouterCatalog, now: u64) -> bool {
    now.saturating_sub(catalog.fetched_at) < CATALOG_TTL.as_secs()
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
fn write_cache(path: &Path, catalog: &OpenRouterCatalog) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_vec(catalog).context("serializing the OpenRouter catalog")?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// Unix seconds now; `0` if the clock is before the epoch.
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
        let dir = std::env::temp_dir().join("loom-cli-catalog-test");
        let path = catalog_path(&dir.join("loom.db"));
        let original = catalog(1_700_000_000);
        write_cache(&path, &original).expect("writes");
        assert_eq!(read_cache(&path), Some(original));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_or_corrupt_cache_reads_as_absent() {
        let path = std::env::temp_dir().join("loom-cli-catalog-missing.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_cache(&path), None);

        let corrupt = std::env::temp_dir().join("loom-cli-catalog-corrupt.json");
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
    }
}
