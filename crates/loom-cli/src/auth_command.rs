//! Execution of `loom auth ...` (ADR-009, architecture §18.3).
//!
//! All account mutation goes through the [`AuthStore`] and [`SecretBackend`]
//! seams; this module only reads flags, prompts for the secret, and reports
//! the result. Printed output never contains the secret — at most its last
//! four characters.

use std::io::Read;

use anyhow::{Context, bail};

use crate::args::{AuthCommand, ListArgs, LoginArgs, RemoveArgs, UseArgs};
use crate::auth::{
    Account, AccountId, AuthError, AuthStore, KeychainBackend, Method, SecretBackend,
    SqliteAuthStore, last_four, now_millis, provider_env_names, run_command,
};
use crate::paths;

/// Runs one `loom auth` subcommand.
pub fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = SqliteAuthStore::open(&paths::db_path()).context("opening the account store")?;
    let secrets = KeychainBackend::default();
    match command {
        AuthCommand::Login(args) => login(&store, &secrets, args),
        AuthCommand::List(args) => list(&store, args),
        AuthCommand::Use(args) => use_account(&store, args),
        AuthCommand::Remove(args) => remove(&store, &secrets, args),
    }
}

fn login(
    store: &dyn AuthStore,
    secrets: &dyn SecretBackend,
    args: LoginArgs,
) -> anyhow::Result<()> {
    let provider = args.provider;
    let label = args.label.unwrap_or_else(|| provider.clone());

    let account = match args.method {
        Method::Environment => {
            let names = provider_env_names(&provider);
            bail!(
                "environment credentials are never stored; export {} and loom run \
                 will pick it up automatically",
                quote_names(names)
            );
        }
        Method::Key => {
            let secret = read_secret(args.stdin)?;
            let last4 = last_four(&secret);
            let id = AccountId::new();
            secrets.set(&id, &secret)?;
            Account {
                id,
                provider: provider.clone(),
                label: label.clone(),
                method: Method::Key,
                active: false,
                created_at_millis: now_millis(),
                expires_at_millis: None,
                command: None,
                last4,
            }
        }
        Method::Command => {
            let command = args.command.ok_or_else(|| {
                AuthError::Invalid(String::from("--method command requires --command <SHELL>"))
            })?;
            // Validate now so a broken command fails at login, not at run.
            let secret = run_command(&command)?;
            Account {
                id: AccountId::new(),
                provider: provider.clone(),
                label: label.clone(),
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
        // The keychain entry is keyed by the account id and has no other
        // owner; drop it so a failed insert cannot orphan it.
        if account.method == Method::Key {
            let _ = secrets.delete(&account.id);
        }
        return Err(error.into());
    }
    println!(
        "added {provider} account `{label}` ({}){}",
        account.method.as_str(),
        account
            .last4
            .as_deref()
            .map(|last4| format!(" ...{last4}"))
            .unwrap_or_default()
    );
    Ok(())
}

fn list(store: &dyn AuthStore, args: ListArgs) -> anyhow::Result<()> {
    let accounts = store.list(args.provider.as_deref())?;
    if accounts.is_empty() {
        println!("no saved accounts");
    } else {
        println!(
            "{:<1} {:<10} {:<20} {:<10} LAST4",
            "", "PROVIDER", "LABEL", "METHOD"
        );
        for account in &accounts {
            println!(
                "{:<1} {:<10} {:<20} {:<10} {}",
                if account.active { "*" } else { "" },
                account.provider,
                account.label,
                account.method.as_str(),
                account
                    .last4
                    .as_deref()
                    .map(|last4| format!("...{last4}"))
                    .unwrap_or_default(),
            );
        }
    }

    let mut providers: Vec<String> = accounts.iter().map(|a| a.provider.clone()).collect();
    match &args.provider {
        Some(provider) => providers.push(provider.clone()),
        None => providers.push(crate::auth::DEFAULT_PROVIDER.to_owned()),
    }
    providers.sort();
    providers.dedup();
    for provider in providers {
        for name in provider_env_names(&provider) {
            let state = match std::env::var(name) {
                Ok(value) if !value.is_empty() => "set",
                _ => "unset",
            };
            println!("  environment {provider} {name} ({state})");
        }
    }
    Ok(())
}

fn use_account(store: &dyn AuthStore, args: UseArgs) -> anyhow::Result<()> {
    let account =
        store
            .find(&args.provider, &args.target)?
            .ok_or_else(|| AuthError::NoAccount {
                provider: args.provider.clone(),
                target: args.target.clone(),
            })?;
    store.set_active(&args.provider, &account.id)?;
    println!("active {} account: {}", args.provider, account.label);
    Ok(())
}

fn remove(
    store: &dyn AuthStore,
    secrets: &dyn SecretBackend,
    args: RemoveArgs,
) -> anyhow::Result<()> {
    let account =
        store
            .find(&args.provider, &args.target)?
            .ok_or_else(|| AuthError::NoAccount {
                provider: args.provider.clone(),
                target: args.target.clone(),
            })?;
    store.remove(&account.id)?;
    if account.method == Method::Key
        && let Err(error) = secrets.delete(&account.id)
    {
        // The row is gone, so the entry is inert; do not abort before
        // promoting a replacement, or the provider could be left with no
        // active account.
        eprintln!("warning: could not remove the keychain secret: {error}");
    }
    if account.active
        && let Some(next) = store.newest(&args.provider)?
    {
        store.set_active(&args.provider, &next.id)?;
        eprintln!("`{}` was active; promoted `{}`", account.label, next.label);
    }
    println!("removed {} account `{}`", args.provider, account.label);
    Ok(())
}

/// Reads a secret from stdin (`--stdin`) or prompts without echo.
fn read_secret(from_stdin: bool) -> Result<String, AuthError> {
    let raw = if from_stdin {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| AuthError::Invalid(error.to_string()))?;
        buffer
    } else {
        rpassword::prompt_password("Paste the API key (input hidden): ")
            .map_err(|_| AuthError::NoSecretInput)?
    };
    let secret = raw.trim();
    if secret.is_empty() {
        return Err(AuthError::NoSecretInput);
    }
    Ok(secret.to_owned())
}

fn quote_names(names: &[&str]) -> String {
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
