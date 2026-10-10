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
//!
//! A command that *leaks* a descendant (a test spawning a background
//! process, then exiting itself) would otherwise pin the collection open
//! forever: the descendant inherited the pipe write-ends, so EOF never
//! arrives even though the direct child is gone. After the direct child
//! exits, the readers get a short grace window; if the pipes are still
//! held, the process group is reaped (the group belongs to this command),
//! and a descendant that escaped reaping (it called `setsid`) gets a final
//! bounded wait before the stream is returned truncated with an explicit
//! marker — never a hang, and the exit status always reaches the model.

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

/// Grace after the direct child exits before a still-open pipe is treated
/// as a leaked descendant holding the write-end. Normal commands reach EOF
/// within microseconds of exit; anything still open after this window
/// never will.
const STREAM_EOF_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Final wait for EOF after the process group has been reaped. SIGKILL
/// closes pipes immediately; this only bounds the pathological case of a
/// descendant that escaped both the group and the kill.
const STREAM_FORCED_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Marker returned for a stream whose reader never reached EOF because a
/// descendant process held the pipe past every reap attempt.
const HELD_PIPE_MARKER: &str =
    "[loom: stream truncated — a descendant process held the pipe after command exit]";

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
/// A bounded execution's result plus the harness facts a raw
/// `std::process::Output` cannot carry: whether an unreapable
/// descendant forced a stream to be abandoned (the marker text alone
/// does not distinguish causes for the trace).
pub(crate) struct ExecOutcome {
    /// The child's exit status and both streams (possibly capped, or
    /// carrying the held-pipe marker).
    pub output: std::process::Output,
    /// A descendant escaped the process group and held the pipes past
    /// the forced grace: both streams were abandoned to it.
    pub pipe_hold_truncated: bool,
}

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
///
/// A command whose descendant holds the pipes past every grace window
/// still returns — its streams carry the held-pipe marker and
/// [`ExecOutcome::pipe_hold_truncated`] is set.
pub(crate) async fn run_bounded(
    program: &str,
    args: &[String],
    ctx: &ToolContext,
    limits: &ExecLimits,
    sandbox: &crate::sandbox::Sandbox,
) -> Result<ExecOutcome, ToolError> {
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
    let mut stdout_task = tokio::spawn(read_capped(stdout, head_cap, tail_cap));
    let mut stderr_task = tokio::spawn(read_capped(stderr, head_cap, tail_cap));
    // The direct child first: its exit status is the contract; the streams
    // drain concurrently so the child cannot block on a full pipe.
    let status = child
        .wait()
        .await
        .map_err(|error| ToolError::Execution(format!("cannot run \"{program}\": {error}")))?;

    // The child has exited. Normal commands close their pipes with it and
    // both readers finish within microseconds. A leaked descendant holding
    // an inherited write-end keeps EOF away — grace, then reap the group
    // (it belongs to this command), then one final bounded wait for a
    // holder that escaped the group entirely. Never a hang; the status is
    // already in hand.
    async fn join_readers(
        a: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
        b: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    ) -> (std::io::Result<Vec<u8>>, std::io::Result<Vec<u8>>) {
        let (stdout, stderr) = futures::future::join(&mut *a, &mut *b).await;
        (flatten_read(stdout), flatten_read(stderr))
    }
    let (stdout, stderr): (std::io::Result<Vec<u8>>, std::io::Result<Vec<u8>>);
    let mut pipe_hold_truncated = false;
    match tokio::time::timeout(
        STREAM_EOF_GRACE,
        join_readers(&mut stdout_task, &mut stderr_task),
    )
    .await
    {
        Ok((out, err)) => {
            stdout = out;
            stderr = err;
        }
        Err(_elapsed) => {
            // Pipes still held: reap the process group. If the group is
            // gone (whole tree dead), the pipes would have closed — so
            // the pgid is still live and not reused.
            let pgid = nix::unistd::Pid::from_raw(guard.pgid);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            match tokio::time::timeout(
                STREAM_FORCED_GRACE,
                join_readers(&mut stdout_task, &mut stderr_task),
            )
            .await
            {
                Ok((out, err)) => {
                    stdout = out;
                    stderr = err;
                }
                Err(_elapsed) => {
                    // A holder escaped the group (setsid). Detach the
                    // readers — dropping the handles lets them finish
                    // quietly whenever the holder dies, their buffers
                    // already bounded — and report truncation.
                    drop(stdout_task);
                    drop(stderr_task);
                    pipe_hold_truncated = true;
                    stdout = Ok(held_pipe_marker_bytes());
                    stderr = Ok(held_pipe_marker_bytes());
                }
            }
        }
    }
    guard.armed = false;
    let stdout =
        stdout.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    let stderr =
        stderr.map_err(|error| ToolError::Execution(format!("cannot read output: {error}")))?;
    Ok(ExecOutcome {
        output: std::process::Output {
            status,
            stdout,
            stderr,
        },
        pipe_hold_truncated,
    })
}

/// Unwraps a reader task's result, mapping a panic or an I/O failure to
/// the same execution-error shape the old inline reads produced.
fn flatten_read(
    result: Result<std::io::Result<Vec<u8>>, tokio::task::JoinError>,
) -> std::io::Result<Vec<u8>> {
    match result {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "output reader task failed: {error}"
        ))),
    }
}

/// The truncation marker for a stream abandoned to an unreapable holder.
fn held_pipe_marker_bytes() -> Vec<u8> {
    format!("{HELD_PIPE_MARKER}\n").into_bytes()
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

    #[tokio::test]
    async fn a_leaked_descendant_holding_the_pipes_is_reaped() {
        use loom_kernel::tool::ToolContext;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        // The command exits at once but its background child inherits the
        // output pipes and lives on — the exact shape of a leaking test.
        // Collection must still finish: grace, group reap, EOF. The odd
        // duration doubles as the cleanup marker: it rides the holder's
        // argv (a `# comment` would swallow the `&`).
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg("sleep 29991")
            .output();
        let root = std::env::temp_dir().join("loom-exec-hold-test");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolContext::new(root, CancellationToken::new(), Duration::from_secs(120));
        let sandbox = crate::sandbox::Sandbox::resolve(
            &ctx.workspace_root,
            crate::sandbox::SandboxGrants::from_rule(None),
        );
        let args = vec![
            "-c".to_owned(),
            String::from("sleep 29991 &\nexec /usr/bin/true"),
        ];

        let started = tokio::time::Instant::now();
        let outcome = run_bounded("sh", &args, &ctx, &ExecLimits::default(), &sandbox)
            .await
            .expect("bounded run despite pipe-holding descendant");
        // Grace (2s) + reap + drain — anything near the forced window means
        // the reap path regressed; a hang would never return at all.
        assert!(
            started.elapsed() < STREAM_EOF_GRACE + STREAM_FORCED_GRACE,
            "collection should finish shortly after the grace window"
        );
        assert!(outcome.output.status.success());
        // The reap closed the pipes, so EOF arrived naturally: the stream is
        // whole (empty), not marked truncated.
        assert!(outcome.output.stdout.is_empty());
        assert!(outcome.output.stderr.is_empty());
        assert!(
            !outcome.pipe_hold_truncated,
            "a reaped group is not an escaped holder"
        );
        // Belt and braces: nothing from this test outlives it.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg("sleep 29991")
            .output();
    }

    #[tokio::test]
    async fn an_escaped_descendant_marks_the_stream_truncated() {
        use loom_kernel::tool::ToolContext;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        // A descendant that starts its own session survives the group reap
        // and keeps holding the pipes; collection must give up with the
        // truncation marker rather than hang. Perl's setsid is the portable
        // way to escape the process group on both macOS and Linux.
        const MARKER: &str = "loom-esc-7731";
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(MARKER)
            .output();
        let root = std::env::temp_dir().join("loom-exec-esc-test");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolContext::new(root, CancellationToken::new(), Duration::from_secs(120));
        let sandbox = crate::sandbox::Sandbox::resolve(
            &ctx.workspace_root,
            crate::sandbox::SandboxGrants::from_rule(None),
        );
        let args = vec![
            "-c".to_owned(),
            format!(
                "perl -e 'use POSIX qw(setsid); setsid; $0 = \"{MARKER}\"; sleep 3000' & \
                 exec /usr/bin/true"
            ),
        ];

        let started = tokio::time::Instant::now();
        let outcome = run_bounded("sh", &args, &ctx, &ExecLimits::default(), &sandbox)
            .await
            .expect("bounded run despite escaped descendant");
        assert!(
            started.elapsed() < STREAM_EOF_GRACE + STREAM_FORCED_GRACE + Duration::from_secs(3),
            "both grace windows must bound collection"
        );
        assert!(outcome.output.status.success());
        assert!(
            outcome.pipe_hold_truncated,
            "an escaped holder must flag the truncation cause"
        );
        let stdout = String::from_utf8_lossy(&outcome.output.stdout);
        assert!(
            stdout.contains(HELD_PIPE_MARKER),
            "stream must be marked truncated, got: {stdout:?}"
        );
        // Do not leak the escaped sleeper into later tests.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(MARKER)
            .output();
    }
}
