use super::*;
use loom_core::kernel::store::StartCheckpoint;
use loom_core::kernel::tool::{BeforeImage, CommandEffect};

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
        head: Some(head),
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
    let lib_ref = store_artifact(artifacts, "pre_image", b"old lib\n").expect("store pre-image");
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
    let error = plan_boundary_restore(&into_snapshots, 2, dir.path()).expect_err("snapshot dir");
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

#[cfg(unix)]
#[test]
fn boundary_plan_refuses_a_planted_intermediate_symlink() {
    // Review S1: a sandboxed model can plant a symlink inside the
    // workspace (`ln -s` is a plain command); restoring "through"
    // it would carry the pre-image's bytes to the link's victim
    // outside the workspace. Here `src` itself is the link.
    let artifacts = MemoryArtifactStore::new();
    let session = boundary_session(&artifacts, false);
    let dir = tempfile::tempdir().expect("tempdir");
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "precious\n").expect("victim");
    std::os::unix::fs::symlink("victim.txt", dir.path().join("src")).expect("link");

    let error = plan_boundary_restore(&session, 2, dir.path()).expect_err("intermediate symlink");
    assert!(
        error.to_string().contains("passes through the symlink"),
        "error said: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read"),
        "precious\n"
    );
}

#[cfg(unix)]
#[test]
fn boundary_apply_rechecks_a_symlink_planted_after_the_plan() {
    // The plan runs before the fork; the model can plant the link
    // in that window. Apply re-checks each entry before writing
    // and refuses — the victim keeps its bytes (review S1's race
    // window), and the write itself is a rename that would
    // replace a link rather than follow it.
    let artifacts = MemoryArtifactStore::new();
    let session = boundary_session(&artifacts, false);
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/lib.rs"), "new lib\n").expect("write");
    std::fs::write(dir.path().join("notes.txt"), "late notes\n").expect("write");

    let plan = plan_boundary_restore(&session, 2, dir.path()).expect("provable plan");

    // The window: swap lib.rs for a link to the victim.
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "precious\n").expect("victim");
    std::fs::remove_file(dir.path().join("src/lib.rs")).expect("clear");
    std::os::unix::fs::symlink("../victim.txt", dir.path().join("src/lib.rs")).expect("link");

    let error = apply_boundary_restore(&plan, &artifacts).expect_err("recheck");
    assert!(
        error.to_string().contains("passes through the symlink"),
        "error said: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read"),
        "precious\n"
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
