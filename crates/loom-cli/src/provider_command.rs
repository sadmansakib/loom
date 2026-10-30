//! `loom provider`: register, list, and remove named custom gateways.

use anyhow::{Context, Result};

use loom_core::engine::registry::{ProviderRegistry, RegistryEntry, env_var_name, validate_name};

use crate::args::ProviderCommand;

pub fn run(command: ProviderCommand) -> Result<()> {
    match command {
        ProviderCommand::Add {
            name,
            base_url,
            models_url,
        } => add(&name, &base_url, models_url),
        ProviderCommand::List => list(),
        ProviderCommand::Remove { name } => remove(&name),
    }
}

fn add(name: &str, base_url: &str, models_url: Option<String>) -> Result<()> {
    validate_name(name).map_err(anyhow::Error::msg)?;
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        anyhow::bail!("--base-url must be an http(s) URL, got `{base_url}`");
    }
    let db_path = loom_core::paths::db_path();
    let mut registry = ProviderRegistry::load(&db_path);
    registry.upsert(
        name,
        RegistryEntry {
            base_url: base_url.to_owned(),
            models_url,
        },
    );
    registry
        .save(&db_path)
        .context("could not write the provider registry")?;
    println!("added provider `{name}`");
    println!(
        "  key:  export {} or `loom auth login {name}`",
        env_var_name(name)
    );
    println!("  run:  loom run --provider {name} --model <model> \"task\"");
    Ok(())
}

fn list() -> Result<()> {
    let registry = ProviderRegistry::load(&loom_core::paths::db_path());
    let mut entries = registry.entries().peekable();
    if entries.peek().is_none() {
        println!("no named providers registered");
        return Ok(());
    }
    for (name, entry) in entries {
        match &entry.models_url {
            Some(url) => println!("{name}  {}  {url}", entry.base_url),
            None => println!("{name}  {}", entry.base_url),
        }
    }
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let db_path = loom_core::paths::db_path();
    let mut registry = ProviderRegistry::load(&db_path);
    if !registry.remove(name) {
        anyhow::bail!("provider `{name}` is not registered");
    }
    registry
        .save(&db_path)
        .context("could not write the provider registry")?;
    println!("removed provider `{name}`");
    Ok(())
}
