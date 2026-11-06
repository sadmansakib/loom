//! The `loom` binary: argument parsing, tracing setup, and dispatch.
//!
//! Responsibilities that belong to the process, not the domain: install a
//! stderr tracing subscriber (`RUST_LOG`, default `warn`), keep stdout for
//! results only, and translate the command outcome into a process exit code.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

mod args;
mod auth_command;
mod instructions;
mod lessons_command;
mod output;
mod provider_command;
mod replay_command;
mod rewind_command;
mod run;
mod sessions_command;
mod stats_command;
mod workspace;

use args::{Cli, Command};

fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Run(args) => run::run(*args),
        Command::Auth(args) => auth_command::run(args.command).map(|()| ExitCode::SUCCESS),
        Command::Provider(args) => provider_command::run(args.command).map(|()| ExitCode::SUCCESS),
        Command::Sessions => sessions_command::run(),
        Command::Rewind(args) => rewind_command::run(args),
        Command::Stats(args) => stats_command::run(args),
        Command::Lessons(args) => lessons_command::run(args),
        Command::ReplayContext(args) => replay_command::run(args),
    };

    match outcome {
        Ok(code) => code,
        Err(error) => {
            // `{:#}` prints the whole anyhow context chain on one line.
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Installs the tracing subscriber on stderr (`RUST_LOG`, default `warn`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // A second subscriber in the same process (tests) is not an error here.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
