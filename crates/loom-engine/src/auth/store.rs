//! Account metadata persistence: the [`AuthStore`] seam and its SQLite
//! implementation.

use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, params};

use super::account::{Account, AccountId, Method};
use super::error::AuthError;
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
        // Review S5: without a busy timeout, any concurrent writer
        // (the session store, another loom process) surfaces as an
        // immediate spurious SQLITE_BUSY. Wait briefly instead — the
        // auth store's transactions are tiny.
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| AuthError::Store(error.to_string()))?;
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

/// Opens the account store at the database path the frontend selected.
///
/// Frontends call this instead of knowing the on-disk layout; `loom auth`
/// and `Engine::open` share it.
pub fn open_store(db_path: &std::path::Path) -> Result<SqliteAuthStore, AuthError> {
    SqliteAuthStore::open(db_path)
}
