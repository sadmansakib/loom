use std::time::{Duration, Instant};

const MARKER: &str = "loom-eval-b7-marker";

/// Cleans up any marker processes this test leaves behind, even
/// when an assertion fails first.
struct PkillOnDrop;
impl Drop for PkillOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill").args(["-f", MARKER]).status();
    }
}

#[tokio::test]
async fn aborting_the_runner_kills_the_whole_process_tree() {
    let _guard = PkillOnDrop;
    let _ = std::process::Command::new("pkill").args(["-f", MARKER]).status();

    let script = format!("while :; do sleep 0.5; done & exec sleep 3000 #{MARKER}");
    let args = vec!["-c".to_string(), script];
    let task = tokio::spawn(async move { loom_eval_b7::run("sh", &args).await });
    assert!(alive(), "the child tree must come up");

    task.abort();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive(), "aborting the runner must kill grandchildren too");
}

fn alive() -> bool {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg(MARKER)
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false)
}
