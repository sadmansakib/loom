//! Local-only git excludes so machine state is never staged.

use std::path::Path;
/// Best-effort: appends `entries` to the repository's local-only
/// `.git/info/exclude` (creating it if absent) so machine-local Loom
/// state is never staged by a blanket `git add -A` — best-of worktree
/// dirs otherwise land in the index as broken gitlinks (160000), and
/// rewind snapshot patches would ride into the operator's next commit.
/// Never touches the tracked `.gitignore`: manifests
/// (`.loom/commands.toml`, `mcp.toml`, `mcp.lock`) stay shareable; only
/// generated state is excluded. Failures are warned and swallowed —
/// this is hygiene, not a gate (review item 2.8).
pub fn exclude_state_from_git(workspace: &Path, entries: &[&str]) {
    let output = std::process::Command::new("git")
        .args(crate::hostgit::CONFIG_ARGS)
        .env(
            crate::hostgit::ENV_NOSYSTEM.0,
            crate::hostgit::ENV_NOSYSTEM.1,
        )
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--path-format=absolute", "--git-dir"])
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if git_dir.is_empty() {
        return;
    }
    let exclude = std::path::Path::new(&git_dir).join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|line| line.trim() == *entry))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut body = existing;
    if !body.ends_with('\n') && !body.is_empty() {
        body.push('\n');
    }
    if body.is_empty() {
        body.push_str("# local excludes written by loom (session state, never committed)\n");
    }
    for entry in missing {
        body.push_str(entry);
        body.push('\n');
    }
    if let Err(error) =
        std::fs::create_dir_all(exclude.parent().unwrap_or(std::path::Path::new("")))
    {
        tracing::warn!(%error, "cannot create .git/info dir for local excludes");
        return;
    }
    if let Err(error) = std::fs::write(&exclude, body) {
        tracing::warn!(%error, path = %exclude.display(), "cannot write local git excludes");
    }
}
