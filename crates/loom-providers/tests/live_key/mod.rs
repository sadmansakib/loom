//! Shared credential resolution for live provider tests.
//!
//! Mirrors the engine's resolution order (ADR-009) without importing it —
//! loom-providers must depend on the kernel alone (architecture §1):
//! the active `loom auth login <provider>` account, then the provider's
//! environment variable. Run `loom auth login groq` once and live tests
//! need no env vars at all.

fn loom_db_path() -> std::path::PathBuf {
    if let Ok(db) = std::env::var("LOOM_DB") {
        return db.into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut path = std::path::PathBuf::from(home);
    path.push(".local/share/loom/loom.db");
    path
}
/// Live key from the loom account store (macOS keychain, service `loom`)
/// or environment; fail loudly when neither is configured.
pub fn live_key(provider: &str, env_name: &str) -> String {
    // Active account id from the state db, then its keychain secret
    // (service `loom`, account = the id; labels live only in the db).
    let db = loom_db_path();
    let active = std::process::Command::new("sqlite3")
        .arg(&db)
        // The sqlite3 CLI binds no `?` from argv; provider names come from
        // the test constants, so interpolate (quotes doubled).
        .args([format!(
            "select id from accounts where provider = '{provider}' and active = 1"
        )])
        .output();
    let keychain = active
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!id.is_empty()).then(|| {
                std::process::Command::new("security")
                    .args(["find-generic-password", "-s", "loom", "-a", &id, "-w"])
                    .output()
            })
        })
        .and_then(|o| o.ok())
        .filter(|o| o.status.success());
    if let Some(out) = keychain {
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !key.is_empty() {
            return key;
        }
    }
    if let Ok(value) = std::env::var(env_name)
        && !value.trim().is_empty()
    {
        return value;
    }
    panic!(
        "no {provider} credential: run `loom auth login {provider}` \
         or set {env_name} for live tests"
    );
}
