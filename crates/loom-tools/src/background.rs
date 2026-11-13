//! Background commands (roadmap item 13): long builds and tests run
//! detached while the agent keeps working, and a completion notice is
//! injected at the next turn boundary instead of blocking a turn.
//!
//! Interactive-only by construction: the engine registers these tools
//! only when the frontend opts in ([`EngineConfig::background_commands`]
//! in `loom-engine`), so the headless run path and every eval harness
//! keep their byte-stable tool list and replay equivalence. The
//! deferral analysis (2026-10-09) showed detaching buys nothing
//! headless — the command result is the input to the next decision —
//! but interactive wall time is a real lever: a user queues work while
//! builds run.
//!
//! The mechanism reuses the foreground contract exactly:
//! - the same [`CommandPolicy`](crate::policy::CommandPolicy) verdict
//!   layer (deny/ask/allow) and the same OS sandbox with per-rule
//!   grants — a background job is never *more* privileged than a
//!   foreground one;
//! - the same bounded exec ([`crate::exec::run_bounded`]): the child
//!   leads its own process group and the whole tree is killed when the
//!   job future is dropped, so detaching never leaks grandchildren;
//! - the same output window (head+tail capped streams).
//!
//! Completion delivery rides the steering queue (item 25): the engine
//! drains finished jobs and calls `TaskControl::steer`, which the
//! kernel appends at the next turn boundary — the model sees the exit
//! status and output tail without polling, and `command_status` gives
//! it a pull path for anything still running. Job state outlives a
//! single task: the registry is engine-owned, so a build started in
//! one chat task reports in the next.
//!
//! Audit: the *start* call records a `CommandEffect` with no exit code
//! (the process had not finished), so the rewind unrestorable-command
//! gate stays honest — the workspace may have been mutated. Dropping
//! the registry (engine close) kills every still-running group.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use loom_kernel::tool::{
    CommandEffect, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::policy::{AskResolver, CommandPolicy, PolicyVerdict};

/// Cap on concurrently running background jobs. Detached work is a
/// wall-time lever, not a parallelism lever; past this the model gets
/// a legible refusal naming the count (it can wait via
/// `command_status` — the same discipline the tool cache holds).
const MAX_CONCURRENT_JOBS: usize = 4;

/// The engine-owned registry of detached commands.
///
/// `Arc<BackgroundJobs>` is shared by the two tools and the engine;
/// the last drop aborts every still-running job future, and aborting
/// the future fires exec's group-kill guard — no orphaned builds
/// outlive the engine.
#[derive(Debug, Default)]
pub struct BackgroundJobs {
    inner: Arc<JobsInner>,
}

#[derive(Debug, Default)]
struct JobsInner {
    jobs: Mutex<Vec<Arc<Job>>>,
    next_id: AtomicU32,
}

impl Clone for BackgroundJobs {
    fn clone(&self) -> Self {
        // Cloning shares the registry (the tools and the engine must
        // see one job table), so clone reborrows the same inner Arc.
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// One detached command's lifecycle.
#[derive(Debug)]
struct Job {
    id: u32,
    program: String,
    args: Vec<String>,
    started: Instant,
    state: Mutex<JobState>,
    /// Set once the completion notice has been drained, so a job
    /// reports exactly once through the steering queue.
    notified: AtomicBool,
    /// The detached exec task; aborting it kills the process group.
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug)]
enum JobState {
    Running,
    Finished { exit: Option<i32>, output: String },
}

/// A point-in-time view of one job, for `command_status`.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: u32,
    pub program: String,
    pub args: Vec<String>,
    pub running: bool,
    pub exit: Option<i32>,
    pub elapsed: Duration,
    /// The bounded head+tail output window (empty while running).
    pub output: String,
}

impl BackgroundJobs {
    /// Spawns one detached command under the already-resolved sandbox
    /// and limits (the tool applied the policy verdict before calling
    /// this). Returns the job id.
    fn spawn_job(
        &self,
        program: &str,
        args: Vec<String>,
        workspace_root: std::path::PathBuf,
        artifacts: Option<Arc<dyn loom_kernel::store::ArtifactStore>>,
        prepared: PreparedExec,
    ) -> u32 {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let job = Arc::new(Job {
            id,
            program: program.to_owned(),
            args,
            started: Instant::now(),
            state: Mutex::new(JobState::Running),
            notified: AtomicBool::new(false),
            handle: Mutex::new(None),
        });
        // A synthetic context: the job must NOT ride the call's
        // cancellation token (a /cancel stops the task, not the
        // build), and no per-call timeout applies — the rule's own
        // `timeout_secs`, when present, is enforced below.
        let ctx = ToolContext::new(
            workspace_root,
            CancellationToken::new(),
            Duration::from_secs(24 * 60 * 60),
        )
        .with_artifacts(artifacts);
        let exec_job = Arc::clone(&job);
        let exec_program = program.to_owned();
        let PreparedExec {
            limits,
            sandbox,
            rule_timeout,
        } = prepared;
        let handle = tokio::spawn(async move {
            let bounded =
                crate::exec::run_bounded(&exec_program, &exec_job.args, &ctx, &limits, &sandbox);
            let outcome = match rule_timeout {
                None => bounded.await.ok(), // None = spawn failed; recorded below
                Some(secs) => {
                    match tokio::time::timeout(Duration::from_secs(secs), bounded).await {
                        Ok(Ok(outcome)) => Some(outcome),
                        Ok(Err(_)) => None,
                        // The elapsed future was dropped, so the group-kill
                        // guard already reaped the tree — mirror the
                        // foreground timeout message.
                        Err(_elapsed) => {
                            finish(
                                &exec_job,
                                None,
                                format!(
                                    "command \"{exec_program}\" timed out after {secs}s \
                                     (process group killed)"
                                ),
                            );
                            return;
                        }
                    }
                }
            };
            match outcome {
                Some(outcome) => {
                    let body = completion_body(&exec_program, &exec_job.args, &outcome);
                    finish(&exec_job, outcome.output.status.code(), body);
                }
                None => {
                    finish(&exec_job, None, format!("cannot run \"{exec_program}\""));
                }
            }
        });
        *job.handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.push(job);
        id
    }

    /// How many jobs are still running.
    fn running_count(&self) -> usize {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|job| {
                matches!(
                    &*job
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    JobState::Running
                )
            })
            .count()
    }

    /// Snapshot of one job, if the id is known.
    fn snapshot(&self, id: u32) -> Option<JobSnapshot> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|job| job.id == id)
            .map(snapshot_of)
    }

    /// One-line status for every job, oldest first.
    fn list(&self) -> Vec<String> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|job| {
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*state {
                    JobState::Running => format!(
                        "job {}: $ {} {} — running ({:.0}s)",
                        job.id,
                        job.program,
                        job.args.join(" "),
                        job.started.elapsed().as_secs_f32()
                    ),
                    JobState::Finished { exit, .. } => format!(
                        "job {}: $ {} {} — finished (exit status: {})",
                        job.id,
                        job.program,
                        job.args.join(" "),
                        exit.map_or_else(|| "signal".to_owned(), |code| code.to_string())
                    ),
                }
            })
            .collect()
    }

    /// Drains completion notices not yet reported through the steering
    /// queue. Each notice is one steering message the kernel appends
    /// at the next turn boundary.
    #[must_use]
    pub fn take_finished_notices(&self) -> Vec<String> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut notices = Vec::new();
        for job in jobs.iter() {
            if job.notified.load(Ordering::Relaxed) {
                continue;
            }
            let state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let JobState::Finished { exit, output } = &*state {
                // Only claim the once-flag when a notice is actually
                // emitted — a poll while the job is still running must
                // not burn the report.
                job.notified.store(true, Ordering::Relaxed);
                notices.push(format!(
                    "[background] job {} finished — $ {} {}: exit status: {}\n{}",
                    job.id,
                    job.program,
                    job.args.join(" "),
                    exit.map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                    output
                ));
            }
        }
        notices
    }
}

impl Drop for JobsInner {
    fn drop(&mut self) {
        // Last registry reference gone (engine closed): abort every
        // running job future. The abort drops run_bounded, whose
        // group-kill guard SIGKILLs the process group — the same
        // reaping a foreground timeout performs.
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            if let Some(handle) = job
                .handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                handle.abort();
            }
        }
    }
}

fn finish(job: &Job, exit: Option<i32>, output: String) {
    *job.state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = JobState::Finished { exit, output };
}

fn snapshot_of(job: &Arc<Job>) -> JobSnapshot {
    let state = job
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (running, exit, output) = match &*state {
        JobState::Running => (true, None, String::new()),
        JobState::Finished { exit, output } => (false, *exit, output.clone()),
    };
    JobSnapshot {
        id: job.id,
        program: job.program.clone(),
        args: job.args.clone(),
        running,
        exit,
        elapsed: job.started.elapsed(),
        output,
    }
}

/// The completion notice body: the same first line the foreground
/// render produces, plus the bounded streams — the model reads it the
/// way it reads a `run_command` result.
fn completion_body(program: &str, args: &[String], outcome: &crate::exec::ExecOutcome) -> String {
    let mut text = format!("$ {program}");
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text.push('\n');
    let _ = write!(
        text,
        "exit status: {}",
        outcome.output.status.code().map_or_else(
            || "terminated by signal".to_owned(),
            |code| code.to_string()
        )
    );
    let stdout = String::from_utf8_lossy(&outcome.output.stdout);
    let stderr = String::from_utf8_lossy(&outcome.output.stderr);
    if !stdout.is_empty() {
        text.push_str("\n--- stdout ---\n");
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        text.push_str("\n--- stderr ---\n");
        text.push_str(&stderr);
    }
    if outcome.pipe_hold_truncated {
        text.push_str("\n[output truncated: a descendant held the pipes after exit]");
    }
    text
}

/// The policy verdict + PATH + rule/limits/sandbox resolution shared
/// with the foreground tool. Extracted verbatim from `run_command`'s
/// call path so the two tools can never disagree about privilege.
pub(crate) struct PreparedExec {
    pub limits: crate::exec::ExecLimits,
    pub sandbox: crate::sandbox::Sandbox,
    pub rule_timeout: Option<u64>,
}

pub(crate) async fn prepare_exec(
    policy: &CommandPolicy,
    ask: Option<&AskResolver>,
    program: &str,
    args: &[String],
    workspace_root: &std::path::Path,
) -> Result<PreparedExec, ToolError> {
    match policy.verdict_for_argv(program, args) {
        PolicyVerdict::Allow => {}
        PolicyVerdict::Deny => {
            let wrapped = policy.wrapped_denied_name(program, args);
            return Err(ToolError::InvalidInput(format!(
                "program \"{program}\" is denied by workspace policy (.loom/commands.toml){}; \
                 pick another approach",
                wrapped.map_or_else(String::new, |name| format!(
                    " — it wraps the denied program \"{name}\""
                )),
            )));
        }
        PolicyVerdict::Ask => {
            let approved = match ask {
                Some(resolver) => {
                    resolver(crate::policy::AskRequest {
                        program: program.to_owned(),
                        args: args.to_vec(),
                        sandboxed: policy.rule(program).is_none_or(|rule| rule.sandbox),
                        network: policy.rule(program).is_some_and(|rule| rule.network),
                    })
                    .await
                }
                None => false,
            };
            if !approved {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{program}\" needs operator approval (policy unknown = \"ask\") \
                     and was not approved; pick another approach",
                )));
            }
        }
    }
    if crate::run_command::resolve_in_path(program).is_none() {
        return Err(ToolError::Execution(format!(
            "cannot run \"{program}\": not found on PATH (no shell here: pass an \
             executable name and separate `args`; there is no `cd`, so use \
             workspace-relative paths)",
        )));
    }
    let rule = policy.rule(program);
    let limits = crate::exec::ExecLimits {
        stream_window: rule.and_then(|rule| rule.max_output_bytes),
        scrub: rule.map(|rule| rule.env_scrub.clone()).unwrap_or_default(),
        env_allow: rule.map(|rule| rule.env_allow.clone()).unwrap_or_default(),
    };
    let grants = crate::sandbox::SandboxGrants::from_rule(rule);
    let sandbox = crate::sandbox::Sandbox::resolve(workspace_root, grants);
    Ok(PreparedExec {
        limits,
        sandbox,
        rule_timeout: rule.and_then(|rule| rule.timeout_secs),
    })
}

/// Starts a detached command (roadmap item 13). Interactive surfaces
/// only — the engine does not register this tool otherwise.
#[derive(Clone)]
pub struct RunBackgroundTool {
    policy: CommandPolicy,
    jobs: BackgroundJobs,
    spec: ToolSpec,
    ask: Option<AskResolver>,
}

impl RunBackgroundTool {
    /// Builds the detached-start tool around a shared job registry and
    /// the command policy every foreground command already follows.
    #[must_use]
    pub fn new(policy: CommandPolicy, jobs: BackgroundJobs) -> Self {
        let description = format!(
            "Starts a program detached in the workspace (same OS sandbox and \
             .loom/commands.toml policy as run_command) and returns \
             immediately with a job id. Use it for long builds and test \
             runs: the command keeps running while you work, and its exit \
             status and output tail arrive as a [background] notice at \
             your next turn boundary — no turn is spent waiting. Poll \
             anything still running with command_status. At most \
             {MAX_CONCURRENT_JOBS} jobs run at once. Escalated, grants \
             beyond the sandbox default: {}.",
            policy
                .allowed_names()
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        Self {
            spec: ToolSpec {
                name: "run_background".to_owned(),
                description,
                parameters: crate::schema::parameters::<BackgroundInput>(),
            },
            policy,
            jobs,
            ask: None,
        }
    }

    /// Wires the interactive ask resolver, mirroring `run_command`.
    #[must_use]
    pub fn with_ask_resolver(mut self, ask: AskResolver) -> Self {
        self.ask = Some(ask);
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct BackgroundInput {
    /// Executable name, no path; runs inside the sandbox unless the
    /// policy escalates it.
    program: String,
    /// Arguments, one array entry each.
    #[serde(default)]
    args: Vec<String>,
}

impl Tool for RunBackgroundTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        // ToolFailure is #[non_exhaustive]: the wildcard arm is required.
        #[allow(clippy::wildcard_enum_match_arm)]
        match failure {
            loom_kernel::transcript::ToolFailure::InvalidInput => Some(
                "expected fields: program (bare executable name), args (array of strings)"
                    .to_owned(),
            ),
            _ => None,
        }
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: BackgroundInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            if input.program.contains('/') || input.program.contains('\\') {
                return Err(ToolError::InvalidInput(format!(
                    "program \"{}\" must be a bare executable name, not a path",
                    input.program
                )));
            }
            if self.jobs.running_count() >= MAX_CONCURRENT_JOBS {
                return Err(ToolError::InvalidInput(format!(
                    "at most {MAX_CONCURRENT_JOBS} background jobs run at once; check the \
                     running ones with command_status and start this again once one finishes",
                )));
            }
            let prepared = prepare_exec(
                &self.policy,
                self.ask.as_ref(),
                &input.program,
                &input.args,
                &ctx.workspace_root,
            )
            .await?;
            let rule_timeout = prepared.rule_timeout;
            let id = self.jobs.spawn_job(
                &input.program,
                input.args.clone(),
                ctx.workspace_root.clone(),
                ctx.artifacts.clone(),
                prepared,
            );
            // The start is the audited mutation: a command effect with
            // no exit code yet, so the rewind unrestorable gate sees
            // that the workspace may have been changed.
            let mut text = format!(
                "started job {id}: $ {} {}\nrunning detached; the exit status and output \
                 tail arrive as a [background] notice at the next turn boundary — keep \
                 working, or poll with command_status",
                input.program,
                input.args.join(" ")
            );
            if let Some(secs) = rule_timeout {
                let _ = write!(
                    text,
                    "\n(wall cap {secs}s from the workspace policy — the group is killed at \
                     the cap)"
                );
            }
            Ok(ToolOutput::new(text).with_effects(
                ToolEffects::none().with_command(CommandEffect::new(
                    &input.program,
                    input.args,
                    None,
                )),
            ))
        })
    }
}

/// Read-only status over the background job table.
#[derive(Clone)]
pub struct CommandStatusTool {
    jobs: BackgroundJobs,
    spec: ToolSpec,
}

impl CommandStatusTool {
    /// Builds the read-only status tool over the same job registry.
    #[must_use]
    pub fn new(jobs: BackgroundJobs) -> Self {
        Self {
            spec: ToolSpec {
                name: "command_status".to_owned(),
                description: format!(
                    "Lists background jobs started with run_background (one line each: id, \
                     command, state), or shows one job in detail — running for how long, or \
                     finished with its exit status and bounded output. Read-only; use it \
                     before starting another job (at most {MAX_CONCURRENT_JOBS} run at once)."
                ),
                parameters: crate::schema::parameters::<StatusInput>(),
            },
            jobs,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StatusAction {
    /// One line per background job.
    List,
    /// Full detail for one job, including its bounded output.
    Show,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct StatusInput {
    /// `list` summarizes every background job; `show` returns one job in
    /// detail.
    action: StatusAction,
    /// Job id to show (required when `action` is `show`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u32>,
}

impl Tool for CommandStatusTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: StatusInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            match (input.action, input.id) {
                (StatusAction::List, _) => {
                    let lines = self.jobs.list();
                    if lines.is_empty() {
                        return Ok(ToolOutput::new("no background jobs"));
                    }
                    Ok(ToolOutput::new(lines.join("\n")))
                }
                (StatusAction::Show, None) => Ok(ToolOutput::new(
                    "`show` needs the job `id`; use `list` to see the known ids",
                )),
                (StatusAction::Show, Some(id)) => match self.jobs.snapshot(id) {
                    None => Ok(ToolOutput::new(format!(
                        "no background job {id}; `list` shows the known ones"
                    ))),
                    Some(snapshot) => {
                        let mut text = format!(
                            "job {}: $ {} {}",
                            snapshot.id,
                            snapshot.program,
                            snapshot.args.join(" ")
                        );
                        if snapshot.running {
                            let _ = write!(
                                text,
                                "\nrunning for {:.0}s",
                                snapshot.elapsed.as_secs_f32()
                            );
                        } else {
                            let _ = write!(
                                text,
                                "\nfinished, exit status: {}",
                                snapshot
                                    .exit
                                    .map_or_else(|| "signal".to_owned(), |code| code.to_string())
                            );
                            if !snapshot.output.is_empty() {
                                text.push('\n');
                                text.push_str(&snapshot.output);
                            }
                        }
                        Ok(ToolOutput::new(text))
                    }
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
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
}
