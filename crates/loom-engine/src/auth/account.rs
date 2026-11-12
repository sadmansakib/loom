//! Stored-account metadata: ids, methods, and the `Account` row shape.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::error::AuthError;
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
