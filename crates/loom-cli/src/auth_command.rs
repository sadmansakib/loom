//! Execution of `loom auth ...` (ADR-009, architecture §18.3).
//!
//! The engine ([`loom_core::engine::auth`]) owns the store, the keychain backend,
//! and every account mutation; this module only reads flags, prompts for the
//! secret, and reports the result. Printed output never contains the secret —
//! at most its last four characters.

use std::io::Read;

use anyhow::Context;
use loom_core::engine::auth::{self, Auth, DEFAULT_PROVIDER, LoginSpec, Method};

use crate::args::{AuthCommand, ListArgs, LoginArgs, RemoveArgs, UseArgs};

/// Runs one `loom auth` subcommand.
pub fn run(command: AuthCommand) -> anyhow::Result<()> {
    let auth = Auth::open().context("opening the account store")?;
    match command {
        AuthCommand::Login(args) => login(&auth, args),
        AuthCommand::List(args) => list(&auth, args),
        AuthCommand::Use(args) => use_account(&auth, args),
        AuthCommand::Remove(args) => remove(&auth, args),
    }
}

fn login(auth: &Auth, args: LoginArgs) -> anyhow::Result<()> {
    let provider = args.provider;
    let secret = if args.method == Method::Key {
        Some(read_secret(args.stdin)?)
    } else {
        None
    };
    let account = auth.login(LoginSpec {
        provider: provider.clone(),
        label: args.label,
        method: args.method,
        command: args.command,
        secret,
    })?;
    println!(
        "added {provider} account `{}` ({}){}",
        account.label,
        account.method.as_str(),
        account
            .last4
            .as_deref()
            .map(|last4| format!(" ...{last4}"))
            .unwrap_or_default()
    );
    Ok(())
}

fn list(auth: &Auth, args: ListArgs) -> anyhow::Result<()> {
    let accounts = auth.list(args.provider.as_deref())?;
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
        None => providers.push(DEFAULT_PROVIDER.to_owned()),
    }
    providers.sort();
    providers.dedup();
    for provider in providers {
        for name in auth::provider_env_names(&provider) {
            let state = match std::env::var(name) {
                Ok(value) if !value.is_empty() => "set",
                _ => "unset",
            };
            println!("  environment {provider} {name} ({state})");
        }
    }
    Ok(())
}

fn use_account(auth: &Auth, args: UseArgs) -> anyhow::Result<()> {
    let account = auth.activate(&args.provider, &args.target)?;
    println!("active {} account: {}", args.provider, account.label);
    Ok(())
}

fn remove(auth: &Auth, args: RemoveArgs) -> anyhow::Result<()> {
    let removed = auth.remove(&args.provider, &args.target)?;
    if let Some(warning) = &removed.secret_warning {
        eprintln!("warning: could not remove the keychain secret: {warning}");
    }
    if let Some(next) = &removed.promoted {
        eprintln!(
            "`{}` was active; promoted `{}`",
            removed.account.label, next.label
        );
    }
    println!(
        "removed {} account `{}`",
        args.provider, removed.account.label
    );
    Ok(())
}

/// Reads a secret from stdin (`--stdin`) or prompts without echo.
fn read_secret(from_stdin: bool) -> Result<String, auth::AuthError> {
    let raw = if from_stdin {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| auth::AuthError::Invalid(error.to_string()))?;
        buffer
    } else {
        rpassword::prompt_password("Paste the API key (input hidden): ")
            .map_err(|_| auth::AuthError::NoSecretInput)?
    };
    let secret = raw.trim();
    if secret.is_empty() {
        return Err(auth::AuthError::NoSecretInput);
    }
    Ok(secret.to_owned())
}
