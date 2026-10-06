//! Bounded external-process execution shared by the exec-style tools (2F).
//!
//! Extracted from `run_command` so `verify` runs commands through exactly the
//! same bounded, deadlock-free path: stdin closed, both pipes drained
//! concurrently, each stream capped at
//! [`MAX_TOOL_OUTPUT_BYTES`](loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES),
//! and the child killed if the future is dropped. A non-zero exit status is
//! **not** an error here — the caller decides what a failing status means, so
//! the model can always see the failure.

use std::process::Stdio;

use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
use loom_kernel::tool::{ToolContext, ToolError};
use tokio::io::AsyncReadExt;

/// Upper bound kept per stream; the remainder is read and discarded so the
/// child never blocks writing to a full pipe.
const STREAM_BUFFER_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Runs `program` with `args` in the workspace root, capturing bounded stdout
/// and stderr plus the exit status.
///
/// Mirrors the `run_command` execution contract: bare program name, no shell,
/// workspace-root working directory, cancellation by dropping the future.
pub(crate) async fn run_bounded(
    program: &str,
    args: &[String],
    ctx: &ToolContext,
) -> Result<std::process::Output, ToolError> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(&ctx.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ToolError::Execution(format!("cannot run \"{program}\": {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution(String::from("cannot capture stdout pipe")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution(String::from("cannot capture stderr pipe")))?;
    let (stdout, stderr) = futures::join!(read_capped(stdout), read_capped(stderr));
    let stdout =
        stdout.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    let stderr =
        stderr.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    let status = child
        .wait()
        .await
        .map_err(|error| ToolError::Execution(format!("cannot run \"{program}\": {error}")))?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Reads a child stream to EOF, keeping at most [`STREAM_BUFFER_BYTES`] and
/// discarding the rest so the child cannot block writing to a full pipe.
async fn read_capped<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if kept.len() < STREAM_BUFFER_BYTES {
            let room = STREAM_BUFFER_BYTES - kept.len();
            kept.extend_from_slice(&chunk[..room.min(read)]);
        }
    }
    Ok(kept)
}
