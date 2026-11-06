//! Workspace snapshots for the rewind surface (`docs/rewind-design.md`
//! §4 tiers 1–2 and §5 safety rule).
//!
//! Three operations, all pure process concerns (the engine never touches
//! git directly outside worktrees; this is the CLI adapter's seam):
//!
//! - [`snapshot_start`] runs at `loom run` start, before the task can
//!   touch the tree, and captures the workspace's git HEAD when — and
//!   only when — the checkout was clean. A dirty or non-git start is
//!   recorded as such, so a later restore refuses with the real reason
//!   instead of guessing (tier 0).
//! - [`plan_restore`] / [`apply_restore`] turn a recorded
//!   [`StartCheckpoint`] back into tree state (tier 1: `git reset` to
//!   the clean start). Refusal beats best-effort: every claim the
//!   restore makes is checked first, and the mutation writes a recovery
//!   snapshot before it resets anything (invariant 2).
//! - [`plan_boundary_restore`] / [`apply_boundary_restore`] restore to
//!   any transcript boundary from the per-turn pre-images tools capture
//!   at execution time (tier 2). No git is involved: every claim is a
//!   recorded effect, so the same restore works on a dirty checkout or
//!   a workspace with no repository at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use loom_core::kernel::artifact::ArtifactRef;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::{ArtifactStore, StartCheckpoint};
use loom_core::kernel::tool::{BeforeImage, CommandEffect};
use loom_core::kernel::transcript::Item;

/// Runs `git -C <root> <args>`, returning trimmed stdout.
///
/// The error message is git's stderr when it has one — the caller
/// surfaces it, and a git that is missing or unhappy should never be
/// papered over here.
fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("running git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or_default())
        } else {
            stderr
        })
    }
}

/// Captures the workspace state for a session-start checkpoint.
///
/// Clean checkout → `head` set, the exact tier-1 restore point. Dirty
/// or non-git → `head` absent with the reason; git itself being broken
/// or missing also degrades to a reason (the run proceeds, a later
/// `--restore-workspace` refuses honestly).
pub fn snapshot_start(workspace: &Path) -> StartCheckpoint {
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let inside = run_git(&canonical, &["rev-parse", "--is-inside-work-tree"]);
    let inside = matches!(inside.as_deref(), Ok("true"));
    if !inside {
        return StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some("workspace is not a git repository".to_owned()),
        };
    }
    let head = match run_git(&canonical, &["rev-parse", "--verify", "HEAD"]) {
        Ok(head) if !head.is_empty() => head,
        Ok(_) => {
            return StartCheckpoint {
                workspace: canonical,
                head: None,
                reason: Some("git repository has no commits".to_owned()),
            };
        }
        Err(error) => {
            return StartCheckpoint {
                workspace: canonical,
                head: None,
                reason: Some(format!("reading git HEAD failed: {error}")),
            };
        }
    };
    match run_git(&canonical, &["status", "--porcelain"]) {
        Ok(status) if status.is_empty() => StartCheckpoint {
            workspace: canonical,
            head: Some(head),
            reason: None,
        },
        // One line per change; the count keeps the refusal concrete
        // without dumping paths into a meta row.
        Ok(status) => StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some(format!(
                "workspace was dirty at session start ({} changed path{})",
                status.lines().count(),
                if status.lines().count() == 1 { "" } else { "s" }
            )),
        },
        Err(error) => StartCheckpoint {
            workspace: canonical,
            head: None,
            reason: Some(format!("reading git status failed: {error}")),
        },
    }
}

/// A restore that passed every provability check.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// The recorded workspace (from the checkpoint, not the caller's
    /// cwd — a rewind may run from anywhere).
    pub workspace: PathBuf,
    /// The commit `git reset --hard` will target.
    pub head: String,
}

/// Checks everything a tier-1 restore claims before touching anything
/// (invariant 2: refusal beats best-effort).
///
/// Refuses, with the checkpoint's own reason, when no restore point was
/// ever provable. Refuses when the recorded workspace no longer exists,
/// is no longer a git work tree, or the recorded commit is no longer
/// present locally (reset cannot invent objects).
pub fn plan_restore(checkpoint: &StartCheckpoint) -> Result<RestorePlan> {
    let Some(head) = checkpoint.head.clone() else {
        bail!(
            "no provable workspace state to restore: {}",
            checkpoint
                .reason
                .as_deref()
                .unwrap_or("no start checkpoint head")
        );
    };
    if !checkpoint.workspace.is_dir() {
        bail!(
            "recorded workspace {} no longer exists",
            checkpoint.workspace.display()
        );
    }
    if !matches!(
        run_git(
            &checkpoint.workspace,
            &["rev-parse", "--is-inside-work-tree"]
        )
        .as_deref(),
        Ok("true")
    ) {
        bail!(
            "recorded workspace {} is no longer a git work tree",
            checkpoint.workspace.display()
        );
    }
    if let Err(error) = run_git(
        &checkpoint.workspace,
        &["cat-file", "-e", &format!("{head}^{{commit}}")],
    ) {
        bail!(
            "start commit {head} is not available in {} ({error}); \
             it may have been garbage-collected",
            checkpoint.workspace.display()
        );
    }
    Ok(RestorePlan {
        workspace: checkpoint.workspace.clone(),
        head,
    })
}

/// Writes the pre-restore recovery snapshot and applies the plan:
/// `git reset --hard <head>` + `git clean -fd`, excluding `.loom` so
/// the snapshot itself survives the clean.
///
/// The snapshot stages everything (`git add -A`) and writes `git diff
/// --cached` to `<workspace>/.loom/rewind/<millis>.patch` — the full
/// content of what the restore is about to discard, kept indefinitely
/// (never garbage-collected with run artifacts). Returns its path.
pub fn apply_restore(plan: &RestorePlan) -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let rewind_dir = plan.workspace.join(".loom").join("rewind");
    std::fs::create_dir_all(&rewind_dir)
        .with_context(|| format!("creating {}", rewind_dir.display()))?;
    let patch = rewind_dir.join(format!("{timestamp}.patch"));

    run_git(&plan.workspace, &["add", "-A"])
        .map_err(|error| anyhow::anyhow!("staging for the recovery snapshot: {error}"))?;
    let diff = match run_git(&plan.workspace, &["diff", "--cached"]) {
        Ok(diff) => diff,
        // Staging happened; undo it so a failed snapshot leaves the
        // index as it was (reset --hard would rewrite it anyway, but a
        // refusal should not leave side effects).
        Err(error) => {
            let _ = run_git(&plan.workspace, &["reset"]);
            bail!("capturing the recovery snapshot: {error}");
        }
    };
    std::fs::write(&patch, &diff).with_context(|| format!("writing {}", patch.display()))?;

    if let Err(error) = run_git(&plan.workspace, &["reset", "--hard", &plan.head]) {
        let _ = run_git(&plan.workspace, &["reset"]);
        bail!(
            "resetting {} to {} failed after the recovery snapshot at {} was written: {error}",
            plan.workspace.display(),
            plan.head,
            patch.display()
        );
    }
    if let Err(error) = run_git(&plan.workspace, &["clean", "-fd", "-e", ".loom"]) {
        bail!(
            "tree was reset to {} but cleaning untracked files failed: {error} \
             (recovery snapshot at {})",
            plan.head,
            patch.display()
        );
    }
    Ok(patch)
}

/// One path's restore action at a boundary: write the captured
/// pre-image body back, or delete the file because it did not exist
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryEntry {
    /// Workspace-relative, exactly as the recording effect spelled it.
    pub path: String,
    /// The captured pre-image, or `None` when the path did not exist
    /// at the boundary — the restore then deletes it.
    pub content: Option<ArtifactRef>,
}

/// A tier-2 boundary restore that passed every provability check.
///
/// Built read-only from recorded effects; the mutation happens only in
/// [`apply_boundary_restore`], after a recovery snapshot. `commands`
/// carries every command the dropped region ran — effects Loom cannot
/// undo — for the caller to surface and gate on (invariant 6: never
/// silently absorbed).
#[derive(Debug, Clone)]
pub struct BoundaryRestorePlan {
    /// The recorded workspace (from the checkpoint, not the cwd — a
    /// rewind may run from anywhere).
    pub workspace: PathBuf,
    /// One entry per path first touched after the boundary, sorted by
    /// path so output and snapshots are stable.
    pub entries: Vec<BoundaryEntry>,
    /// Commands the dropped region ran; unrestorable by construction.
    pub commands: Vec<CommandEffect>,
}

/// The dropped region's effects reduced to what a boundary restore
/// needs: for each path, the pre-image recorded by its FIRST touch
/// after `boundary` (first-touch-wins), plus every command run.
///
/// Why the first touch is the proof: nothing in `[boundary, i)` touched
/// the path (it is the first touch), so the bytes that tool read off
/// disk are exactly the path's content at the boundary — including
/// content from before the session or from out-of-band edits. A
/// pre-image from a later item would describe a newer state; one from
/// an earlier item, an older one. Only the first touch's image is the
/// boundary's state.
fn dropped_region_effects(
    session: &Session,
    boundary: u32,
) -> (BTreeMap<String, Option<BeforeImage>>, Vec<CommandEffect>) {
    let mut first_touch: BTreeMap<String, Option<BeforeImage>> = BTreeMap::new();
    let mut commands = Vec::new();
    for item in &session.transcript[boundary as usize..] {
        let Item::ToolResult(result) = item else {
            continue;
        };
        commands.extend(result.effects.commands_run.iter().cloned());
        for path in result
            .effects
            .files_written
            .iter()
            .chain(&result.effects.files_deleted)
        {
            if first_touch.contains_key(path) {
                continue;
            }
            let image = result
                .effects
                .before_images
                .iter()
                .find(|image| &image.path == path)
                .cloned();
            first_touch.insert(path.clone(), image);
        }
    }
    (first_touch, commands)
}

/// What the boundary listing shows per row: whether a tier-2 restore of
/// that row is provable, and how many unrestorable commands the dropped
/// region ran. Advisory only; the plan built at fork time is the
/// authority.
pub struct BoundaryRestoreSummary {
    /// Every first-touched path has a recorded pre-image.
    pub provable: bool,
    /// `commands_run` entries in the dropped region; unrestorable.
    pub commands: usize,
}

/// Reduces the dropped region of `session.transcript[from..]` to the
/// advisory summary the listing prints.
pub fn boundary_restore_summary(session: &Session, from: u32) -> BoundaryRestoreSummary {
    let (first_touch, commands) = dropped_region_effects(session, from);
    let provable = first_touch.values().all(|image| image.is_some());
    BoundaryRestoreSummary {
        provable,
        commands: commands.len(),
    }
}

/// A path a restore may act on: workspace-relative, no traversal, and
/// outside the recovery-snapshot directory itself.
fn ensure_safe_restore_path(path: &str) -> Result<()> {
    anyhow::ensure!(!path.is_empty(), "an effect recorded an empty path");
    anyhow::ensure!(
        !path.starts_with('/'),
        "an effect recorded an absolute path: {path}"
    );
    anyhow::ensure!(
        !path.split('/').any(|segment| segment == ".."),
        "an effect recorded a path that escapes the workspace: {path}"
    );
    anyhow::ensure!(
        path != ".loom/rewind" && !path.starts_with(".loom/rewind/"),
        "restoring \"{path}\" would write inside the recovery-snapshot \
         directory"
    );
    Ok(())
}

/// Checks everything a tier-2 boundary restore claims, read-only, and
/// returns the plan. Invariant 2 — refusal beats best-effort — wants
/// refusals BEFORE the fork runs, so the caller plans first and applies
/// only after the fork succeeded.
///
/// Refuses when the recorded workspace is gone; when a first-touched
/// path has no pre-image entry (its touch predates tier-2 capture or
/// ran without an artifact store — a missing entry is an honest "not
/// provable", never a guess); and when a path would escape the
/// workspace or land inside `.loom/rewind` (a restore must never
/// destroy its own recovery snapshots).
pub fn plan_boundary_restore(
    session: &Session,
    boundary: u32,
    workspace: &Path,
) -> Result<BoundaryRestorePlan> {
    let tip = session.transcript.len() as u32;
    anyhow::ensure!(
        boundary <= tip,
        "boundary {boundary} is past the session tip {tip}"
    );
    anyhow::ensure!(
        workspace.is_dir(),
        "recorded workspace {} no longer exists",
        workspace.display()
    );
    let (first_touch, commands) = dropped_region_effects(session, boundary);
    let mut entries = Vec::with_capacity(first_touch.len());
    for (path, image) in &first_touch {
        ensure_safe_restore_path(path)?;
        let Some(image) = image else {
            bail!(
                "no pre-image was captured for \"{path}\" (its first touch \
                 after the boundary predates tier-2 capture or ran without an \
                 artifact store); restoring it is not provable"
            );
        };
        entries.push(BoundaryEntry {
            path: path.clone(),
            content: image.content.clone(),
        });
    }
    Ok(BoundaryRestorePlan {
        workspace: workspace.to_owned(),
        entries,
        commands,
    })
}

/// Applies a tier-2 boundary restore: recovery snapshot first (§5),
/// then one mutation per entry. The snapshot is per-file copies — not a
/// git diff — because tier-2 claims come from recorded effects, not
/// from git, and must recover on a dirty checkout or a workspace with
/// no repository at all. Returns the snapshot directory.
///
/// Every write is proven: the pre-image body comes from the artifact
/// store, and its sha256 must match the recorded reference — a body
/// that drifted (foreign store, bit rot) refuses rather than writes.
pub fn apply_boundary_restore(
    plan: &BoundaryRestorePlan,
    artifacts: &dyn ArtifactStore,
) -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    // `.files` directories hold per-file copies; `.patch` files (tier 1)
    // never collide with them.
    let snapshot_dir = plan
        .workspace
        .join(".loom")
        .join("rewind")
        .join(format!("{timestamp}.files"));
    std::fs::create_dir_all(&snapshot_dir)
        .with_context(|| format!("creating {}", snapshot_dir.display()))?;

    // Snapshot before any mutation: copy each plan path's current bytes
    // (if any), then a manifest recording what existed. Enough to undo
    // the restore by hand even though the restore itself is exact.
    let mut manifest: Vec<serde_json::Value> = Vec::new();
    for entry in &plan.entries {
        let target = plan.workspace.join(&entry.path);
        if target.is_file() {
            let copy = snapshot_dir.join(&entry.path);
            if let Some(parent) = copy.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::copy(&target, &copy).with_context(|| {
                format!("snapshotting {} to {}", target.display(), copy.display())
            })?;
            manifest.push(serde_json::json!({
                "path": entry.path,
                "existed": true,
            }));
        } else {
            manifest.push(serde_json::json!({
                "path": entry.path,
                "existed": false,
            }));
        }
    }
    let manifest_value = serde_json::json!({
        "kind": "boundary-restore-snapshot",
        "created_at_unix_ms": timestamp,
        "workspace": plan.workspace,
        "entries": manifest,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest_value)
        .map_err(|error| anyhow::anyhow!("encoding the snapshot manifest: {error}"))?;
    std::fs::write(snapshot_dir.join("manifest.json"), manifest_bytes)
        .with_context(|| "writing the snapshot manifest")?;

    for entry in &plan.entries {
        let target = plan.workspace.join(&entry.path);
        match &entry.content {
            // Absent at the boundary: the entry positively claims the
            // path did not exist. Delete what is there now — the
            // snapshot above holds its bytes.
            None => {
                if target.is_dir() {
                    bail!(
                        "\"{}\" is a directory now but was absent at the \
                         boundary; refusing to remove it (recovery snapshot: {})",
                        entry.path,
                        snapshot_dir.display()
                    );
                }
                if target.exists() {
                    std::fs::remove_file(&target).with_context(|| {
                        format!("deleting {} (absent at the boundary)", target.display())
                    })?;
                }
            }
            Some(reference) => {
                let stored = artifacts
                    .get(&reference.id)
                    .with_context(|| format!("reading the pre-image for \"{}\"", entry.path))?
                    .with_context(|| {
                        format!(
                            "the pre-image body for \"{}\" is missing from the \
                             artifact store (collected, or a foreign store); \
                             refusing to guess",
                            entry.path
                        )
                    })?;
                anyhow::ensure!(
                    stored.sha256 == reference.sha256,
                    "the pre-image body for \"{}\" failed its content check \
                     (recorded {}, stored {}); refusing to write it",
                    entry.path,
                    reference.sha256,
                    stored.sha256
                );
                if let Some(parent) = target.parent().filter(|p| !p.exists()) {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&target, &stored.body).with_context(|| {
                    format!("restoring {} from its pre-image", target.display())
                })?;
            }
        }
    }
    Ok(snapshot_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A committed git repo in a temp dir with one tracked file, plus
    /// its HEAD sha.
    fn temp_repo(contents: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("file.txt"), contents).expect("write");
        for args in [
            vec!["init".to_owned()],
            vec![
                "config".to_owned(),
                "user.email".to_owned(),
                "t@loom.test".to_owned(),
            ],
            vec![
                "config".to_owned(),
                "user.name".to_owned(),
                "loom test".to_owned(),
            ],
            vec!["add".to_owned(), "-A".to_owned()],
            vec!["commit".to_owned(), "-m".to_owned(), "base".to_owned()],
        ] {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            run_git(root, &refs).unwrap_or_else(|error| panic!("{}: {error}", args[0]));
        }
        let head = run_git(root, &["rev-parse", "HEAD"]).expect("head");
        (dir, head)
    }

    #[test]
    fn snapshot_start_captures_head_on_a_clean_checkout() {
        let (dir, head) = temp_repo("base\n");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head.as_deref(), Some(head.as_str()));
        assert_eq!(checkpoint.reason, None);
        // Canonicalized: the checkpoint is a restore target, so it must
        // survive the caller passing a symlinked path (macOS /var).
        assert_eq!(
            checkpoint.workspace,
            std::fs::canonicalize(dir.path()).expect("canonicalize")
        );
    }

    #[test]
    fn snapshot_start_refuses_a_dirty_start_with_the_change_count() {
        let (dir, _head) = temp_repo("base\n");
        std::fs::write(dir.path().join("file.txt"), "dirty\n").expect("write");
        std::fs::write(dir.path().join("new.txt"), "untracked\n").expect("write");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head, None);
        let reason = checkpoint.reason.expect("reason");
        assert!(reason.contains("dirty"), "reason said: {reason}");
        assert!(reason.contains("2 changed paths"), "reason said: {reason}");
    }

    #[test]
    fn snapshot_start_refuses_a_workspace_without_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkpoint = snapshot_start(dir.path());
        assert_eq!(checkpoint.head, None);
        assert!(
            checkpoint
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not a git repository")),
            "reason said: {:?}",
            checkpoint.reason
        );
    }

    #[test]
    fn plan_restore_refuses_everything_it_cannot_prove() {
        let (dir, head) = temp_repo("base\n");

        let dirty = StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: None,
            reason: Some("workspace was dirty at session start (3 changed paths)".to_owned()),
        };
        let error = plan_restore(&dirty).expect_err("dirty checkpoint");
        assert!(
            error.to_string().contains("3 changed paths"),
            "error said: {error}"
        );

        let gone = StartCheckpoint {
            workspace: dir.path().join("gone"),
            head: Some(head.clone()),
            reason: None,
        };
        assert!(plan_restore(&gone).is_err(), "missing workspace");

        let missing_commit = StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: Some("0".repeat(40)),
            reason: None,
        };
        let error = plan_restore(&missing_commit).expect_err("bogus commit");
        assert!(
            error.to_string().contains("not available"),
            "error said: {error}"
        );
    }

    #[test]
    fn apply_restore_resets_the_tree_and_keeps_a_recovery_snapshot() {
        let (dir, head) = temp_repo("base\n");
        std::fs::write(dir.path().join("file.txt"), "changed\n").expect("write");
        std::fs::write(dir.path().join("untracked.txt"), "throwaway\n").expect("write");

        let plan = plan_restore(&StartCheckpoint {
            workspace: dir.path().to_owned(),
            head: Some(head.clone()),
            reason: None,
        })
        .expect("plan");
        let patch = apply_restore(&plan).expect("restore");

        assert!(patch.is_file(), "snapshot at {}", patch.display());
        let snapshot = std::fs::read_to_string(&patch).expect("read snapshot");
        assert!(
            snapshot.contains("changed") && snapshot.contains("throwaway"),
            "snapshot should hold the discarded content"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt")).expect("read"),
            "base\n"
        );
        assert!(!dir.path().join("untracked.txt").exists());
        // The snapshot dir itself survives the clean (-e .loom).
        assert!(patch.exists());
        assert_eq!(
            run_git(dir.path(), &["rev-parse", "HEAD"]).expect("head"),
            head
        );
    }

    // ---- tier 2: boundary restore from pre-images ----

    use loom_core::kernel::artifact::store_artifact;
    use loom_core::kernel::id::SessionId;
    use loom_core::kernel::store::MemoryArtifactStore;
    use loom_core::kernel::transcript::{ToolCallId, ToolResult, ToolStatus};

    /// A session whose one tool result rewrote `src/lib.rs` (pre-image
    /// captured) and created `notes.txt` (absent at the boundary), and
    /// which optionally ran one command. Transcript: user, assistant,
    /// result — boundary 2 drops the whole turn.
    fn boundary_session(
        artifacts: &MemoryArtifactStore,
        with_command: bool,
    ) -> loom_core::kernel::session::Session {
        let lib_ref =
            store_artifact(artifacts, "pre_image", b"old lib\n").expect("store pre-image");
        let mut result = ToolResult::new(
            ToolCallId::try_from("call_edit_1".to_owned()).expect("call id"),
            ToolStatus::Success,
            "wrote",
            false,
        );
        result.effects = result
            .effects
            .with_file_written("src/lib.rs")
            .with_file_written("notes.txt")
            .with_before_image(BeforeImage::captured("src/lib.rs", lib_ref))
            .with_before_image(BeforeImage::absent("notes.txt"));
        if with_command {
            result.effects = result.effects.with_command(CommandEffect::new(
                "cargo",
                vec!["test".to_owned(), "--all".to_owned()],
                Some(0),
            ));
        }
        let session = loom_core::kernel::session::Session::new(SessionId::new());
        let mut session = session;
        let user = loom_core::kernel::transcript::Item::User {
            text: "go".to_owned(),
        };
        session.push_item(user).expect("push user");
        session
            .push_item(loom_core::kernel::transcript::Item::Assistant(
                loom_core::kernel::transcript::AssistantMessage {
                    content: "editing".to_owned(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: loom_core::kernel::model::StopReason::EndTurn,
                },
            ))
            .expect("push assistant");
        session
            .push_item(loom_core::kernel::transcript::Item::ToolResult(result))
            .expect("push result");
        session
    }

    #[test]
    fn boundary_plan_refuses_paths_it_cannot_prove_or_trust() {
        let artifacts = MemoryArtifactStore::new();
        let session = boundary_session(&artifacts, false);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("dirs");

        // Provable: entries in path order, absent-vs-captured preserved.
        // Artifact ids are fresh per store write even for identical
        // bytes — the sha is the content identity, so compare that.
        let plan = plan_boundary_restore(&session, 2, dir.path()).expect("provable plan");
        let lib_sha = store_artifact(&artifacts, "pre_image", b"old lib\n")
            .expect("same content, same sha")
            .sha256;
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (
                    entry.path.as_str(),
                    entry.content.as_ref().map(|r| r.sha256.as_str())
                ))
                .collect::<Vec<_>>(),
            vec![("notes.txt", None), ("src/lib.rs", Some(lib_sha.as_str())),]
        );
        assert!(plan.commands.is_empty());

        // The summary agrees with the plan.
        let summary = boundary_restore_summary(&session, 2);
        assert!(summary.provable);
        assert_eq!(summary.commands, 0);

        // No pre-image on the first touch: honest refusal, no guess.
        let mut bare = boundary_session(&artifacts, false);
        if let loom_core::kernel::transcript::Item::ToolResult(result) =
            bare.transcript.last_mut().expect("result item")
        {
            result.effects.before_images.clear();
        }
        let error = plan_boundary_restore(&bare, 2, dir.path()).expect_err("no pre-image");
        assert!(
            error.to_string().contains("no pre-image was captured"),
            "error said: {error}"
        );

        // Escaping paths and the snapshot dir are never written.
        let mut escape = boundary_session(&artifacts, false);
        if let loom_core::kernel::transcript::Item::ToolResult(result) =
            escape.transcript.last_mut().expect("result item")
        {
            result
                .effects
                .files_written
                .push("../outside.txt".to_owned());
        }
        let error = plan_boundary_restore(&escape, 2, dir.path()).expect_err("escape");
        assert!(
            error.to_string().contains("escapes the workspace"),
            "error said: {error}"
        );

        let mut into_snapshots = boundary_session(&artifacts, false);
        if let loom_core::kernel::transcript::Item::ToolResult(result) =
            into_snapshots.transcript.last_mut().expect("result item")
        {
            result
                .effects
                .files_written
                .push(".loom/rewind/evil.txt".to_owned());
        }
        let error =
            plan_boundary_restore(&into_snapshots, 2, dir.path()).expect_err("snapshot dir");
        assert!(
            error.to_string().contains("recovery-snapshot directory"),
            "error said: {error}"
        );

        // A gone workspace refuses, and so does a past-tip boundary.
        let missing = dir.path().join("missing");
        let error = plan_boundary_restore(&session, 2, &missing).expect_err("gone workspace");
        assert!(
            error.to_string().contains("no longer exists"),
            "error said: {error}"
        );
        let error = plan_boundary_restore(&session, 99, dir.path()).expect_err("past tip");
        assert!(
            error.to_string().contains("past the session tip"),
            "error said: {error}"
        );
    }

    #[test]
    fn boundary_apply_restores_bytes_deletes_absent_and_snapshots() {
        let artifacts = MemoryArtifactStore::new();
        let session = boundary_session(&artifacts, false);
        let dir = tempfile::tempdir().expect("tempdir");
        // The workspace as the dropped turn left it: lib.rs rewritten,
        // notes.txt created. No git anywhere — tier 2 never needs it.
        std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
        std::fs::write(dir.path().join("src/lib.rs"), "new lib\n").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "late notes\n").expect("write");

        let plan = plan_boundary_restore(&session, 2, dir.path()).expect("provable plan");
        let snapshot = apply_boundary_restore(&plan, &artifacts).expect("apply");

        // Exactly the boundary state: old bytes back, created file gone.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read"),
            "old lib\n"
        );
        assert!(!dir.path().join("notes.txt").exists());

        // The recovery snapshot holds what was discarded (§5), with a
        // manifest naming it.
        let discarded_lib = snapshot.join("src").join("lib.rs");
        assert_eq!(
            std::fs::read_to_string(&discarded_lib).expect("read"),
            "new lib\n"
        );
        assert_eq!(
            std::fs::read_to_string(snapshot.join("notes.txt")).expect("read"),
            "late notes\n"
        );
        let manifest = std::fs::read_to_string(snapshot.join("manifest.json")).expect("read");
        assert!(
            manifest.contains("\"existed\": true") && manifest.contains("notes.txt"),
            "manifest should record what existed: {manifest}"
        );
    }

    #[test]
    fn boundary_apply_refuses_a_body_that_cannot_be_proven() {
        let artifacts = MemoryArtifactStore::new();
        let session = boundary_session(&artifacts, false);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
        std::fs::write(dir.path().join("src/lib.rs"), "new lib\n").expect("write");

        // A foreign store: same transcript, but the bodies never made
        // it there. The plan still builds (effects claim an image); the
        // apply must refuse instead of guessing.
        let foreign = MemoryArtifactStore::new();
        let plan = plan_boundary_restore(&session, 2, dir.path()).expect("provable plan");
        let error = apply_boundary_restore(&plan, &foreign).expect_err("missing body");
        assert!(
            error
                .to_string()
                .contains("missing from the artifact store"),
            "error said: {error}"
        );

        // Commands surface on the plan for the caller to gate on; the
        // file restore itself stays provable.
        let with_commands = boundary_session(&artifacts, true);
        let plan = plan_boundary_restore(&with_commands, 2, dir.path()).expect("plan");
        assert_eq!(plan.commands.len(), 1);
        let summary = boundary_restore_summary(&with_commands, 2);
        assert!(summary.provable);
        assert_eq!(summary.commands, 1);
    }
}
