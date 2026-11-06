//! `loom rewind` end to end: the binary reads a store it did not create,
//! lists conversation-valid boundaries, and forks at one — leaving the
//! source intact and the lineage visible in `loom sessions`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use loom_core::kernel::id::SessionId;
use loom_core::kernel::model::StopReason;
use loom_core::kernel::session::Session;
use loom_core::kernel::store::SessionStore;
use loom_core::kernel::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use loom_core::store::SqliteStore;
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).unwrap(),
        name: name.to_owned(),
        arguments: ToolArguments::try_from(args).unwrap(),
    }
}

fn result(id: &str, content: &str) -> ToolResult {
    ToolResult::new(
        ToolCallId::try_from(id.to_owned()).unwrap(),
        ToolStatus::Success,
        content,
        false,
    )
}

fn turn(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: text.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    }
}

/// Mirrors the engine's persist-then-commit flow into the store.
fn commit(
    store: &SqliteStore,
    id: &SessionId,
    session: &mut Session,
    assistant: AssistantMessage,
    results: Vec<ToolResult>,
) {
    let planned = session.plan_turn(assistant, results).unwrap();
    store.append_turn(id, &planned).unwrap();
    session.apply_planned_turn(planned);
}

/// Seeds one session: user input, a read turn, an edit turn whose result
/// records a written file. Transcript: 0 user, 1 assistant, 2 result,
/// 3 assistant, 4 result — tip 5, boundaries 0/1/3.
fn seeded_store(db: &std::path::Path) -> SessionId {
    let store = SqliteStore::open(db).unwrap();
    let id = SessionId::new();
    let mut session = Session::new(id);
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "fix the bug".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    commit(
        &store,
        &id,
        &mut session,
        turn(
            "reading",
            vec![call("a", "read_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result("a", "fn main() {}")],
    );
    let mut edit = result("b", "applied");
    edit.effects.files_written = vec!["src/lib.rs".to_owned()];
    commit(
        &store,
        &id,
        &mut session,
        turn(
            "editing",
            vec![call("b", "edit_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![edit],
    );
    id
}

fn loom(db: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(args)
        .env("LOOM_DB", db)
        .output()
        .expect("spawn loom");
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// Seeds a session whose edit turn recorded tier-2 pre-images: `lib.rs`
/// rewritten (its old bytes content-addressed into the store) and
/// `notes.txt` created (absent at the boundary). Optionally also ran
/// one command. Same shape as [`seeded_store`]: tip 5, boundaries
/// 0/1/3.
fn seeded_store_with_pre_images(db: &std::path::Path, with_command: bool) -> SessionId {
    use loom_core::kernel::artifact::store_artifact;
    use loom_core::kernel::tool::{BeforeImage, CommandEffect};

    let store = SqliteStore::open(db).unwrap();
    let id = SessionId::new();
    let mut session = Session::new(id);
    store.ensure_session(&id).unwrap();
    let user = Item::User {
        text: "fix the bug".to_owned(),
    };
    let index = session.push_item(user.clone()).unwrap();
    store.append_item(&id, index, &user).unwrap();

    commit(
        &store,
        &id,
        &mut session,
        turn(
            "reading",
            vec![call("a", "read_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![result("a", "fn main() {}")],
    );
    let old = store_artifact(&store, "pre_image", b"fn main() { old }\n")
        .expect("store the pre-image body");
    let mut edit = result("b", "applied");
    edit.effects = edit
        .effects
        .with_file_written("src/lib.rs")
        .with_file_written("notes.txt")
        .with_before_image(BeforeImage::captured("src/lib.rs", old))
        .with_before_image(BeforeImage::absent("notes.txt"));
    if with_command {
        edit.effects = edit.effects.with_command(CommandEffect::new(
            "cargo",
            vec!["test".to_owned()],
            Some(0),
        ));
    }
    commit(
        &store,
        &id,
        &mut session,
        turn(
            "editing",
            vec![call("b", "edit_file", json!({"path": "src/lib.rs"}))],
        ),
        vec![edit],
    );
    id
}

#[test]
fn rewind_lists_boundaries_with_effects() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string()]);
    assert!(ok, "listing failed: {stderr}");
    assert!(stdout.contains("session"), "header missing: {stdout}");
    assert!(stdout.contains("user"), "user row missing: {stdout}");
    assert!(stdout.contains("turn 1"), "turn 1 row missing: {stdout}");
    assert!(stdout.contains("turn 2"), "turn 2 row missing: {stdout}");
    assert!(stdout.contains("tip"), "tip row missing: {stdout}");
    // The turn-1 row drops the edit turn, whose effects name the file.
    assert!(
        stdout.contains("src/lib.rs"),
        "dropped-region effects missing: {stdout}"
    );
    assert!(
        stdout.contains(&format!("fork with: loom rewind {id}")),
        "hint missing: {stdout}"
    );
}

#[test]
fn rewind_refuses_mid_turn_and_unknown_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    // Index 2 is a tool result: cutting there would strand the call.
    let (ok, _stdout, stderr) = loom(&db, &["rewind", &id.to_string(), "--at", "2"]);
    assert!(!ok, "mid-turn fork must fail");
    assert!(
        stderr.contains("not a rewind boundary"),
        "wrong refusal: {stderr}"
    );

    // A well-formed id that was never stored.
    let stranger = SessionId::new().to_string();
    let (ok, _stdout, stderr) = loom(&db, &["rewind", &stranger]);
    assert!(!ok, "unknown session must fail");
    assert!(
        stderr.contains("unknown session"),
        "wrong refusal: {stderr}"
    );
}

#[test]
fn rewind_forks_at_a_boundary_and_sessions_shows_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);

    // Boundary 3: before the edit turn — keep user + read turn.
    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string(), "--at", "3"]);
    assert!(ok, "fork failed: {stderr}");
    assert!(
        stdout.contains("kept [0, 3), dropped 2 item(s)"),
        "fork summary missing: {stdout}"
    );
    let new_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("forked "))
        .and_then(|rest| rest.split(' ').next())
        .expect("forked id in output")
        .to_owned();
    assert_ne!(new_id, id.to_string());
    assert!(
        stdout.contains(&format!("loom run --session {new_id}")),
        "continuation hint missing: {stdout}"
    );

    // The fork carries lineage; the source row is unchanged and the
    // fork's transcript is the kept prefix.
    let (ok, stdout, stderr) = loom(&db, &["sessions"]);
    assert!(ok, "sessions failed: {stderr}");
    assert!(
        stdout.contains(&format!("[fork of {id} @ 3]")),
        "lineage marker missing: {stdout}"
    );

    let store = SqliteStore::open(&db).unwrap();
    let forked: SessionId = new_id.parse().unwrap();
    let forked = store.load(&forked).unwrap().expect("forked session");
    assert_eq!(forked.transcript.len(), 3);
    let source = store.load(&id).unwrap().expect("source intact");
    assert_eq!(source.transcript.len(), 5);
}

/// A git repo with one committed file, diverged after the fact: the
/// tracked file rewritten and an untracked stray added. Returns the
/// repo dir and the base commit.
fn diverged_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "t@loom.test"],
        vec!["config", "user.name", "loom test"],
        vec!["add", "-A"],
        vec!["commit", "-m", "base"],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(&args)
            .output()
            .expect("spawn git");
        assert!(status.status.success(), "git {args:?} failed in fixture");
    }
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("spawn git")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    std::fs::write(root.join("base.txt"), "diverged\n").unwrap();
    std::fs::write(root.join("stray.txt"), "untracked\n").unwrap();
    (dir, head)
}

#[test]
fn rewind_restores_the_workspace_to_the_start_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);
    let (repo, head) = diverged_repo();

    // The run that started this session began on a clean checkout at
    // `head` (what `loom run` records via the workspace snapshot).
    let store = SqliteStore::open(&db).unwrap();
    let checkpoint = loom_core::kernel::store::StartCheckpoint {
        workspace: std::fs::canonicalize(repo.path()).unwrap(),
        head: Some(head.clone()),
        reason: None,
    };
    store.record_start_checkpoint(&id, &checkpoint).unwrap();

    // The listing says what is provable before any fork is attempted.
    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string()]);
    assert!(ok, "listing failed: {stderr}");
    assert!(
        stdout.contains(&format!("workspace restore: to-start {}", &head[..8])),
        "restore line missing: {stdout}"
    );

    let (ok, stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
            "--to-start",
        ],
    );
    assert!(ok, "restore rewind failed: {stderr}");
    assert!(
        stdout.contains(&format!(
            "restored workspace {} to start {}",
            std::fs::canonicalize(repo.path()).unwrap().display(),
            &head[..8]
        )),
        "restore summary missing: {stdout}"
    );

    // The tree is exactly the base commit; the stray is gone; the
    // recovery snapshot kept what was discarded (§5: kept, not
    // garbage-collected) and survived `git clean` itself.
    assert_eq!(
        std::fs::read_to_string(repo.path().join("base.txt")).unwrap(),
        "base\n"
    );
    assert!(!repo.path().join("stray.txt").exists());
    let rewind_dir = repo.path().join(".loom").join("rewind");
    let patches: Vec<_> = std::fs::read_dir(&rewind_dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "patch")
        })
        .collect();
    assert_eq!(patches.len(), 1, "one recovery snapshot");
    let patch = std::fs::read_to_string(patches[0].as_ref().unwrap().path()).unwrap();
    assert!(
        patch.contains("diverged") && patch.contains("untracked"),
        "snapshot must hold the discarded content: {patch}"
    );

    // The fork carries the lineage's restore point, and the source is
    // intact with its own checkpoint unchanged.
    let fork_id: SessionId = stdout
        .lines()
        .find_map(|line| line.strip_prefix("forked "))
        .and_then(|rest| rest.split(' ').next())
        .expect("forked id")
        .parse()
        .unwrap();
    assert_eq!(
        store.start_checkpoint(&fork_id).unwrap(),
        Some(checkpoint.clone())
    );
    assert_eq!(store.start_checkpoint(&id).unwrap(), Some(checkpoint));
}

#[test]
fn rewind_refuses_workspace_restore_without_provable_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store(&db);
    let (repo, _head) = diverged_repo();

    // Tier 0: a dirty start records its reason, and the refusal quotes
    // it — checked BEFORE the fork, so nothing half-done remains.
    let store = SqliteStore::open(&db).unwrap();
    store
        .record_start_checkpoint(
            &id,
            &loom_core::kernel::store::StartCheckpoint {
                workspace: std::fs::canonicalize(repo.path()).unwrap(),
                head: None,
                reason: Some("workspace was dirty at session start (2 changed paths)".to_owned()),
            },
        )
        .unwrap();
    let (ok, _stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
            "--to-start",
        ],
    );
    assert!(!ok, "a dirty start must refuse");
    assert!(
        stderr.contains("dirty at session start"),
        "wrong refusal: {stderr}"
    );
    // Refused before the fork: no fork row, tree untouched.
    assert_eq!(store.session_ids().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("base.txt")).unwrap(),
        "diverged\n"
    );
    assert!(repo.path().join("stray.txt").exists());

    // Tier 0 again: a session with no checkpoint at all (predates the
    // feature or ran with --no-save).
    let stranger = seeded_store(&db);
    let (ok, _stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &stranger.to_string(),
            "--at",
            "3",
            "--restore-workspace",
            "--to-start",
        ],
    );
    assert!(!ok, "a checkpointless session must refuse");
    assert!(
        stderr.contains("no start checkpoint"),
        "wrong refusal: {stderr}"
    );

    // Tier 2 pairing, on a session recorded before pre-images: the
    // file touch exists but its proof does not, so the restore refuses
    // rather than guessing — and refuses before the fork.
    let (ok, _stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
        ],
    );
    assert!(!ok, "an unprovable boundary restore must fail");
    assert!(
        stderr.contains("no pre-image was captured"),
        "wrong refusal: {stderr}"
    );

    // The flag without a boundary (or --to-start) says how to pick one
    // instead of being silently ignored by the listing.
    let (ok, _stdout, stderr) = loom(&db, &["rewind", &id.to_string(), "--restore-workspace"]);
    assert!(
        !ok,
        "--restore-workspace without --at or --to-start must fail"
    );
    assert!(
        stderr.contains("--at IDX"),
        "pairing hint missing: {stderr}"
    );
}

#[test]
fn rewind_restores_the_workspace_to_a_boundary_from_pre_images() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store_with_pre_images(&db, false);
    // The workspace exactly as the edit turn left it — deliberately
    // NOT a git repo: tier-2 claims come from recorded effects, so a
    // dirty or repository-less workspace restores like any other.
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/lib.rs"), "fn main() { new }\n").unwrap();
    std::fs::write(work.path().join("notes.txt"), "late notes\n").unwrap();

    // A dirty start has no tier-1 head; the recorded workspace is all
    // tier 2 needs.
    let store = SqliteStore::open(&db).unwrap();
    store
        .record_start_checkpoint(
            &id,
            &loom_core::kernel::store::StartCheckpoint {
                workspace: std::fs::canonicalize(work.path()).unwrap(),
                head: None,
                reason: Some("workspace was dirty at session start".to_owned()),
            },
        )
        .unwrap();

    // The listing says the boundary is provably restorable before the
    // user commits to anything.
    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string()]);
    assert!(ok, "listing failed: {stderr}");
    assert!(stdout.contains("RESTORE"), "restore column: {stdout}");
    assert!(stdout.contains("yes"), "row should be provable: {stdout}");

    let (ok, stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
        ],
    );
    assert!(ok, "boundary restore failed: {stderr}");
    assert!(
        stdout.contains("restored workspace") && stdout.contains("boundary 3"),
        "restore summary missing: {stdout}"
    );

    // Exactly the boundary state: old bytes back, the later-created
    // file gone.
    assert_eq!(
        std::fs::read_to_string(work.path().join("src/lib.rs")).unwrap(),
        "fn main() { old }\n"
    );
    assert!(!work.path().join("notes.txt").exists());

    // The recovery snapshot kept what the restore discarded (§5),
    // including the file that no longer exists.
    let snapshots: Vec<_> = std::fs::read_dir(work.path().join(".loom").join("rewind"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".files"))
        })
        .collect();
    assert_eq!(snapshots.len(), 1, "one recovery snapshot dir");
    assert_eq!(
        std::fs::read_to_string(snapshots[0].join("src").join("lib.rs")).unwrap(),
        "fn main() { new }\n"
    );
    assert_eq!(
        std::fs::read_to_string(snapshots[0].join("notes.txt")).unwrap(),
        "late notes\n"
    );
    let manifest = std::fs::read_to_string(snapshots[0].join("manifest.json")).unwrap();
    assert!(
        manifest.contains("notes.txt") && manifest.contains("src/lib.rs"),
        "manifest names both paths: {manifest}"
    );
}

#[test]
fn rewind_gates_boundary_restore_on_unrestorable_commands() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("loom.db");
    let id = seeded_store_with_pre_images(&db, true);
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/lib.rs"), "fn main() { new }\n").unwrap();
    std::fs::write(work.path().join("notes.txt"), "late notes\n").unwrap();

    let store = SqliteStore::open(&db).unwrap();
    store
        .record_start_checkpoint(
            &id,
            &loom_core::kernel::store::StartCheckpoint {
                workspace: std::fs::canonicalize(work.path()).unwrap(),
                head: None,
                reason: Some("workspace was dirty at session start".to_owned()),
            },
        )
        .unwrap();

    // The listing names the commands so the gate is not a surprise.
    let (ok, stdout, stderr) = loom(&db, &["rewind", &id.to_string()]);
    assert!(ok, "listing failed: {stderr}");
    assert!(
        stdout.contains("cmds(1)"),
        "commands should mark the row: {stdout}"
    );

    // Without the flag: refused, naming the command — and before the
    // fork, so nothing half-done remains (tree and session count).
    let (ok, _stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
        ],
    );
    assert!(!ok, "unrestorable commands must gate the restore");
    assert!(
        stderr.contains("cargo test") && stderr.contains("--accept-unrestorable"),
        "wrong refusal: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join("src/lib.rs")).unwrap(),
        "fn main() { new }\n"
    );
    assert!(work.path().join("notes.txt").exists());
    assert_eq!(store.session_ids().unwrap().len(), 1, "no fork happened");

    // With the flag: the provable file changes restore and the
    // commands are surfaced as accepted, not silently absorbed.
    let (ok, stdout, stderr) = loom(
        &db,
        &[
            "rewind",
            &id.to_string(),
            "--at",
            "3",
            "--restore-workspace",
            "--accept-unrestorable",
        ],
    );
    assert!(ok, "accepted restore failed: {stderr}");
    assert!(
        stdout.contains("accepted as unrestorable: cargo test"),
        "acceptance line missing: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(work.path().join("src/lib.rs")).unwrap(),
        "fn main() { old }\n"
    );
    assert!(!work.path().join("notes.txt").exists());
}
