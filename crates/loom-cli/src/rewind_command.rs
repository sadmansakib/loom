//! `loom rewind`: fork a stored session at a chosen transcript boundary
//! (`docs/rewind-design.md`, rollout steps 1–2).
//!
//! Rewind never deletes: it copies items `[0, boundary)` into a fresh
//! session id and leaves the original intact — the transcript is
//! append-only and indices are stable for the session's life (ADR-004),
//! so a fork that truncated its source would break both promises. The
//! fork is pure conversation state: task records, todos, and events
//! belong to the run that produced them.
//!
//! Code restore is tiered by what is provable (§4). Step 2 ships tier
//! 1: `--restore-workspace --to-start` resets the tree to the commit
//! recorded when the run started, which is exact only because the
//! checkpoint was taken on a clean checkout before the task ran. Every
//! other tree state — dirty start, non-git workspace, mid-session
//! boundary — is refused with its reason (tier 0); per-boundary
//! before-images are tier 2. Any restore writes a recovery snapshot
//! first and never garbage-collects it (§5).
//!
//! Without `--at`, the command lists the boundaries where a fork is
//! conversation-valid — before a user message or an assistant turn,
//! never mid-turn — plus the files the dropped region touched, taken
//! from recorded tool effects. With `--at`, it forks and prints the
//! continuation line. Like `loom sessions`, it reads the store
//! directly: no engine open, no provider, no credentials. A session
//! with a live single-writer lease is refused by the store rather than
//! raced.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::kernel::id::SessionId;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::{SessionStore, StartCheckpoint};
use loom_core::kernel::transcript::{Item, ItemIndex};

use crate::args::RewindArgs;

/// One row of the boundary listing: an index where a fork may cut.
struct Boundary {
    index: u32,
    kind: String,
    head: String,
}

/// Lists rewind boundaries for `args.session`, or forks it at `--at`.
pub fn run(args: RewindArgs) -> anyhow::Result<ExitCode> {
    let id: SessionId = args.session.parse().with_context(|| {
        format!(
            "parsing SESSION {:?} (expected an id from `loom sessions`)",
            args.session
        )
    })?;
    let store = loom_core::store::SqliteStore::open(loom_core::paths::db_path())
        .context("opening the session store")?;
    let session = store
        .load(&id)
        .context("loading the session")?
        .with_context(|| format!("unknown session {id} (see `loom sessions`)"))?;
    let boundaries = boundary_rows(&session);
    let tip = session.transcript.len() as u32;
    let checkpoint = store
        .start_checkpoint(&id)
        .context("reading the start checkpoint")?;

    let Some(at) = args.at else {
        print_boundaries(&id, tip, &boundaries, &session, checkpoint.as_ref());
        return Ok(ExitCode::SUCCESS);
    };

    // A mid-turn cut would strand tool calls without results; the store
    // would accept it, but the conversation would be malformed. Only
    // listed boundaries and the tip are valid.
    if at != tip && !boundaries.iter().any(|row| row.index == at) {
        anyhow::bail!(
            "index {at} is not a rewind boundary (it would cut a turn in \
             half); choose an IDX from: loom rewind {id}"
        );
    }
    // Refusal beats best-effort (invariant 2): every tree claim is
    // checked read-only BEFORE the fork runs, so a refused restore
    // leaves nothing half-done. The workspace comes from the
    // checkpoint, not the cwd — a rewind may run from anywhere.
    let restore = if args.restore_workspace {
        let checkpoint = checkpoint.as_ref().with_context(|| {
            format!(
                "--restore-workspace refused: session {id} has no start \
                 checkpoint (it predates the feature or was run with \
                 --no-save); the conversation fork alone is available"
            )
        })?;
        Some(crate::workspace::plan_restore(checkpoint)?)
    } else {
        None
    };
    let new_id = SessionId::new();
    store
        .fork(&id, ItemIndex::from(at), &new_id)
        .context("forking the session")?;
    println!(
        "forked {new_id} from {id}: kept [0, {at}), dropped {} item(s)",
        tip - at
    );
    if let Some(plan) = &restore {
        let patch = crate::workspace::apply_restore(plan)?;
        println!(
            "restored workspace {} to start {} (recovery snapshot: {})",
            plan.workspace.display(),
            plan.head.get(..8).unwrap_or(&plan.head),
            patch.display()
        );
    }
    println!("resume with: loom run --session {new_id} TASK");
    Ok(ExitCode::SUCCESS)
}

/// Walks the transcript, collecting the indices a fork may cut at: every
/// user message (drop that attempt entirely) and every assistant turn
/// (drop from that turn on). Tool results never start a boundary.
fn boundary_rows(session: &Session) -> Vec<Boundary> {
    let mut rows = Vec::new();
    let mut turn = 0;
    for (index, item) in session.transcript.iter().enumerate() {
        match item {
            Item::User { text } => rows.push(Boundary {
                index: index as u32,
                kind: "user".to_owned(),
                head: head_line(text),
            }),
            Item::Assistant(message) => {
                turn += 1;
                let raw = if message.content.trim().is_empty() {
                    format!("[{} tool call(s)]", message.tool_calls.len())
                } else {
                    message.content.clone()
                };
                rows.push(Boundary {
                    index: index as u32,
                    kind: format!("turn {turn}"),
                    head: head_line(&raw),
                });
            }
            Item::ToolResult(_) => {}
            // `Item` is non_exhaustive; future kinds start no boundary
            // until proven otherwise.
            _ => {}
        }
    }
    rows
}

/// First non-empty line of `raw`, capped to keep the table readable.
fn head_line(raw: &str) -> String {
    let Some(first) = raw.lines().find(|line| !line.trim().is_empty()) else {
        return "(no text)".to_owned();
    };
    if first.chars().count() > 48 {
        let head: String = first.chars().take(45).collect();
        format!("{head}...")
    } else {
        first.to_owned()
    }
}

/// The distinct files the items from `from` to the tip touched, from
/// recorded effects only — never inferred (ADR-012).
fn files_dropped(session: &Session, from: u32) -> String {
    let mut files: Vec<&str> = Vec::new();
    for item in &session.transcript[from as usize..] {
        if let Item::ToolResult(result) = item {
            for path in result
                .effects
                .files_written
                .iter()
                .chain(&result.effects.files_deleted)
            {
                if !files.contains(&path.as_str()) {
                    files.push(path);
                }
            }
        }
    }
    match files.len() {
        0 => "none".to_owned(),
        n if n <= 3 => files.join(", "),
        n => format!("{} (+{} more)", files[..3].join(", "), n - 3),
    }
}

fn print_boundaries(
    id: &SessionId,
    tip: u32,
    rows: &[Boundary],
    session: &Session,
    checkpoint: Option<&StartCheckpoint>,
) {
    println!("session {id}: {tip} item(s)");
    // The restore line says what is provable for THIS session's tree,
    // so the user picks a boundary knowing what --restore-workspace
    // can and cannot do (tier 0 names the reason).
    let restore = match checkpoint {
        None => "unavailable: no start checkpoint recorded".to_owned(),
        Some(checkpoint) => match &checkpoint.head {
            Some(head) => format!(
                "to-start {} ({})",
                head.get(..8).unwrap_or(head),
                checkpoint.workspace.display()
            ),
            None => format!(
                "unavailable: {}",
                checkpoint.reason.as_deref().unwrap_or("no checkpoint head")
            ),
        },
    };
    println!("workspace restore: {restore}");
    println!(
        "{:<5} {:<7} {:<34} {:>5}  FILES DROPPED",
        "IDX", "KIND", "HEAD", "DROPS"
    );
    for row in rows {
        println!(
            "{:<5} {:<7} {:<34} {:>5}  {}",
            row.index,
            row.kind,
            row.head,
            tip - row.index,
            files_dropped(session, row.index),
        );
    }
    println!(
        "{:<5} {:<7} {:<34} {:>5}",
        tip, "tip", "(fork without dropping)", 0
    );
    println!("fork with: loom rewind {id} --at <IDX>");
}
