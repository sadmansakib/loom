//! Provider credential storage and resolution (ADR-009, architecture §18.3).
//!
//! Account *metadata* lives in the `accounts` table of `loom.db`; the secret
//! itself lives in the OS keychain (service [`KEYRING_SERVICE`], user = the
//! account id). Rows are managed only through `loom auth ...` — never by
//! editing the database.
//!
//! Resolution per run, in order (§18.3):
//!
//! ```text
//! --account <label>  →  active saved account  →  provider env names  →  error
//! ```
//!
//! A saved account deliberately beats the environment (OpenCode V2): an
//! explicit, named choice is not silently overridden by a stray exported
//! variable. The resolved source is reported, never the secret value.
//!
//! The whole subsystem is behind two traits — [`AuthStore`] (SQLite, with an
//! in-memory implementation) and [`SecretBackend`] (OS keychain, with a stub)
//! — so tests never touch a real keychain (CI).

#[cfg(test)]
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use loom_providers::SecretString;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// OS-keychain service name; the account id is the keychain user.
pub const KEYRING_SERVICE: &str = "loom";

/// The default provider for Iteration 1.
pub const DEFAULT_PROVIDER: &str = "deepseek";

/// Ordered environment-variable fallback names for a provider (ADR-009).
///
/// Adapter-declared in spirit; Iteration 1 has exactly one provider. An
/// unknown provider has no fallback and resolves only from saved accounts.
pub fn provider_env_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "deepseek" => &["DEEPSEEK_API_KEY"],
        _ => &[],
    }
}

/// Identity of one stored account, used as the keychain user. A UUID v7
/// string, so ids are time-ordered and collision-free.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    /// Generates a fresh, time-ordered account id.
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AccountId {
    type Err = AuthError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(AuthError::Invalid(String::from(
                "account id must not be empty",
            )));
        }
        Ok(Self(value.to_owned()))
    }
}

/// How a stored account supplies its secret (ADR-009 method taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// An API key held in the OS keychain.
    Key,
    /// A command whose trimmed stdout is the key, re-run when needed.
    Command,
    /// Never stored: the key is read from the provider's ordered env names.
    Environment,
}

impl Method {
    /// Stable token used in the database and in `loom auth list`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Command => "command",
            Self::Environment => "environment",
        }
    }
}

impl FromStr for Method {
    type Err = AuthError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "key" | "api_key" => Ok(Self::Key),
            "command" => Ok(Self::Command),
            "environment" | "env" => Ok(Self::Environment),
            other => Err(AuthError::Invalid(format!("unknown method `{other}`"))),
        }
    }
}

/// One stored account. Metadata only — the secret itself is never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Generated identity; the keychain user for [`Method::Key`].
    pub id: AccountId,
    /// Provider this account authenticates.
    pub provider: String,
    /// Human label; unique only for resolution by newest match.
    pub label: String,
    /// How the secret is obtained.
    pub method: Method,
    /// Whether this is the provider's active account.
    pub active: bool,
    /// Creation time, Unix epoch milliseconds.
    pub created_at_millis: u64,
    /// Optional expiry (OAuth); unused by [`Method::Key`].
    pub expires_at_millis: Option<u64>,
    /// Shell command for [`Method::Command`].
    pub command: Option<String>,
    /// Last four characters of the secret, for display only.
    pub last4: Option<String>,
}

/// Why credential storage or resolution failed.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The account metadata store failed.
    #[error("account store failed: {0}")]
    Store(String),
    /// The secret backend (OS keychain) failed.
    #[error("secret backend failed: {0}")]
    SecretBackend(String),
    /// No saved account and no environment fallback produced a credential.
    #[error("no {provider} credentials: no saved account and none of {env:?} is set")]
    NoCredential {
        /// Provider that was requested.
        provider: String,
        /// Environment names that were tried.
        env: Vec<String>,
    },
    /// The requested account does not exist.
    #[error("{provider} account `{target}` not found")]
    NoAccount {
        /// Provider that was requested.
        provider: String,
        /// Label or id that did not match.
        target: String,
    },
    /// An account row exists but its secret is missing from the keychain.
    #[error("secret for {provider} account `{label}` is missing from the keychain")]
    MissingSecret {
        /// Provider of the account.
        provider: String,
        /// Display label of the account.
        label: String,
    },
    /// A [`Method::Command`] account's command failed.
    #[error("credential command failed: {0}")]
    Command(String),
    /// Caller supplied an invalid value (method, id, empty input).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// Reading a secret required interactive input that was unavailable.
    #[error("no secret provided (pass --stdin or pipe it in)")]
    NoSecretInput,
}

impl From<rusqlite::Error> for AuthError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.to_string())
    }
}

/// Metadata store for accounts (SQLite; in-memory for tests).
///
/// Deliberately **not** `Send + Sync`: the SQLite connection is `Send` but not
/// `Sync`, and the CLI resolves credentials synchronously before it starts
/// the runtime. A future shared store would wrap it in a mutex.
pub trait AuthStore {
    /// Inserts `account`, optionally making it the provider's active account
    /// (demoting the previous one in the same transaction).
    fn insert(&self, account: &Account, make_active: bool) -> Result<(), AuthError>;

    /// Removes an account by id. No-op if absent.
    fn remove(&self, id: &AccountId) -> Result<(), AuthError>;

    /// Makes `id` the active account for its provider.
    fn set_active(&self, provider: &str, id: &AccountId) -> Result<(), AuthError>;

    /// Lists accounts, optionally filtered to one provider, newest first.
    fn list(&self, provider: Option<&str>) -> Result<Vec<Account>, AuthError>;

    /// Finds an account by label or id, newest match first.
    fn find(&self, provider: &str, target: &str) -> Result<Option<Account>, AuthError>;

    /// Returns the active account for a provider, if any.
    fn active(&self, provider: &str) -> Result<Option<Account>, AuthError>;

    /// Returns the newest remaining account for a provider, if any (used to
    /// promote a replacement after removing the active one).
    fn newest(&self, provider: &str) -> Result<Option<Account>, AuthError>;
}

/// SQLite-backed [`AuthStore`], sharing the `loom.db` file with future state.
pub struct SqliteAuthStore {
    connection: Connection,
}

impl SqliteAuthStore {
    /// Opens (creating if needed) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, AuthError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| AuthError::Store(error.to_string()))?;
        }
        let connection =
            Connection::open(path).map_err(|error| AuthError::Store(error.to_string()))?;
        Self::from_connection(connection)
    }

    /// Opens an in-memory store (tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, AuthError> {
        let connection =
            Connection::open_in_memory().map_err(|error| AuthError::Store(error.to_string()))?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, AuthError> {
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS accounts (
                     id           TEXT PRIMARY KEY,
                     provider     TEXT NOT NULL,
                     label        TEXT NOT NULL,
                     method       TEXT NOT NULL,
                     active       INTEGER NOT NULL DEFAULT 0,
                     created_at   INTEGER NOT NULL,
                     expires_at   INTEGER,
                     command      TEXT,
                     last4        TEXT
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS accounts_one_active
                     ON accounts(provider) WHERE active = 1;",
            )
            .map_err(|error| AuthError::Store(error.to_string()))?;
        Ok(Self { connection })
    }

    fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
        let method_text: String = row.get("method")?;
        let method = Method::from_str(&method_text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let id_text: String = row.get("id")?;
        let id = AccountId::from_str(&id_text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(Account {
            id,
            provider: row.get("provider")?,
            label: row.get("label")?,
            method,
            active: row.get::<_, i64>("active")? != 0,
            created_at_millis: row.get::<_, i64>("created_at")? as u64,
            expires_at_millis: row.get::<_, Option<i64>>("expires_at")?.map(|v| v as u64),
            command: row.get("command")?,
            last4: row.get("last4")?,
        })
    }

    fn query(&self, sql: &str, bound: &[&dyn rusqlite::ToSql]) -> Result<Vec<Account>, AuthError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(|error| AuthError::Store(error.to_string()))?;
        let rows = statement
            .query_map(bound, Self::row_to_account)
            .map_err(|error| AuthError::Store(error.to_string()))?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(row.map_err(|error| AuthError::Store(error.to_string()))?);
        }
        Ok(accounts)
    }
}

impl AuthStore for SqliteAuthStore {
    fn insert(&self, account: &Account, make_active: bool) -> Result<(), AuthError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| AuthError::Store(error.to_string()))?;
        if make_active {
            transaction
                .execute(
                    "UPDATE accounts SET active = 0 WHERE provider = ?1",
                    params![account.provider],
                )
                .map_err(|error| AuthError::Store(error.to_string()))?;
        }
        transaction
            .execute(
                "INSERT INTO accounts
                     (id, provider, label, method, active, created_at, expires_at, command, last4)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    account.id.as_str(),
                    account.provider,
                    account.label,
                    account.method.as_str(),
                    i64::from(account.active || make_active),
                    account.created_at_millis as i64,
                    account.expires_at_millis.map(|v| v as i64),
                    account.command,
                    account.last4,
                ],
            )
            .map_err(|error| AuthError::Store(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| AuthError::Store(error.to_string()))?;
        Ok(())
    }

    fn remove(&self, id: &AccountId) -> Result<(), AuthError> {
        self.connection
            .execute("DELETE FROM accounts WHERE id = ?1", params![id.as_str()])
            .map_err(|error| AuthError::Store(error.to_string()))?;
        Ok(())
    }

    fn set_active(&self, provider: &str, id: &AccountId) -> Result<(), AuthError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| AuthError::Store(error.to_string()))?;
        transaction
            .execute(
                "UPDATE accounts SET active = 0 WHERE provider = ?1",
                params![provider],
            )
            .map_err(|error| AuthError::Store(error.to_string()))?;
        let updated = transaction
            .execute(
                "UPDATE accounts SET active = 1 WHERE id = ?1 AND provider = ?2",
                params![id.as_str(), provider],
            )
            .map_err(|error| AuthError::Store(error.to_string()))?;
        if updated == 0 {
            // The demotion above matched the provider but the activation
            // matched no row. Returning here drops the transaction, rolling
            // the demotion back; committing it would leave the provider with
            // no active account.
            return Err(AuthError::NoAccount {
                provider: provider.to_owned(),
                target: id.as_str().to_owned(),
            });
        }
        transaction
            .commit()
            .map_err(|error| AuthError::Store(error.to_string()))?;
        Ok(())
    }

    fn list(&self, provider: Option<&str>) -> Result<Vec<Account>, AuthError> {
        match provider {
            Some(provider) => self.query(
                "SELECT * FROM accounts WHERE provider = ?1 ORDER BY created_at DESC, rowid DESC",
                &[&provider],
            ),
            None => self.query(
                "SELECT * FROM accounts ORDER BY provider, created_at DESC, rowid DESC",
                &[],
            ),
        }
    }

    fn find(&self, provider: &str, target: &str) -> Result<Option<Account>, AuthError> {
        Ok(self
            .query(
                "SELECT * FROM accounts
                 WHERE provider = ?1 AND (label = ?2 OR id = ?2)
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                &[&provider, &target],
            )?
            .into_iter()
            .next())
    }

    fn active(&self, provider: &str) -> Result<Option<Account>, AuthError> {
        Ok(self
            .query(
                "SELECT * FROM accounts WHERE provider = ?1 AND active = 1 LIMIT 1",
                &[&provider],
            )?
            .into_iter()
            .next())
    }

    fn newest(&self, provider: &str) -> Result<Option<Account>, AuthError> {
        Ok(self
            .query(
                "SELECT * FROM accounts WHERE provider = ?1
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                &[&provider],
            )?
            .into_iter()
            .next())
    }
}

/// Secret storage backend (OS keychain; stub for tests).
pub trait SecretBackend: Send + Sync {
    /// Stores `secret` for `id`, replacing any previous value.
    fn set(&self, id: &AccountId, secret: &str) -> Result<(), AuthError>;
    /// Retrieves the secret for `id`, or `None` when absent.
    fn get(&self, id: &AccountId) -> Result<Option<String>, AuthError>;
    /// Deletes the secret for `id`. No-op when absent.
    fn delete(&self, id: &AccountId) -> Result<(), AuthError>;
}

/// [`SecretBackend`] backed by the OS keychain via the `keyring` crate.
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

impl SecretBackend for KeychainBackend {
    fn set(&self, id: &AccountId, secret: &str) -> Result<(), AuthError> {
        let entry = keyring::Entry::new(&self.service, id.as_str())
            .map_err(|error| AuthError::SecretBackend(error.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|error| AuthError::SecretBackend(error.to_string()))
    }

    fn get(&self, id: &AccountId) -> Result<Option<String>, AuthError> {
        let entry = keyring::Entry::new(&self.service, id.as_str())
            .map_err(|error| AuthError::SecretBackend(error.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(AuthError::SecretBackend(error.to_string())),
        }
    }

    fn delete(&self, id: &AccountId) -> Result<(), AuthError> {
        let entry = keyring::Entry::new(&self.service, id.as_str())
            .map_err(|error| AuthError::SecretBackend(error.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
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

/// Where a resolved credential came from (traced, never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// A saved account row.
    Account,
    /// A provider environment variable.
    Environment,
}

impl CredentialSource {
    /// Stable token for tracing (`credential.source`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Environment => "environment",
        }
    }
}

/// A credential ready for provider construction.
#[derive(Debug)]
pub struct ResolvedCredential {
    /// The secret value; redacted in `Debug`.
    pub secret: SecretString,
    /// Where it came from.
    pub source: CredentialSource,
    /// The account used, when the source is a saved account.
    pub account: Option<Account>,
}

/// Resolves the credential for `provider` (ADR-009 order).
///
/// `requested` is the optional `--account` label/id. `get_env` is injected so
/// resolution is testable without touching the process environment.
pub fn resolve(
    store: &dyn AuthStore,
    secrets: &dyn SecretBackend,
    provider: &str,
    requested: Option<&str>,
    get_env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedCredential, AuthError> {
    // 1. Explicit --account beats everything.
    if let Some(target) = requested {
        let account = store
            .find(provider, target)?
            .ok_or_else(|| AuthError::NoAccount {
                provider: provider.to_owned(),
                target: target.to_owned(),
            })?;
        let secret = secret_for(&account, secrets, get_env)?;
        return Ok(ResolvedCredential {
            secret,
            source: CredentialSource::Account,
            account: Some(account),
        });
    }

    // 2. The active saved account beats the environment.
    if let Some(account) = store.active(provider)? {
        let secret = secret_for(&account, secrets, get_env)?;
        return Ok(ResolvedCredential {
            secret,
            source: CredentialSource::Account,
            account: Some(account),
        });
    }

    // 3. Provider environment fallback.
    let names = provider_env_names(provider);
    for name in names {
        if let Some(value) = get_env(name)
            && !value.is_empty()
        {
            return Ok(ResolvedCredential {
                secret: SecretString::new(value),
                source: CredentialSource::Environment,
                account: None,
            });
        }
    }

    Err(AuthError::NoCredential {
        provider: provider.to_owned(),
        env: names.iter().map(|name| (*name).to_owned()).collect(),
    })
}

fn secret_for(
    account: &Account,
    secrets: &dyn SecretBackend,
    get_env: impl Fn(&str) -> Option<String>,
) -> Result<SecretString, AuthError> {
    match account.method {
        Method::Key => secrets
            .get(&account.id)?
            .map(SecretString::new)
            .ok_or_else(|| AuthError::MissingSecret {
                provider: account.provider.clone(),
                label: account.label.clone(),
            }),
        Method::Command => {
            let command = account.command.as_deref().ok_or_else(|| {
                AuthError::Invalid(format!(
                    "account `{}` has method `command` but no command",
                    account.label
                ))
            })?;
            run_command(command).map(SecretString::new)
        }
        Method::Environment => {
            for name in provider_env_names(&account.provider) {
                if let Some(value) = get_env(name)
                    && !value.is_empty()
                {
                    return Ok(SecretString::new(value));
                }
            }
            Err(AuthError::NoCredential {
                provider: account.provider.clone(),
                env: provider_env_names(&account.provider)
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
            })
        }
    }
}

/// Runs a credential command and returns its trimmed stdout.
pub(crate) fn run_command(command: &str) -> Result<String, AuthError> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|error| AuthError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(AuthError::Command(format!(
            "`{command}` exited with {}",
            output.status
        )));
    }
    let secret = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if secret.is_empty() {
        return Err(AuthError::Command(format!(
            "`{command}` produced no output"
        )));
    }
    Ok(secret)
}

/// Unix epoch milliseconds, UTC.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// The last four characters of a secret, for display; `None` when shorter.
pub fn last_four(secret: &str) -> Option<String> {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 4 {
        return None;
    }
    Some(chars[chars.len() - 4..].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SqliteAuthStore {
        SqliteAuthStore::open_in_memory().unwrap()
    }

    fn account(provider: &str, label: &str, method: Method) -> Account {
        Account {
            id: AccountId::new(),
            provider: provider.to_owned(),
            label: label.to_owned(),
            method,
            active: false,
            created_at_millis: now_millis(),
            expires_at_millis: None,
            command: None,
            last4: None,
        }
    }

    #[test]
    fn insert_marks_new_active_and_demotes_previous() {
        let store = store();
        let first = account("deepseek", "one", Method::Key);
        store.insert(&first, true).unwrap();

        let second = account("deepseek", "two", Method::Key);
        store.insert(&second, true).unwrap();

        let active = store.active("deepseek").unwrap().unwrap();
        assert_eq!(active.label, "two");
        let all = store.list(Some("deepseek")).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().filter(|a| a.active).count(), 1);
    }

    #[test]
    fn active_uniqueness_is_enforced_per_provider() {
        let store = store();
        store
            .insert(&account("deepseek", "one", Method::Key), true)
            .unwrap();
        // Second active row for the same provider must not coexist: a raw
        // insert that skips demotion violates the partial unique index.
        let clashing = Account {
            active: true,
            ..account("deepseek", "two", Method::Key)
        };
        let error = store.insert(&clashing, false).unwrap_err();
        assert!(matches!(error, AuthError::Store(_)));
    }

    #[test]
    fn set_active_rejects_an_absent_account_without_demoting() {
        let store = store();
        let acct = account("deepseek", "work", Method::Key);
        store.insert(&acct, true).unwrap();

        let missing = AccountId::new();
        let error = store.set_active("deepseek", &missing).unwrap_err();
        assert!(matches!(error, AuthError::NoAccount { .. }));
        // The rollback left the previous active account in place.
        assert_eq!(store.active("deepseek").unwrap().unwrap().label, "work");
    }

    #[test]
    fn different_providers_each_keep_an_active_account() {
        let store = store();
        store
            .insert(&account("deepseek", "deep", Method::Key), true)
            .unwrap();
        store
            .insert(&account("glm", "glm", Method::Key), true)
            .unwrap();
        assert_eq!(store.active("deepseek").unwrap().unwrap().label, "deep");
        assert_eq!(store.active("glm").unwrap().unwrap().label, "glm");
    }

    #[test]
    fn find_matches_label_or_id() {
        let store = store();
        let acct = account("deepseek", "work", Method::Key);
        store.insert(&acct, true).unwrap();

        assert_eq!(store.find("deepseek", "work").unwrap().unwrap().id, acct.id);
        assert_eq!(
            store
                .find("deepseek", acct.id.as_str())
                .unwrap()
                .unwrap()
                .label,
            "work"
        );
        assert!(store.find("deepseek", "missing").unwrap().is_none());
    }

    #[test]
    fn newest_returns_most_recently_created() {
        let store = store();
        let mut older = account("deepseek", "old", Method::Key);
        older.created_at_millis = 1_000;
        store.insert(&older, false).unwrap();
        let mut newer = account("deepseek", "new", Method::Key);
        newer.created_at_millis = 2_000;
        store.insert(&newer, false).unwrap();

        assert_eq!(store.newest("deepseek").unwrap().unwrap().label, "new");
    }

    #[test]
    fn resolve_prefers_active_account_over_env() {
        let store = store();
        let secrets = StubBackend::new();
        let acct = account("deepseek", "work", Method::Key);
        secrets.set(&acct.id, "account-key").unwrap();
        store.insert(&acct, true).unwrap();

        let resolved = resolve(&store, &secrets, "deepseek", None, |name| {
            (name == "DEEPSEEK_API_KEY").then(|| "env-key".to_owned())
        })
        .unwrap();

        assert_eq!(resolved.secret.expose(), "account-key");
        assert_eq!(resolved.source, CredentialSource::Account);
        assert_eq!(resolved.account.unwrap().label, "work");
    }

    #[test]
    fn resolve_falls_back_to_environment() {
        let store = store();
        let secrets = StubBackend::new();
        let resolved = resolve(&store, &secrets, "deepseek", None, |name| {
            (name == "DEEPSEEK_API_KEY").then(|| "env-key".to_owned())
        })
        .unwrap();

        assert_eq!(resolved.secret.expose(), "env-key");
        assert_eq!(resolved.source, CredentialSource::Environment);
        assert!(resolved.account.is_none());
    }

    #[test]
    fn resolve_explicit_account_overrides_active() {
        let store = store();
        let secrets = StubBackend::new();
        let active = account("deepseek", "active", Method::Key);
        secrets.set(&active.id, "active-key").unwrap();
        store.insert(&active, true).unwrap();
        let other = account("deepseek", "other", Method::Key);
        secrets.set(&other.id, "other-key").unwrap();
        store.insert(&other, false).unwrap();

        let resolved = resolve(&store, &secrets, "deepseek", Some("other"), |_| None).unwrap();
        assert_eq!(resolved.secret.expose(), "other-key");
        assert_eq!(resolved.account.unwrap().label, "other");
    }

    #[test]
    fn resolve_errors_without_any_credential() {
        let store = store();
        let secrets = StubBackend::new();
        let error = resolve(&store, &secrets, "deepseek", None, |_| None).unwrap_err();
        assert!(matches!(error, AuthError::NoCredential { .. }));
    }

    #[test]
    fn resolve_errors_for_unknown_explicit_account() {
        let store = store();
        let secrets = StubBackend::new();
        let error = resolve(&store, &secrets, "deepseek", Some("nope"), |_| None).unwrap_err();
        assert!(matches!(error, AuthError::NoAccount { .. }));
    }

    #[test]
    fn resolve_reports_missing_keychain_secret() {
        let store = store();
        let secrets = StubBackend::new();
        let acct = account("deepseek", "work", Method::Key);
        store.insert(&acct, true).unwrap();

        let error = resolve(&store, &secrets, "deepseek", None, |_| None).unwrap_err();
        assert!(matches!(error, AuthError::MissingSecret { .. }));
    }

    #[test]
    fn resolve_runs_command_method_and_trims_output() {
        let store = store();
        let secrets = StubBackend::new();
        let mut acct = account("deepseek", "cmd", Method::Command);
        acct.command = Some("printf '  command-key\\n'".to_owned());
        store.insert(&acct, true).unwrap();

        let resolved = resolve(&store, &secrets, "deepseek", None, |_| None).unwrap();
        assert_eq!(resolved.secret.expose(), "command-key");
    }

    #[test]
    fn failed_command_is_an_error_not_a_secret() {
        let store = store();
        let secrets = StubBackend::new();
        let mut acct = account("deepseek", "cmd", Method::Command);
        acct.command = Some("exit 3".to_owned());
        store.insert(&acct, true).unwrap();

        let error = resolve(&store, &secrets, "deepseek", None, |_| None).unwrap_err();
        assert!(matches!(error, AuthError::Command(_)));
    }

    #[test]
    fn canary_secret_never_reaches_the_accounts_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loom.db");
        let store = SqliteAuthStore::open(&path).unwrap();
        let secrets = StubBackend::new();
        let canary = "sk-canary-DO-NOT-PERSIST-0000";
        let mut acct = account("deepseek", "work", Method::Key);
        acct.last4 = last_four(canary);
        secrets.set(&acct.id, canary).unwrap();
        store.insert(&acct, true).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let haystack = String::from_utf8_lossy(&bytes);
        assert!(!haystack.contains(canary), "secret leaked into loom.db");
        // The row is still usable: metadata is present, secret is not.
        assert_eq!(
            store.active("deepseek").unwrap().unwrap().last4.as_deref(),
            Some("0000")
        );
    }

    #[test]
    fn account_id_round_trips_and_rejects_empty() {
        let id = AccountId::new();
        let parsed: AccountId = id.as_str().parse().unwrap();
        assert_eq!(parsed, id);
        assert!("".parse::<AccountId>().is_err());
    }

    #[test]
    fn method_round_trips_through_tokens() {
        for method in [Method::Key, Method::Command, Method::Environment] {
            assert_eq!(method.as_str().parse::<Method>().unwrap(), method);
        }
        assert!("oauth".parse::<Method>().is_err());
    }
}
