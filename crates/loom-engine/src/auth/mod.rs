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

mod account;
mod env;
mod error;
mod flows;
mod resolve;
mod secret;
mod store;

pub use account::{Account, AccountId, Method, last_four, now_millis};
pub use env::{DEFAULT_PROVIDER, EnvVarState, provider_env_names, provider_env_state};
pub use error::AuthError;
pub use flows::{Auth, LoginSpec, RemovedAccount, activate, login, remove_account};
pub use resolve::{CredentialSource, ResolvedCredential, resolve};
#[cfg(test)]
pub use secret::StubBackend;
pub use secret::{KEYRING_SERVICE, KeychainBackend, SecretBackend};
pub use store::{AuthStore, SqliteAuthStore, open_store};

#[cfg(test)]
use env::provider_env_state_with;
#[cfg(test)]
use resolve::run_command_with_timeout;

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
    fn env_var_names_cover_every_shipped_provider() {
        assert_eq!(provider_env_names("deepseek"), vec!["DEEPSEEK_API_KEY"]);
        assert_eq!(provider_env_names("anthropic"), vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(provider_env_names("openrouter"), vec!["OPENROUTER_API_KEY"]);
        assert_eq!(
            provider_env_names("glm"),
            vec!["ZAI_API_KEY", "Z_AI_API_KEY"]
        );
        assert_eq!(
            provider_env_names("nvidia"),
            vec!["NVIDIA_API_KEY", "NIM_API_KEY"]
        );
        assert_eq!(provider_env_names("groq"), vec!["GROQ_API_KEY"]);
        assert_eq!(provider_env_names("custom"), vec!["CUSTOM_API_KEY"]);
        // Any other name (a `loom provider add` name, or a typo) derives
        // its fallback.
        assert_eq!(provider_env_names("nope"), vec!["NOPE_API_KEY"]);
        assert_eq!(provider_env_names("my-gateway"), vec!["MY_GATEWAY_API_KEY"]);
    }
    #[test]
    fn env_state_marks_only_populated_names() {
        // glm declares two fallbacks in order; only a set, non-empty
        // variable counts as live.
        let state = provider_env_state_with("glm", |name| {
            (name == "ZAI_API_KEY").then(|| "k".to_owned())
        });
        assert_eq!(
            state,
            vec![
                EnvVarState {
                    name: "ZAI_API_KEY".to_owned(),
                    set: true
                },
                EnvVarState {
                    name: "Z_AI_API_KEY".to_owned(),
                    set: false
                },
            ]
        );
        // An empty value is as good as absent.
        let state = provider_env_state_with("deepseek", |_| Some(String::new()));
        assert_eq!(
            state,
            vec![EnvVarState {
                name: "DEEPSEEK_API_KEY".to_owned(),
                set: false
            }]
        );
        // Any other name reports its derived fallback, unset here.
        assert_eq!(
            provider_env_state_with("nope", |_| None),
            vec![EnvVarState {
                name: "NOPE_API_KEY".to_owned(),
                set: false
            }]
        );
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
    fn an_empty_environment_is_not_a_credential_for_accounts() {
        // Review item 2.8: an environment-method account must not mint
        // an empty secret (it would ride the wire as `Bearer `).
        let store = store();
        let secrets = StubBackend::new();
        let saved = account("deepseek", "work", Method::Environment);
        store.insert(&saved, true).unwrap();

        let error = resolve(&store, &secrets, "deepseek", None, |name| {
            (name == "DEEPSEEK_API_KEY").then(String::new)
        })
        .unwrap_err();
        assert!(matches!(error, AuthError::NoCredential { .. }), "{error:?}");
    }

    #[test]
    fn an_empty_environment_falls_through_to_the_next_name() {
        let store = store();
        let secrets = StubBackend::new();
        let saved = account("glm", "work", Method::Environment);
        store.insert(&saved, true).unwrap();

        let resolved = resolve(&store, &secrets, "glm", None, |name| match name {
            "ZAI_API_KEY" => Some(String::new()),
            "Z_AI_API_KEY" => Some("second-name-key".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(resolved.secret.expose(), "second-name-key");
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
    fn a_hanging_credential_command_times_out_and_names_the_command() {
        // A shortened deadline stands in for the shipped 30 s one;
        // the run must return at the deadline, not after `sleep 60`.
        let started = std::time::Instant::now();
        let error = run_command_with_timeout("sleep 60", std::time::Duration::from_millis(200))
            .expect_err("must time out");
        assert!(matches!(error, AuthError::CommandTimeout { .. }), "{error}");
        // The error names the command, never its output.
        let message = error.to_string();
        assert!(message.contains("sleep 60"), "{message}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "returned at the deadline ({}s), not after the sleep",
            started.elapsed().as_secs()
        );
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
