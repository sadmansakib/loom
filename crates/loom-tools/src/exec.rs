//! Bounded external-process execution shared by the exec-style tools (2F).
//!
//! Extracted from `run_command` so `verify` runs commands through exactly the
//! same bounded, deadlock-free path: stdin closed, both pipes drained
//! concurrently, each stream keeping a bounded head plus tail (errors live at
//! the tail; a marker names what was dropped), and the whole **process
//! group** killed if the future is dropped, so a command that spawned
//! children cannot leave orphans holding the pipes. Bytes past the kept
//! windows are still read and discarded, so the child never blocks writing
//! to a full pipe. A non-zero exit status is **not** an error here — the
//! caller decides what a failing status means, so the model can always see
//! the failure.

use std::collections::VecDeque;
use std::process::Stdio;

use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
use loom_kernel::tool::{ToolContext, ToolError};
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use tokio::io::AsyncReadExt;

/// Leading bytes kept per stream.
const STREAM_HEAD_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Trailing bytes kept per stream: the band of a log where a failing
/// command's diagnosis usually lands.
const STREAM_TAIL_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Per-command execution limits from the command policy.
///
/// `None` fields keep the shipped constants; a rule that sets them
/// tightens nothing else. The credential floor
/// ([`SCRUB_FLOOR`](crate::policy::SCRUB_FLOOR)) always applies on top
/// of `scrub` — a caller cannot widen it.
#[derive(Default)]
pub(crate) struct ExecLimits {
    /// Per-stream head/tail window override in bytes.
    pub stream_window: Option<usize>,
    /// Extra environment names removed from the child.
    pub scrub: Vec<String>,
}

/// Kills the whole process group when dropped while still armed.
///
/// process-wrap's tokio `KillOnDrop` shim only sets tokio's own
/// kill-on-drop flag, which signals the direct child pid — grandchildren
/// survive it. The child is spawned as its own group leader
/// ([`ProcessGroup::leader`]), so one best-effort `killpg` reaps the
/// tree on the cancellation path. Disarmed on normal completion: a
/// reaped pgid could be reused by an unrelated process.
struct GroupKillOnDrop {
    pgid: i32,
    armed: bool,
}

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: cancellation must not wait on the doomed tree.
            let pgid = nix::unistd::Pid::from_raw(self.pgid);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// Runs `program` with `args` in the workspace root, capturing bounded stdout
/// and stderr plus the exit status.
///
/// Mirrors the `run_command` execution contract: bare program name, no shell,
/// workspace-root working directory, cancellation by dropping the future.
/// The child leads its own process group and the group is killed on
/// drop, so a timeout or cancel reaps grandchildren too — with tokio's
/// plain `kill_on_drop` only the direct child died (documented debt,
/// now closed).
///
/// `sandbox` confines the command at the OS level when its grants
/// enable it; the wrapper leads the process group, so the group-kill
/// and pipe-drain contract covers everything the command spawns.
pub(crate) async fn run_bounded(
    program: &str,
    args: &[String],
    ctx: &ToolContext,
    limits: &ExecLimits,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<std::process::Output, ToolError> {
    let window = limits.stream_window;
    let head_cap = window.unwrap_or(STREAM_HEAD_BYTES);
    let tail_cap = window.unwrap_or(STREAM_TAIL_BYTES);
    let (wrapper, wrapped_args) = sandbox.wrap(program, args);
    let mut child = CommandWrap::with_new(wrapper, |command| {
        command
            .args(&wrapped_args)
            .current_dir(&ctx.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The credential floor plus the rule's extra names; the child
        // never sees Loom's own keys (command execution policy).
        for name in crate::policy::SCRUB_FLOOR
            .iter()
            .copied()
            .chain(limits.scrub.iter().map(String::as_str))
        {
            command.env_remove(name);
        }
    })
    .wrap(ProcessGroup::leader())
    .spawn()
    .map_err(|error| ToolError::Execution(format!("cannot run \"{program}\": {error}")))?;
    let mut guard = match child.id() {
        Some(pid) => GroupKillOnDrop {
            pgid: i32::try_from(pid).unwrap_or(-1),
            armed: true,
        },
        None => GroupKillOnDrop {
            pgid: -1,
            armed: false,
        },
    };
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| ToolError::Execution(String::from("cannot capture stdout pipe")))?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| ToolError::Execution(String::from("cannot capture stderr pipe")))?;
    let (stdout, stderr) = futures::join!(
        read_capped(stdout, head_cap, tail_cap),
        read_capped(stderr, head_cap, tail_cap)
    );
    let stdout =
        stdout.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    let stderr =
        stderr.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    let status = child
        .wait()
        .await
        .map_err(|error| ToolError::Execution(format!("cannot run \"{program}\": {error}")))?;
    guard.armed = false;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Reads a child stream to EOF, keeping the first `head_cap` and the
/// last `tail_cap` bytes with an explicit `... [N bytes omitted]`
/// marker naming the dropped middle, so the child cannot block writing to a
/// full pipe and the tail (where errors land) survives.
async fn read_capped<R>(mut reader: R, head_cap: usize, tail_cap: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::with_capacity(tail_cap);
    let mut chunk = [0_u8; 8192];
    let mut total: u64 = 0;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total += read as u64;
        let data = &chunk[..read];
        if head.len() < head_cap {
            let room = head_cap - head.len();
            let take = room.min(read);
            head.extend_from_slice(&data[..take]);
            tail.extend(&data[take..]);
        } else {
            tail.extend(data);
        }
        if tail.len() > tail_cap {
            tail.drain(..tail.len() - tail_cap);
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
        assert_eq!(
            read_capped(reader, STREAM_HEAD_BYTES, STREAM_TAIL_BYTES)
                .await
                .unwrap(),
            b"hello"
        );
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
        let kept = read_capped(reader, STREAM_HEAD_BYTES, STREAM_TAIL_BYTES)
            .await
            .unwrap();
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
        let kept = read_capped(reader, STREAM_HEAD_BYTES, STREAM_TAIL_BYTES)
            .await
            .unwrap();
        producer.await.unwrap();
        let payload = vec![b'x'; STREAM_HEAD_BYTES - 1];
        assert_eq!(kept, payload);
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_group() {
        use loom_kernel::tool::ToolContext;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        // A unique marker so pgrep cannot collide with anything else on the
        // machine. The `&` subshell stays a `sh` whose argv embeds the
        // marker (an `exec` would drop it), and the parent `exec`s a sleep
        // so both hold the stdout pipe open — the read side only ends when
        // the whole group is killed.
        const MARKER: &str = "loom-grp-9873";
        // Leftovers from an earlier failed run of this very test would
        // satisfy the liveness probe and mask the behaviour under test.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(MARKER)
            .output();
        let root = std::env::temp_dir().join("loom-exec-grp-test");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolContext::new(root, CancellationToken::new(), Duration::from_secs(120));
        let script = format!("while :; do sleep 0.5; done & exec sleep 3000 # {MARKER}");
        let args = vec!["-c".to_owned(), script];

        // Cancelling the task drops the future mid-run — the same
        // cancellation path a timeout or Ctrl-C takes in production.
        let sandbox = crate::sandbox::Sandbox::resolve(
            &ctx.workspace_root,
            crate::sandbox::SandboxGrants::from_rule(None),
        );
        let task = tokio::spawn(async move {
            run_bounded("sh", &args, &ctx, &ExecLimits::default(), &sandbox).await
        });
        let alive = || {
            std::process::Command::new("pgrep")
                .arg("-f")
                .arg(MARKER)
                .output()
                .map(|out| out.status.success() && !out.stdout.is_empty())
                .unwrap_or(false)
        };
        // Hold the future briefly so the group is up, then drop it: the
        // group must die, orphans included.
        let mut up = false;
        for _ in 0..50 {
            if alive() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(up, "test setup: the process group never came up");

        task.abort();

        let mut dead = false;
        for _ in 0..50 {
            if !alive() {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(dead, "dropping the future must kill grandchildren too");
    }
}
