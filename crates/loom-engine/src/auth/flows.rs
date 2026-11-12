//! The frontend auth surface: the [`Auth`] handle and the flow operations
//! (`login`, `activate`, `remove_account`) behind it.

use super::account::{Account, AccountId, Method, last_four, now_millis};
use super::env::provider_env_names;
use super::error::AuthError;
use super::resolve::{ResolvedCredential, resolve, run_command};
use super::secret::{KeychainBackend, SecretBackend};
use super::store::{AuthStore, SqliteAuthStore, open_store};
/// The default credential surface a frontend drives (`loom auth ...`).
///
/// Owns the two backends so a frontend never names the SQLite store or the OS
/// keychain: it opens one `Auth` and calls the operations. The free functions
/// below stay public so tests can inject in-memory backends.
pub struct Auth {
    store: SqliteAuthStore,
    secrets: KeychainBackend,
}

impl Auth {
    /// Opens the frontend-selected store and the keychain backend.
    pub fn open(db_path: &std::path::Path) -> Result<Self, AuthError> {
        Ok(Self {
            store: open_store(db_path)?,
            secrets: KeychainBackend::default(),
        })
    }

    /// Saves a new account and makes it active (see [`login`]).
    pub fn login(&self, spec: LoginSpec) -> Result<Account, AuthError> {
        login(&self.store, &self.secrets, spec)
    }

    /// Lists accounts, optionally filtered to one provider (see [`AuthStore::list`]).
    pub fn list(&self, provider: Option<&str>) -> Result<Vec<Account>, AuthError> {
        self.store.list(provider)
    }

    /// Makes the matching account active (see [`activate`]).
    pub fn activate(&self, provider: &str, target: &str) -> Result<Account, AuthError> {
        activate(&self.store, provider, target)
    }

    /// Removes an account and its secret (see [`remove_account`]).
    pub fn remove(&self, provider: &str, target: &str) -> Result<RemovedAccount, AuthError> {
        remove_account(&self.store, &self.secrets, provider, target)
    }

    /// Resolves the credential for `provider` (ADR-009 order: explicit
    /// `requested` account, then the active account, then the provider's
    /// environment variables).
    ///
    /// A frontend that must act before opening the engine — fetching an
    /// OpenRouter catalog, say — uses this so credential resolution has
    /// exactly one implementation.
    pub fn resolve(
        &self,
        provider: &str,
        requested: Option<&str>,
    ) -> Result<ResolvedCredential, AuthError> {
        resolve(&self.store, &self.secrets, provider, requested, |name| {
            std::env::var(name).ok()
        })
    }
}

/// Everything `loom auth login` needs, with the secret already read.
///
/// The engine never prompts: the frontend reads the secret (hidden prompt or
/// stdin) and passes it here for [`Method::Key`]; [`Method::Command`] carries
/// the shell command instead, and [`Method::Environment`] is rejected because
/// environment credentials are never stored (ADR-009).
#[derive(Debug, Clone)]
pub struct LoginSpec {
    /// Provider the account authenticates.
    pub provider: String,
    /// Human label; defaults to the provider name when `None`.
    pub label: Option<String>,
    /// How the secret is obtained.
    pub method: Method,
    /// Shell command whose stdout is the key, for [`Method::Command`].
    pub command: Option<String>,
    /// The secret, for [`Method::Key`]; `None` for other methods.
    pub secret: Option<String>,
}

/// Saves a new account and makes it the provider's active account.
///
/// A [`Method::Key`] secret is written to the keychain first and dropped again
/// if the row insert fails, so a failed login cannot orphan a keychain entry.
pub fn login(
    store: &dyn AuthStore,
    secrets: &dyn SecretBackend,
    spec: LoginSpec,
) -> Result<Account, AuthError> {
    let provider = spec.provider;
    let label = spec.label.unwrap_or_else(|| provider.clone());

    let account = match spec.method {
        Method::Environment => {
            return Err(AuthError::Invalid(format!(
                "environment credentials are never stored; export {} and `loom run` \
                 will pick it up automatically",
                quote_names(&provider_env_names(&provider))
            )));
        }
        Method::Key => {
            let secret = spec.secret.ok_or(AuthError::NoSecretInput)?;
            let last4 = last_four(&secret);
            let id = AccountId::new();
            secrets.set(&id, &secret)?;
            Account {
                id,
                provider: provider.clone(),
                label,
                method: Method::Key,
                active: false,
                created_at_millis: now_millis(),
                expires_at_millis: None,
                command: None,
                last4,
            }
        }
        Method::Command => {
            let command = spec.command.ok_or_else(|| {
                AuthError::Invalid(String::from("--method command requires --command <SHELL>"))
            })?;
            // Validate now so a broken command fails at login, not at run.
            let secret = run_command(&command)?;
            Account {
                id: AccountId::new(),
                provider: provider.clone(),
                label,
                method: Method::Command,
                active: false,
                created_at_millis: now_millis(),
                expires_at_millis: None,
                command: Some(command),
                last4: last_four(&secret),
            }
        }
    };

    if let Err(error) = store.insert(&account, true) {
        if account.method == Method::Key {
            let _ = secrets.delete(&account.id);
        }
        return Err(error);
    }
    Ok(account)
}

/// Makes the account matching `target` (label or id) the provider's active one.
pub fn activate(store: &dyn AuthStore, provider: &str, target: &str) -> Result<Account, AuthError> {
    let account = store
        .find(provider, target)?
        .ok_or_else(|| AuthError::NoAccount {
            provider: provider.to_owned(),
            target: target.to_owned(),
        })?;
    store.set_active(provider, &account.id)?;
    Ok(account)
}

/// The result of removing an account: what went, and what replaced it.
#[derive(Debug)]
pub struct RemovedAccount {
    /// The account that was removed.
    pub account: Account,
    /// The account promoted to active, when the removed one was active.
    pub promoted: Option<Account>,
    /// A keychain cleanup failure that did not abort the removal.
    pub secret_warning: Option<String>,
}

/// Removes an account and its keychain secret, promoting a replacement.
///
/// A failed keychain delete is reported (not fatal): the row is already gone,
/// so the entry is inert, and the provider must not be left with no active
/// account.
pub fn remove_account(
    store: &dyn AuthStore,
    secrets: &dyn SecretBackend,
    provider: &str,
    target: &str,
) -> Result<RemovedAccount, AuthError> {
    let account = store
        .find(provider, target)?
        .ok_or_else(|| AuthError::NoAccount {
            provider: provider.to_owned(),
            target: target.to_owned(),
        })?;
    store.remove(&account.id)?;

    let secret_warning = if account.method == Method::Key {
        secrets
            .delete(&account.id)
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };

    let promoted = if account.active {
        match store.newest(provider)? {
            Some(next) => {
                store.set_active(provider, &next.id)?;
                Some(next)
            }
            None => None,
        }
    } else {
        None
    };

    Ok(RemovedAccount {
        account,
        promoted,
        secret_warning,
    })
}

/// Renders environment-variable names for the "export one of these" message.
fn quote_names(names: &[String]) -> String {
    if names.is_empty() {
        String::from("the provider's key variable")
    } else {
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(" or ")
    }
}
