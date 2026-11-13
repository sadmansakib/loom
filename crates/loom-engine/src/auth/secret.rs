//! Secret persistence: the [`SecretBackend`] seam, the OS keychain backend,
//! and the in-memory stub for tests.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

use super::account::AccountId;
use super::error::AuthError;
/// OS-keychain service name; the account id is the keychain user.
pub const KEYRING_SERVICE: &str = "loom";

/// Secret storage backend (OS keychain; stub for tests).
pub trait SecretBackend: Send + Sync {
    /// Stores `secret` for `id`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::SecretBackend`] when the backend cannot write.
    fn set(&self, id: &AccountId, secret: &str) -> Result<(), AuthError>;
    /// Retrieves the secret for `id`, or `None` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::SecretBackend`] when the backend cannot read.
    fn get(&self, id: &AccountId) -> Result<Option<String>, AuthError>;
    /// Deletes the secret for `id`. No-op when absent.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::SecretBackend`] when the backend cannot delete.
    fn delete(&self, id: &AccountId) -> Result<(), AuthError>;
}

/// [`SecretBackend`] backed by the OS credential store via `keyring-core`.
///
/// The platform store installs once as keyring-core's default:
/// macOS → the Keychain directly; Linux → the Secret Service (needs a
/// session D-Bus and an unlocked keyring daemon). On Linux, the first
/// platform/storage failure — a headless host with no Secret Service —
/// switches the default to the kernel keyutils store: always present,
/// headless-safe, in-memory (cleared on logout/reboot), so subsequent
/// resolution falls through to the environment names as designed
/// (§18.3) and `loom auth login` still works.
pub struct KeychainBackend {
    service: String,
}

impl KeychainBackend {
    /// Creates a backend writing to the given keychain service.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl Default for KeychainBackend {
    fn default() -> Self {
        Self::new(KEYRING_SERVICE)
    }
}

/// The default credential store is process-global and installed once.
static DEFAULT_STORE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
/// Linux: whether the fallback to the kernel keyutils store ran already.
#[cfg(target_os = "linux")]
static KEYUTILS_FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Installs this platform's default credential store exactly once.
///
/// macOS uses the Keychain with no fallback. Linux starts on the
/// Secret Service — persistent when a session D-Bus and an unlocked
/// keyring daemon exist — and falls back to the kernel keyutils store
/// at construction time if even the store cannot be built (and at
/// operation time via [`try_keyutils_fallback`] when the Secret
/// Service answers with a platform/storage failure). Other targets
/// install nothing: every operation reports the missing store.
fn ensure_default_store() {
    DEFAULT_STORE.get_or_init(|| {
        #[cfg(target_os = "macos")]
        match apple_native_keyring_store::keychain::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(error) => tracing::warn!(%error, "keychain store unavailable"),
        }
        #[cfg(target_os = "linux")]
        {
            let secret_service = match dbus_secret_service_keyring_store::Store::new() {
                Ok(store) => {
                    keyring_core::set_default_store(store);
                    true
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "secret service unavailable; falling back to the kernel keyutils store"
                    );
                    false
                }
            };
            if !secret_service {
                match linux_keyutils_keyring_store::Store::new() {
                    Ok(store) => keyring_core::set_default_store(store),
                    Err(error) => tracing::warn!(%error, "kernel keyutils store unavailable"),
                }
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {}
    });
}

/// Whether a keyring failure is the *store* being unavailable (missing
/// D-Bus, locked keyring, denied access) rather than an answer about
/// the credential itself — only the former is worth a fallback.
fn is_store_failure(error: &keyring_core::Error) -> bool {
    !matches!(
        error,
        keyring_core::Error::NoEntry
            | keyring_core::Error::Invalid(..)
            | keyring_core::Error::Ambiguous(..)
            | keyring_core::Error::BadEncoding(..)
    )
}

/// Linux: switch the default store to the kernel keyutils store and
/// report whether a retry may proceed. Runs at most once per process.
#[cfg(target_os = "linux")]
fn try_keyutils_fallback() -> bool {
    use std::sync::atomic::Ordering;
    if KEYUTILS_FALLBACK.swap(true, Ordering::Relaxed) {
        return false;
    }
    match linux_keyutils_keyring_store::Store::new() {
        Ok(store) => {
            keyring_core::set_default_store(store);
            true
        }
        Err(error) => {
            tracing::warn!(%error, "kernel keyutils store unavailable");
            false
        }
    }
}

/// Non-Linux targets have no fallback store.
#[cfg(not(target_os = "linux"))]
fn try_keyutils_fallback() -> bool {
    false
}

/// Runs `op` against an entry for `service`/`user` on the current
/// default store; on a store failure, swaps to the fallback store once
/// (Linux) and retries the operation.
fn with_store<T>(
    service: &str,
    user: &str,
    op: impl Fn(&keyring_core::Entry) -> keyring_core::Result<T>,
) -> keyring_core::Result<T> {
    ensure_default_store();
    let attempt = || keyring_core::Entry::new(service, user).and_then(|entry| op(&entry));
    match attempt() {
        Err(error) if is_store_failure(&error) && try_keyutils_fallback() => attempt(),
        result => result,
    }
}

impl SecretBackend for KeychainBackend {
    fn set(&self, id: &AccountId, secret: &str) -> Result<(), AuthError> {
        with_store(&self.service, id.as_str(), |entry| {
            entry.set_password(secret)
        })
        .map_err(|error| AuthError::SecretBackend(error.to_string()))
    }

    fn get(&self, id: &AccountId) -> Result<Option<String>, AuthError> {
        match with_store(
            &self.service,
            id.as_str(),
            keyring_core::Entry::get_password,
        ) {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(AuthError::SecretBackend(error.to_string())),
        }
    }

    fn delete(&self, id: &AccountId) -> Result<(), AuthError> {
        match with_store(&self.service, id.as_str(), |entry| {
            entry.delete_credential()
        }) {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(AuthError::SecretBackend(error.to_string())),
        }
    }
}

/// In-memory [`SecretBackend`] for tests: no OS keychain in CI.
#[cfg(test)]
#[derive(Default)]
pub struct StubBackend {
    secrets: Mutex<HashMap<String, String>>,
}

#[cfg(test)]
impl StubBackend {
    #[must_use]
    /// Creates an empty stub backend.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
impl SecretBackend for StubBackend {
    fn set(&self, id: &AccountId, secret: &str) -> Result<(), AuthError> {
        let mut secrets = self
            .secrets
            .lock()
            .map_err(|_| AuthError::SecretBackend(String::from("stub backend poisoned")))?;
        secrets.insert(id.as_str().to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(&self, id: &AccountId) -> Result<Option<String>, AuthError> {
        let secrets = self
            .secrets
            .lock()
            .map_err(|_| AuthError::SecretBackend(String::from("stub backend poisoned")))?;
        Ok(secrets.get(id.as_str()).cloned())
    }

    fn delete(&self, id: &AccountId) -> Result<(), AuthError> {
        let mut secrets = self
            .secrets
            .lock()
            .map_err(|_| AuthError::SecretBackend(String::from("stub backend poisoned")))?;
        secrets.remove(id.as_str());
        Ok(())
    }
}
