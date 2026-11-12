//! Hardened `git` invocation helpers over [`crate::hostgit`].

use std::path::Path;
use std::process::Stdio;

use super::config::WorktreeError;
/// Runs `git` in `dir`, returning raw stdout. Non-zero exits carry the
/// stderr tail in the error. Every invocation is hardened
/// ([`crate::hostgit`]): repo config cannot name programs to execute.
pub(super) async fn git_raw(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = tokio::process::Command::new("git")
        .args(crate::hostgit::CONFIG_ARGS)
        .args(crate::hostgit::neutralize_filters(dir))
        .env(
            crate::hostgit::ENV_NOSYSTEM.0,
            crate::hostgit::ENV_NOSYSTEM.1,
        )
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::new(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        )))
    }
}

/// Runs `git` in `dir`, returning trimmed stdout. Non-zero exits carry the
/// stderr tail in the error.
pub(super) async fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    Ok(git_raw(dir, args).await?.trim().to_owned())
}

/// Runs `git` in `dir` feeding `stdin` to it.
pub(super) async fn git_with_stdin(
    dir: &Path,
    args: &[&str],
    stdin: &str,
) -> Result<String, WorktreeError> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("git")
        .args(crate::hostgit::CONFIG_ARGS)
        .args(crate::hostgit::neutralize_filters(dir))
        .env(
            crate::hostgit::ENV_NOSYSTEM.0,
            crate::hostgit::ENV_NOSYSTEM.1,
        )
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes())
            .await
            .map_err(|err| WorktreeError::new(format!("git stdin: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|err| WorktreeError::new(format!("git {}: {err}", args.first().unwrap_or(&""))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(WorktreeError::new(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        )))
    }
}
