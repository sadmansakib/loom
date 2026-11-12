//! Credential resolution (ADR-009 order) and credential-command execution.

use loom_providers::SecretString;

use super::account::{Account, Method};
use super::env::provider_env_names;
use super::error::AuthError;
use super::secret::SecretBackend;
use super::store::AuthStore;
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
    for name in &names {
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
        env: names,
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
            // Review item 2.8: an empty-but-set variable is not a
            // credential (the env-fallback path in `resolve` already
            // skips it); an empty secret would ride onto the wire as
            // `Authorization: Bearer `.
            for name in provider_env_names(&account.provider) {
                if let Some(value) = get_env(&name)
                    && !value.is_empty()
                {
                    return Ok(SecretString::new(value));
                }
            }
            Err(AuthError::NoCredential {
                provider: account.provider.clone(),
                env: provider_env_names(&account.provider),
            })
        }
    }
}

/// How long a [`Method::Command`] credential command may run before
/// it is killed. Credential helpers are operator configuration; a
/// typo (`sleep 600` for `sleep 6`) or a two-factor prompt must not
/// wedge engine startup forever.
pub(crate) const CREDENTIAL_COMMAND_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Runs a credential command with the standard 30 s deadline.
pub(crate) fn run_command(command: &str) -> Result<String, AuthError> {
    run_command_with_timeout(command, CREDENTIAL_COMMAND_TIMEOUT)
}

/// The injectable-deadline core of [`run_command`]; tests shorten it.
///
/// The wait is bounded: on timeout the child is killed and reaped so
/// no orphan lingers, and the error names the command — never its
/// output, which is a secret. Stderr is discarded for the same
/// reason. The child is killed on timeout from this side (a sync
/// poll loop rather than `kill_on_drop`, because both callers —
/// engine open and `loom auth add` — are synchronous contexts).
pub(super) fn run_command_with_timeout(
    command: &str,
    timeout: std::time::Duration,
) -> Result<String, AuthError> {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| AuthError::Command(error.to_string()))?;
    // Drain stdout on a thread: a chatty command must not fill the
    // pipe and deadlock the bounded wait below.
    let pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
        }
        bytes
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                } else {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                    return Err(AuthError::CommandTimeout {
                        command: command.to_owned(),
                        millis,
                    });
                }
            }
            Err(error) => return Err(AuthError::Command(error.to_string())),
        }
    };
    let output = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(AuthError::Command(format!(
            "`{command}` exited with {status}"
        )));
    }
    let secret = String::from_utf8_lossy(&output).trim().to_owned();
    if secret.is_empty() {
        return Err(AuthError::Command(format!(
            "`{command}` produced no output"
        )));
    }
    Ok(secret)
}
