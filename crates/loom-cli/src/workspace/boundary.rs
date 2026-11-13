//! Tier-2 boundary restore from recorded per-turn pre-images.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use loom_core::kernel::artifact::ArtifactRef;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::ArtifactStore;
use loom_core::kernel::tool::{BeforeImage, CommandEffect};
use loom_core::kernel::transcript::Item;

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
    let provable = first_touch.values().all(std::option::Option::is_some);
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

/// A restore must not write *through* a symlink (review S1): a
/// sandboxed model can plant `ln -s ~/.ssh/authorized_keys victim.txt`
/// inside the workspace and let a later rewind "restore" a pre-image
/// straight out of it. Every component of the target — intermediate
/// directories and the final entry alike — must be a real path, not a
/// link. Components below a missing one cannot exist, so the walk
/// stops at the first miss.
fn ensure_no_symlink_traversal(workspace: &Path, relative: &str) -> Result<()> {
    let mut cursor = workspace.to_path_buf();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        cursor.push(segment);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => bail!(
                "\"{relative}\" passes through the symlink {} — refusing to \
                 restore through it; delete the link if it is not yours",
                cursor.display()
            ),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Writes a restored pre-image through a temp file in the target's
/// own directory, then renames it into place. A rename replaces the
/// target's directory entry itself, so even a link planted in the
/// race window between the traversal check and the write is
/// *replaced*, never followed out of the workspace (review S1).
fn restore_via_rename(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("restore target has no parent directory"))?;
    let temp = parent.join(format!(
        ".loom-restore-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&temp, bytes)?;
    match std::fs::rename(&temp, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Checks everything a tier-2 boundary restore claims, read-only, and
/// returns the plan. Invariant 2 — refusal beats best-effort — wants
/// refusals BEFORE the fork runs, so the caller plans first and applies
/// only after the fork succeeded.
///
/// Refuses when the recorded workspace is gone; when a first-touched
/// path has no pre-image entry (its touch predates tier-2 capture or
/// ran without an artifact store — a missing entry is an honest "not
/// provable", never a guess); when a path would escape the
/// workspace or land inside `.loom/rewind` (a restore must never
/// destroy its own recovery snapshots); and when any component of a
/// target path is a symlink (a planted link would otherwise carry the
/// restored bytes outside the workspace — review S1).
pub fn plan_boundary_restore(
    session: &Session,
    boundary: u32,
    workspace: &Path,
) -> Result<BoundaryRestorePlan> {
    let tip = u32::try_from(session.transcript.len()).unwrap_or(u32::MAX);
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
        ensure_no_symlink_traversal(workspace, path)?;
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
/// No path is written through a symlink: each entry's components are
/// re-checked at apply time (the plan ran before the fork), and the
/// write itself is a directory-entry rename that replaces a link
/// instead of following it (review S1).
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
        // Re-checked here because the plan ran before the fork: a link
        // planted in that window must still refuse (review S1).
        ensure_no_symlink_traversal(&plan.workspace, &entry.path)?;
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
                restore_via_rename(&target, &stored.body).with_context(|| {
                    format!("restoring {} from its pre-image", target.display())
                })?;
            }
        }
    }
    Ok(snapshot_dir)
}
