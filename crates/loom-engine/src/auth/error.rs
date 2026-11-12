//! Why credential storage or resolution failed.
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
    /// A [`Method::Command`] account's command exceeded its deadline
    /// and was killed. Names the command, never its output — the
    /// output is (meant to be) the secret.
    #[error("credential command `{command}` timed out after {millis}ms and was killed")]
    CommandTimeout {
        /// The shell command that stalled.
        command: String,
        /// The deadline that elapsed, in milliseconds.
        millis: u64,
    },
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
