//! Structured `git` tools (iteration 4 open list): `git_status`,
//! `git_diff`, `git_log`.
//!
//! `run_command` already runs git and even reduces its output (item 3),
//! but through the generic exec path: whatever the user's `~/.gitconfig`
//! says (aliases, colors, fsmonitor hooks) changes the bytes, quoting of
//! non-ASCII paths depends on `core.quotepath`, and the call is
//! classified as a mutating sandbox command. These tools pin the
//! invocation instead: a fixed global flag set (`--no-optional-locks` so
//! `status` never touches the index, `core.quotepath=false` so paths
//! arrive literally, colors and advice off) makes output deterministic
//! regardless of environment; the tools are read-only (scout-registry
//! eligible); path arguments are confined to the workspace; and every
//! invocation still lands in the transcript as a
//! [`CommandEffect`](loom_kernel::tool::CommandEffect) audit record.
//!
//! Scope is deliberately read-only porcelain — the agent inspects
//! history and workspace state, it does not create commits. Non-zero
//! exits (not a repository, bad revision) are shown as content with the
//! exit status, the same contract `run_command` uses, so the model can
//! self-correct in one turn.
//!
//! Registration is gated (`EngineConfig::git`, default off) so the
//! frozen SWE-bench prompt bytes are unchanged until a frontend opts in.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use futures::future::BoxFuture;
use loom_kernel::tool::{
    CommandEffect, PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::confinement::resolve_within;

/// Global flags prepended to every invocation. `--no-optional-locks`
/// keeps even `status` from writing the index; the `-c` values make
/// output environment-independent.
const GIT_GLOBALS: [&str; 7] = [
    "--no-optional-locks",
    "-c",
    "core.quotepath=false",
    "-c",
    "color.ui=never",
    "-c",
    "advice.detachedHead=false",
];

/// Default and cap for `git_log`'s `limit`.
const LOG_LIMIT_DEFAULT: u32 = 20;
const LOG_LIMIT_MAX: u32 = 200;

/// One bounded git invocation: globals + subcommand args, default
/// sandbox grants (read-only needs no escalation), default stream
/// windows, audit record. Non-zero exits are content, not errors.
async fn run_git(
    ctx: &ToolContext,
    subcommand: &str,
    args: &[String],
) -> Result<(String, Option<i32>), ToolError> {
    let mut argv: Vec<String> = GIT_GLOBALS.iter().map(|&g| g.to_owned()).collect();
    argv.push(subcommand.to_owned());
    argv.extend(args.iter().cloned());
    let mut grants = crate::sandbox::SandboxGrants::from_rule(None);
    // git's diff plumbing opens /dev/null read-write (the stand-in for
    // an absent file side); the blanket Seatbelt write-deny blocks the
    // character device, so grant exactly that sink — nothing else.
    grants
        .extra_writable
        .push(std::path::PathBuf::from("/dev/null"));
    let sandbox = crate::sandbox::Sandbox::resolve(&ctx.workspace_root, grants);
    let outcome = crate::exec::run_bounded(
        "git",
        &argv,
        ctx,
        &crate::exec::ExecLimits::default(),
        &sandbox,
    )
    .await?;
    let mut combined = String::from_utf8_lossy(&outcome.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&outcome.output.stderr).into_owned();
    if !stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Ok((combined, outcome.output.status.code()))
}

/// Confines an optional path argument to the workspace and returns the
/// `--` separator form git wants, or an `InvalidInput` error naming the
/// problem.
fn confined_path(ctx: &ToolContext, raw: &str) -> Result<String, ToolError> {
    let resolved: PathBuf = resolve_within(&ctx.workspace_root, raw)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let rel = resolved
        .strip_prefix(&ctx.workspace_root)
        .unwrap_or(&resolved);
    Ok(rel.to_string_lossy().into_owned())
}

/// Renders an exit status line when the command did not succeed.
fn status_line(exit: Option<i32>) -> String {
    match exit {
        Some(0) | None => String::new(),
        Some(code) => format!("\nexit status: {code}"),
    }
}

// ---------------------------------------------------------------------------
// git_status
// ---------------------------------------------------------------------------

/// Reports branch and workspace changes (`git_status`).
#[derive(Debug)]
pub struct GitStatusTool;

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStatusInput {}

impl Tool for GitStatusTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "git_status".to_owned(),
            description: "Shows the current branch and every workspace \
                          change git reports (staged, unstaged, untracked), \
                          with deterministic formatting. Read-only; call it \
                          before diffing or patching to see the workspace \
                          state at a glance."
                .to_owned(),
            parameters: crate::schema::parameters::<GitStatusInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some("git_status takes no arguments.".to_owned());
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let _ = serde_json::from_value::<GitStatusInput>(input.clone())
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            let (raw, exit) = run_git(
                ctx,
                "status",
                &["--porcelain=v1".into(), "-z".into(), "--branch".into()],
            )
            .await?;
            let mut out = String::new();
            if exit.unwrap_or(0) == 0 {
                let parsed = parse_porcelain(&raw);
                let _ = writeln!(out, "branch: {}", parsed.branch);
                if parsed.entries.is_empty() {
                    let _ = writeln!(out, "clean: no changes");
                } else {
                    for entry in &parsed.entries {
                        let _ = writeln!(out, "{}", entry);
                    }
                }
            } else {
                // Surface git's own complaint (usually "not a git
                // repository") plus the exit status.
                out.push_str(&raw);
                out.push_str(&status_line(exit));
            }
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                "git",
                vec!["status".to_owned()],
                exit,
            ));
            Ok(ToolOutput::new(out).with_effects(effects))
        })
    }
}

struct Porcelain {
    branch: String,
    /// One `--porcelain=v1 -z` record per line, already human-rendered.
    entries: Vec<String>,
}

/// Parses `git status --porcelain=v1 -z --branch` output. Records are
/// NUL-separated; the optional `## branch` header comes first; rename
/// and copy entries carry the original path as the next record.
fn parse_porcelain(raw: &str) -> Porcelain {
    let mut branch = String::from("HEAD");
    let mut entries = Vec::new();
    let records: Vec<&str> = raw.split('\0').filter(|r| !r.is_empty()).collect();
    let mut iter = records.iter().peekable();
    if let Some(rest) = iter.peek().and_then(|f| f.strip_prefix("## ")) {
        // `HEAD (no branch)` and `main...origin/main` both reduce to
        // the bare name.
        let bare = rest.split("...").next().unwrap_or(rest);
        branch = bare.trim_end_matches(" (no branch)").to_owned();
        iter.next();
    }
    while let Some(record) = iter.next() {
        if record.len() < 4 {
            continue;
        }
        let xy = &record[..2];
        let path = &record[3..];
        // Rename (R) and copy (C) statuses are followed by the original
        // path in its own record.
        let rendered = if xy.starts_with('R') || xy.starts_with('C') {
            let orig = iter.next().copied().unwrap_or("");
            format!("{xy} {path} <- {orig}")
        } else {
            format!("{xy} {path}")
        };
        entries.push(rendered);
    }
    Porcelain { branch, entries }
}

// ---------------------------------------------------------------------------
// git_diff
// ---------------------------------------------------------------------------

/// Shows workspace changes as a patch (`git_diff`).
#[derive(Debug)]
pub struct GitDiffTool;

#[derive(Debug, Deserialize, JsonSchema)]
struct GitDiffInput {
    /// Workspace-relative file or directory to diff. Omit for the whole
    /// workspace.
    #[serde(default)]
    path: Option<String>,
    /// Diff the staged (index) changes against `HEAD` instead of the
    /// worktree.
    #[serde(default)]
    staged: bool,
}

impl Tool for GitDiffTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "git_diff".to_owned(),
            description: "Shows changes as a unified diff. By default the \
                          worktree against HEAD (unstaged plus staged); \
                          `staged: true` limits to the index. `path` \
                          scopes to one file or directory. Untracked files \
                          never appear in a diff — see git_status for them."
                .to_owned(),
            parameters: crate::schema::parameters::<GitDiffInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        match serde_json::from_value::<GitDiffInput>(input.clone()) {
            Ok(parsed) => parsed.path.map(|p| vec![PathTouch::read(p)]),
            Err(_) => None,
        }
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "git_diff takes an optional workspace-relative `path`, an \
                 optional `staged: true`, and nothing else. Paths outside \
                 the workspace are rejected."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let parsed = serde_json::from_value::<GitDiffInput>(input.clone())
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            let mut args: Vec<String> = vec!["--no-color".into()];
            if parsed.staged {
                args.push("--staged".into());
            }
            if let Some(raw) = &parsed.path {
                let rel = confined_path(ctx, raw)?;
                args.push("--".into());
                args.push(rel);
            }
            let (raw, exit) = run_git(ctx, "diff", &args).await?;
            let mut out = raw;
            if exit.unwrap_or(0) == 0 && out.trim().is_empty() && parsed.path.is_none() {
                out = "(no differences)\n".to_owned();
            }
            out.push_str(&status_line(exit));
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                "git",
                vec!["diff".to_owned()],
                exit,
            ));
            Ok(ToolOutput::new(out).with_effects(effects))
        })
    }
}

// ---------------------------------------------------------------------------
// git_log
// ---------------------------------------------------------------------------

/// Lists commits (`git_log`).
#[derive(Debug)]
pub struct GitLogTool;

#[derive(Debug, Deserialize, JsonSchema)]
struct GitLogInput {
    /// Workspace-relative file or directory whose history to list.
    /// Omit for the whole repository.
    #[serde(default)]
    path: Option<String>,
    /// Maximum commits to list. Default 20, capped at 200.
    #[serde(default)]
    limit: Option<u32>,
}

impl Tool for GitLogTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "git_log".to_owned(),
            description: "Lists recent commits, newest first, one per line \
                          (`hash subject`), optionally scoped to one path's \
                          history. Use it to find the revision that \
                          introduced or last touched code before diffing \
                          or blaming."
                .to_owned(),
            parameters: crate::schema::parameters::<GitLogInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        match serde_json::from_value::<GitLogInput>(input.clone()) {
            Ok(parsed) => parsed.path.map(|p| vec![PathTouch::read(p)]),
            Err(_) => None,
        }
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "git_log takes an optional workspace-relative `path` and an \
                 optional integer `limit` (default 20, max 200)."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let parsed = serde_json::from_value::<GitLogInput>(input.clone())
                .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
            let limit = parsed
                .limit
                .unwrap_or(LOG_LIMIT_DEFAULT)
                .clamp(1, LOG_LIMIT_MAX);
            let mut args: Vec<String> = vec![
                "--oneline".into(),
                "--no-decorate".into(),
                format!("-n{limit}"),
            ];
            if let Some(raw) = &parsed.path {
                let rel = confined_path(ctx, raw)?;
                args.push("--".into());
                args.push(rel);
            }
            let (raw, exit) = run_git(ctx, "log", &args).await?;
            let mut out = raw;
            if exit.unwrap_or(0) == 0 && out.trim().is_empty() {
                out = "(no commits)\n".to_owned();
            }
            out.push_str(&status_line(exit));
            let effects = ToolEffects::none().with_command(CommandEffect::new(
                "git",
                vec!["log".to_owned()],
                exit,
            ));
            Ok(ToolOutput::new(out).with_effects(effects))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_in(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(
            dir.to_owned(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_secs(30),
        )
    }

    fn repo(dir: &std::path::Path) -> std::path::PathBuf {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git spawns")
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        dir.to_owned()
    }

    #[test]
    fn porcelain_parses_branch_entries_and_renames() {
        let raw = "## main...origin/main\0M  src/a.rs\0R  src/new.rs\0src/old.rs\0?? notes.txt\0";
        let parsed = parse_porcelain(raw);
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.entries[0], "M  src/a.rs");
        assert_eq!(parsed.entries[1], "R  src/new.rs <- src/old.rs");
        assert_eq!(parsed.entries[2], "?? notes.txt");
    }

    #[test]
    fn porcelain_handles_detached_head_and_empty() {
        let parsed = parse_porcelain("");
        assert_eq!(parsed.branch, "HEAD");
        assert!(parsed.entries.is_empty());
        let parsed = parse_porcelain("## HEAD (no branch)\0 M x\0");
        assert_eq!(parsed.branch, "HEAD");
        assert_eq!(parsed.entries.len(), 1);
    }

    #[tokio::test]
    async fn status_diff_log_over_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("loom-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        repo(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("u.txt"), "new\n").unwrap();

        let status = GitStatusTool
            .call(serde_json::json!({}), &ctx_in(&dir))
            .await
            .unwrap();
        assert!(
            status.content.contains("branch: main"),
            "{}",
            status.content
        );
        assert!(status.content.contains(" M a.txt"), "{}", status.content);
        assert!(status.content.contains("?? u.txt"), "{}", status.content);

        let diff = GitDiffTool
            .call(serde_json::json!({ "path": "a.txt" }), &ctx_in(&dir))
            .await
            .unwrap();
        assert!(diff.content.contains("+two"), "{}", diff.content);

        let log = GitLogTool
            .call(serde_json::json!({ "limit": 5 }), &ctx_in(&dir))
            .await
            .unwrap();
        assert!(log.content.contains("first"), "{}", log.content);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn status_outside_a_repository_reports_rather_than_errors() {
        let dir = std::env::temp_dir().join(format!("loom-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = GitStatusTool
            .call(serde_json::json!({}), &ctx_in(&dir))
            .await
            .unwrap();
        assert!(out.content.contains("exit status:"), "{}", out.content);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn diff_rejects_paths_outside_the_workspace() {
        let dir = std::env::temp_dir().join(format!("loom-gitconf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        repo(&dir);
        let err = GitDiffTool
            .call(serde_json::json!({ "path": "../../etc" }), &ctx_in(&dir))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn log_limit_is_clamped() {
        let dir = std::env::temp_dir().join(format!("loom-gitlim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        repo(&dir);
        std::fs::write(dir.join("f"), "x\n").unwrap();
        for i in 0..3 {
            let _ = std::process::Command::new("git")
                .args(["commit", "--quiet", "--allow-empty", "-m", &format!("c{i}")])
                .current_dir(&dir)
                .output()
                .unwrap();
        }
        let out = GitLogTool
            .call(serde_json::json!({ "limit": 0 }), &ctx_in(&dir))
            .await
            .unwrap();
        // clamp(1, 200) turns 0 into 1: exactly one commit line.
        assert_eq!(out.content.lines().filter(|l| !l.is_empty()).count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
