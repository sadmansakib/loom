//! Command-line surface (clap derive).
//!
//! Iteration 1 deliberately has **no config file** (plan 1H): every knob a
//! run needs is a flag with a sensible default, and secrets are managed
//! through `loom auth`. `--base-url` alone covers proxies and gateways.

mod auth;
mod cli;
mod headers;
mod lessons;
mod provider;
mod replay_context;
mod rewind;
mod run;
mod stats;
#[cfg(test)]
mod tests;
mod trust;

pub use auth::*;
pub use cli::*;
pub(crate) use headers::*;
pub use lessons::*;
pub use provider::*;
pub use replay_context::*;
pub use rewind::*;
pub use run::*;
pub use stats::*;
pub use trust::*;
