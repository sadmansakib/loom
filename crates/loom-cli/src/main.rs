//! `loom` binary — the composition root.
//!
//! Owns configuration (TOML + environment secrets), installs the
//! `tracing` subscriber (logs to stderr), wires the agent to a provider and
//! tool registry, translates Ctrl-C into `CancellationToken` cancellation,
//! and shuts the runtime down with a bounded timeout so orphaned blocking
//! work cannot hang process exit.
//!
//! This is the only crate allowed to depend on all three library crates and
//! the only one allowed to use `anyhow`.

fn main() {
    println!("loom");
}
