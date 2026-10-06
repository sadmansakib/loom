//! Bounded external-process execution shared by the exec-style tools (2F).
//!
//! Extracted from `run_command` so `verify` runs commands through exactly the
//! same bounded, deadlock-free path: stdin closed, both pipes drained
//! concurrently, each stream keeping a bounded head plus tail (errors live at
//! the tail; a marker names what was dropped), and the child killed if the
//! future is dropped. Bytes past the kept windows are still read and
//! discarded, so the child never blocks writing to a full pipe. A non-zero
//! exit status is **not** an error here — the caller decides what a failing
//! status means, so the model can always see the failure.

use std::collections::VecDeque;
use std::process::Stdio;

use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
use loom_kernel::tool::{ToolContext, ToolError};
use tokio::io::AsyncReadExt;

/// Leading bytes kept per stream.
const STREAM_HEAD_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Trailing bytes kept per stream: the band of a log where a failing
/// command's diagnosis usually lands.
const STREAM_TAIL_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

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

/// Reads a child stream to EOF, keeping the first [`STREAM_HEAD_BYTES`] and
/// the last [`STREAM_TAIL_BYTES`] with an explicit `... [N bytes omitted]`
/// marker naming the dropped middle, so the child cannot block writing to a
/// full pipe and the tail (where errors land) survives.
async fn read_capped<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::with_capacity(STREAM_TAIL_BYTES);
    let mut chunk = [0_u8; 8192];
    let mut total: u64 = 0;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total += read as u64;
        let data = &chunk[..read];
        if head.len() < STREAM_HEAD_BYTES {
            let room = STREAM_HEAD_BYTES - head.len();
            let take = room.min(read);
            head.extend_from_slice(&data[..take]);
            tail.extend(&data[take..]);
        } else {
            tail.extend(data);
        }
        if tail.len() > STREAM_TAIL_BYTES {
            tail.drain(..tail.len() - STREAM_TAIL_BYTES);
        }
    }
    let kept = head.len() + tail.len();
    if u64::try_from(kept).unwrap_or(u64::MAX) < total {
        head.extend_from_slice(
            format!("... [{} bytes omitted] ...\n", total - kept as u64).as_bytes(),
        );
        head.extend(tail);
    }
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn small_stream_passes_through_whole() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"hello").await.unwrap();
        drop(writer);
        assert_eq!(read_capped(reader).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn oversized_stream_keeps_head_tail_and_marker() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        // Produce concurrently: the duplex buffer is tiny, so a sequential
        // write-then-read would deadlock on a full pipe.
        let producer = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
        });
        let kept = read_capped(reader).await.unwrap();
        producer.await.unwrap();
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        let marker = b"... [";
        let cut = kept
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("omission marker present");
        let (head_part, rest) = kept.split_at(cut);
        assert_eq!(head_part, &payload[..head_part.len()]);
        assert!(head_part.len() <= STREAM_HEAD_BYTES);
        // The marker names exactly the dropped middle…
        let marker_end = rest
            .iter()
            .position(|&b| b == b'\n')
            .expect("marker ends with newline");
        let marker_text = String::from_utf8_lossy(&rest[..marker_end]).into_owned();
        let omitted = 300_000 - head_part.len() - STREAM_TAIL_BYTES;
        assert_eq!(marker_text, format!("... [{omitted} bytes omitted] ..."));
        // …and the tail matches the end of the payload.
        let tail_part = &rest[marker_end + 1..];
        assert_eq!(tail_part, &payload[300_000 - STREAM_TAIL_BYTES..]);
    }

    #[tokio::test]
    async fn stream_under_the_windows_keeps_nothing_dropped() {
        // Just under the head window: nothing to omit, everything kept.
        let (mut writer, reader) = tokio::io::duplex(64);
        let payload = vec![b'x'; STREAM_HEAD_BYTES - 1];
        let producer = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
        });
        let kept = read_capped(reader).await.unwrap();
        producer.await.unwrap();
        let payload = vec![b'x'; STREAM_HEAD_BYTES - 1];
        assert_eq!(kept, payload);
    }
}
