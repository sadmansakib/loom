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
//! Code restore is tiered by what is provable (§4). Tier 1
//! (`--restore-workspace --to-start`) resets the tree to the commit
//! recorded when the run started, which is exact only because the
//! checkpoint was taken on a clean checkout before the task ran. Tier
//! 2 (`--restore-workspace --at IDX`) restores to any boundary from
//! the pre-images tools captured as they wrote — git not required,
//! dirty starts included. Anything neither can prove is refused with
//! its reason (tier 0). Every restore writes a recovery snapshot
//! first and never garbage-collects it (§5). `commands_run` effects
//! are never silently absorbed: they surface and gate the restore
//! behind `--accept-unrestorable`.
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

    // A boundary restore needs a boundary: catch the flag before the
    // listing path below returns, so --restore-workspace is never
    // accepted and silently ignored.
    if args.restore_workspace && !args.to_start && args.at.is_none() {
        anyhow::bail!(
            "--restore-workspace restores to a boundary: pass --at IDX, or \
             use --to-start for the session-start reset"
        );
    }

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
        if args.to_start {
            let checkpoint = checkpoint.as_ref().with_context(|| {
                format!(
                    "--restore-workspace refused: session {id} has no start \
                     checkpoint (it predates the feature or was run with \
                     --no-save); the conversation fork alone is available"
                )
            })?;
            Restore::Start(crate::workspace::plan_restore(checkpoint)?)
        } else {
            // Tier 2: restore to the fork boundary itself. Only the
            // checkpoint's workspace location is needed — the claims
            // come from recorded effects, so a dirty or non-git
            // workspace restores exactly like a clean one.
            let at = args.at.with_context(|| {
                "--restore-workspace restores to a boundary: pass --at IDX \
                 from this listing (or --to-start for the session-start \
                 reset)"
            })?;
            let checkpoint = checkpoint.as_ref().with_context(|| {
                format!(
                    "--restore-workspace refused: session {id} has no start \
                     checkpoint (it predates the feature or was run with \
                     --no-save), so the workspace it ran in is unknown; the \
                     conversation fork alone is available"
                )
            })?;
            let plan =
                crate::workspace::plan_boundary_restore(&session, at, &checkpoint.workspace)?;
            // Commands are effects Loom cannot undo; they gate rather
            // than silently absorb (invariant 6).
            if !plan.commands.is_empty() && !args.accept_unrestorable {
                anyhow::bail!(
                    "the dropped region ran {} command(s) Loom cannot undo \
                     ({}); pass --accept-unrestorable to restore the provable \
                     file changes anyway",
                    plan.commands.len(),
                    summarize_commands(&plan.commands)
                );
            }
            Restore::Boundary(plan)
        }
    } else {
        Restore::None
    };
    let new_id = SessionId::new();
    store
        .fork(&id, ItemIndex::from(at), &new_id)
        .context("forking the session")?;
    println!(
        "forked {new_id} from {id}: kept [0, {at}), dropped {} item(s)",
        tip - at
    );
    match &restore {
        Restore::None => {}
        Restore::Start(plan) => {
            let patch = crate::workspace::apply_restore(plan)?;
            println!(
                "restored workspace {} to start {} (recovery snapshot: {})",
                plan.workspace.display(),
                plan.head.get(..8).unwrap_or(&plan.head),
                patch.display()
            );
        }
        Restore::Boundary(plan) => {
            let snapshot = crate::workspace::apply_boundary_restore(plan, &store)?;
            let restored = plan
                .entries
                .iter()
                .filter(|entry| entry.content.is_some())
                .count();
            let deleted = plan.entries.len() - restored;
            println!(
                "restored workspace {} to boundary {at}: {restored} file(s) \
                 rewritten, {deleted} deleted (recovery snapshot: {})",
                plan.workspace.display(),
                snapshot.display()
            );
            if !plan.commands.is_empty() {
                // Accepted, not absorbed: the user sees what stays.
                println!(
                    "accepted as unrestorable: {}",
                    summarize_commands(&plan.commands)
                );
            }
        }
    }
    println!("resume with: loom run --session {new_id} TASK");
    Ok(ExitCode::SUCCESS)
}

/// A workspace restore this rewind will apply after the fork.
enum Restore {
    None,
    /// Tier 1: git reset to the session's start commit.
    Start(crate::workspace::RestorePlan),
    /// Tier 2: per-path pre-images at the fork boundary.
    Boundary(crate::workspace::BoundaryRestorePlan),
}

/// Up to three commands, `program args` joined, for refusal and
/// acceptance lines. Bounded like `files_dropped` keeps its column.
fn summarize_commands(commands: &[loom_core::kernel::tool::CommandEffect]) -> String {
    fn render(command: &loom_core::kernel::tool::CommandEffect) -> String {
        if command.args.is_empty() {
            command.program.clone()
        } else {
            format!("{} {}", command.program, command.args.join(" "))
        }
    }
    match commands.len() {
        0 => "none".to_owned(),
        n if n <= 3 => {
            let parts: Vec<String> = commands.iter().map(render).collect();
            parts.join("; ")
        }
        n => {
            let parts: Vec<String> = commands.iter().take(3).map(render).collect();
            format!("{} (+{} more)", parts.join("; "), n - 3)
        }
    }
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
                "to-start {} ({}); to a boundary: --at IDX (pre-images)",
                head.get(..8).unwrap_or(head),
                checkpoint.workspace.display()
            ),
            None => format!(
                "to-start unavailable ({}); to a boundary: --at IDX \
                 (pre-images, git not required)",
                checkpoint.reason.as_deref().unwrap_or("no checkpoint head")
            ),
        },
    };
    println!("workspace restore: {restore}");
    println!(
        "{:<5} {:<7} {:<34} {:>5}  {:<8}  FILES DROPPED",
        "IDX", "KIND", "HEAD", "DROPS", "RESTORE"
    );
    for row in rows {
        println!(
            "{:<5} {:<7} {:<34} {:>5}  {:<8}  {}",
            row.index,
            row.kind,
            row.head,
            tip - row.index,
            restore_note(session, row.index),
            files_dropped(session, row.index),
        );
    }
    println!(
        "{:<5} {:<7} {:<34} {:>5}  {:<8}",
        tip, "tip", "(fork without dropping)", 0, "-"
    );
    println!("fork with: loom rewind {id} --at <IDX>");
    println!("fork + restore the tree: add --restore-workspace");
}

/// The RESTORE column: what a tier-2 restore of this boundary can
/// prove from effects alone. `yes` means every first-touched path has
/// a pre-image; commands are unrestorable and gate the restore (they
/// name themselves when present, and `--accept-unrestorable` is the
/// escape hatch).
fn restore_note(session: &Session, from: u32) -> String {
    let summary = crate::workspace::boundary_restore_summary(session, from);
    if summary.commands > 0 {
        return format!("cmds({})", summary.commands);
    }
    if summary.provable {
        "yes".to_owned()
    } else {
        "no".to_owned()
    }
}
