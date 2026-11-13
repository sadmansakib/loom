use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};

fn ctx(root: &Path) -> ToolContext {
    ToolContext::new(
        root.to_path_buf(),
        CancellationToken::new(),
        Duration::from_secs(30),
    )
}

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    (dir, root)
}

#[tokio::test]
async fn a_job_finishes_and_reports_once() {
    let (_guard, root) = temp_root();
    let jobs = BackgroundJobs::default();
    let prepared = prepare_exec(&CommandPolicy::allowing(["echo"]), None, "echo", &[], &root)
        .await
        .expect("prepared");
    let id = jobs.spawn_job(
        "echo",
        vec!["done".to_owned()],
        root.clone(),
        None,
        prepared,
    );
    // It reports exactly once, with exit status and output.
    let notice = loop {
        let notices = jobs.take_finished_notices();
        if let Some(notice) = notices.first() {
            break notice.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(notice.contains(&format!("job {id} finished")), "{notice}");
    assert!(notice.contains("exit status: 0"), "{notice}");
    assert!(notice.contains("done"), "{notice}");
    assert!(jobs.take_finished_notices().is_empty(), "reports once");
}

#[tokio::test]
async fn status_lists_and_details_jobs() {
    let (_guard, root) = temp_root();
    let jobs = BackgroundJobs::default();
    let tool = CommandStatusTool::new(jobs.clone());
    let empty = tool
        .call(json!({"action": "list"}), &ctx(&root))
        .await
        .expect("ok");
    assert_eq!(empty.content, "no background jobs");

    let prepared = prepare_exec(&CommandPolicy::allowing(["echo"]), None, "echo", &[], &root)
        .await
        .expect("prepared");
    let id = jobs.spawn_job("echo", vec!["hi".to_owned()], root.clone(), None, prepared);
    let listed = tool
        .call(json!({"action": "list"}), &ctx(&root))
        .await
        .expect("ok");
    assert!(
        listed.content.contains(&format!("job {id}")),
        "{}",
        listed.content
    );

    let _ = jobs.take_finished_notices();
    loop {
        let detailed = tool
            .call(json!({"action": "show", "id": id}), &ctx(&root))
            .await
            .expect("ok");
        if detailed.content.contains("exit status: 0") {
            assert!(detailed.content.contains("hi"), "{}", detailed.content);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let unknown = tool
        .call(json!({"action": "show", "id": 99}), &ctx(&root))
        .await
        .expect("ok");
    assert!(unknown.content.contains("no background job 99"));
}

#[tokio::test]
async fn the_start_tool_applies_the_policy_and_returns_a_job() {
    let (_guard, root) = temp_root();
    let jobs = BackgroundJobs::default();
    let tool = RunBackgroundTool::new(CommandPolicy::allowing(["echo"]), jobs.clone());
    let output = tool
        .call(json!({"program": "echo", "args": ["hi"]}), &ctx(&root))
        .await
        .expect("started");
    assert!(
        output.content.contains("started job 1"),
        "{}",
        output.content
    );
    assert_eq!(
        output.effects.commands_run,
        vec![CommandEffect::new("echo", vec!["hi".to_owned()], None)]
    );

    // Denied programs never start.
    let denied = RunBackgroundTool::new(
        CommandPolicy::parse("[[deny]]\nprogram = \"curl\"\n").expect("policy"),
        jobs.clone(),
    );
    let err = denied
        .call(json!({"program": "curl"}), &ctx(&root))
        .await
        .expect_err("denied");
    assert!(matches!(err, ToolError::InvalidInput(ref msg) if msg.contains("denied")));

    // Path-qualified programs are refused outright.
    let err = tool
        .call(json!({"program": "/bin/echo"}), &ctx(&root))
        .await
        .expect_err("path");
    assert!(matches!(err, ToolError::InvalidInput(ref msg) if msg.contains("bare executable")));
}

#[tokio::test]
async fn a_rule_timeout_caps_a_background_job() {
    let (_guard, root) = temp_root();
    let jobs = BackgroundJobs::default();
    let policy = CommandPolicy::parse("[[command]]\nprogram = \"sleep\"\ntimeout_secs = 1\n")
        .expect("policy");
    let tool = RunBackgroundTool::new(policy, jobs.clone());
    let started = std::time::Instant::now();
    tool.call(json!({"program": "sleep", "args": ["30"]}), &ctx(&root))
        .await
        .expect("started");
    let notice = loop {
        if let Some(notice) = jobs.take_finished_notices().first() {
            break notice.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(notice.contains("timed out after 1s"), "{notice}");
    assert!(
        started.elapsed().as_secs() < 10,
        "must not wait for the child"
    );
}

#[tokio::test]
async fn dropping_the_registry_kills_running_jobs() {
    let (_guard, root) = temp_root();
    let jobs = BackgroundJobs::default();
    let prepared = prepare_exec(
        &CommandPolicy::allowing(["sleep"]),
        None,
        "sleep",
        &[],
        &root,
    )
    .await
    .expect("prepared");
    jobs.spawn_job("sleep", vec!["30".to_owned()], root.clone(), None, prepared);
    // Drop every clone, then poll: the abort lands asynchronously
    // (the scheduler must reap the cancelled task before its
    // group-kill guard fires), so a one-shot check races the kill.
    drop(jobs);
    drop(root);
    let mut leftover = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            // Anchored: an unrelated `sleep 300` elsewhere on the
            // box must not satisfy this poll.
            .arg("sleep 30$")
            .output()
            .expect("pgrep");
        leftover = String::from_utf8_lossy(&out.stdout).into_owned();
        if leftover.is_empty() {
            break;
        }
    }
    assert!(
        leftover.is_empty(),
        "the job must not outlive the registry: {leftover:?}"
    );
}

#[test]
fn the_schemas_and_the_parsers_agree() {
    crate::schema::agreement::check::<BackgroundInput>(json!({"program": "cargo"}));
    crate::schema::agreement::check::<StatusInput>(json!({"action": "list"}));
}
