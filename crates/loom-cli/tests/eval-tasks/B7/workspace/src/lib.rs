//! Run a command and capture its output.
use std::process::Stdio;
use tokio::process::Command;

/// Spawns `program`, captures stdout/stderr, and awaits completion.
pub async fn run(program: &str, args: &[String]) -> std::io::Result<std::process::Output> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child.wait_with_output().await
}
