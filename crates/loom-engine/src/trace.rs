//! Task trace export (roadmap item 20.1, phase 1).
//!
//! `--trace DIR` records everything one task produced, so an eval sweep
//! can keep the raw evidence next to its reports: one directory per
//! task holding
//!
//! * `events.ndjson` — the [`EventEnvelope`] stream, one JSON object per
//!   line, exactly as the sink numbered it;
//! * `transcript.json` — the committed session items and task records;
//! * `manifest.json` — harness version, run configuration, outcome, the
//!   [`TaskSummary`] counters, and the final context census.
//!
//! This module owns the export *format* so every frontend — CLI, TUI,
//! GUI — writes byte-identical traces; [`crate::trace_stats`] (and any
//! other consumer, human or tool) can then read any trace any frontend
//! produced. The driving loop stays in the frontend: it forwards
//! envelopes to [`TraceWriter::record`] and calls
//! [`TraceWriter::finish`] once the task ends.
//!
//! The writer is deliberately passive: it never fails the run. Write
//! errors are counted and surfaced in the manifest (`events_write_failures`)
//! rather than propagated, because a trace is an artifact of the run, not
//! a precondition for it.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;

use loom_kernel::agent::TaskSummary;
use loom_kernel::context::ContextCensus;
use loom_kernel::event::{EVENT_SCHEMA_VERSION, EventEnvelope};
use loom_kernel::id::TaskId;
use loom_kernel::session::Session;

use crate::ReasoningEffort;

/// Run configuration recorded into the manifest, supplied by the
/// frontend that drove the task.
pub struct TraceMeta<'a> {
    /// Provider adapter name (`deepseek`, `anthropic`, `openrouter`).
    pub provider: &'a str,
    /// Model id as the frontend resolved it.
    pub model: &'a str,
    /// Reasoning-effort knob the run was configured with.
    pub reasoning_effort: ReasoningEffort,
    /// Turn ceiling configured for the run.
    pub max_turns: u32,
    /// Whether the run also persisted a session store.
    pub persisted: bool,
}

/// How `run_task` terminated, distilled for the manifest.
pub enum Outcome {
    /// The loop completed; the engine's summary is authoritative.
    Completed,
    /// The loop ended in an error. `status` is the coarse bucket
    /// (`failed` / `cancelled`), `end_reason` the stable token from
    /// `TaskError::end_reason()`.
    Ended {
        /// Coarse terminal bucket.
        status: &'static str,
        /// Stable snake_case cause token.
        end_reason: String,
    },
}

/// The manifest's copy of [`TaskSummary`], separated so the export
/// format (and these tests) do not depend on kernel constructors for a
/// `#[non_exhaustive]` struct.
#[derive(Debug, Serialize)]
pub struct SummarySnapshot {
    /// Committed model turns.
    pub turns: u32,
    /// Model requests issued, including retried attempts.
    pub model_requests: u32,
    /// Prompt tokens summed over completed requests.
    pub input_tokens: u64,
    /// Generated tokens summed over completed requests.
    pub output_tokens: u64,
    /// Cached prompt tokens, when the provider reported them.
    pub cached_input_tokens: Option<u64>,
    /// Tool calls that succeeded.
    pub successful_tool_calls: u32,
    /// Tool calls that failed.
    pub failed_tool_calls: u32,
    /// Wall time of the task in milliseconds.
    pub wall_time_millis: u64,
    /// Events the sink dropped (closed receiver).
    pub events_dropped: u64,
    /// Token census of the final compiled request.
    pub last_context_census: CensusSnapshot,
}

/// The manifest's copy of the final [`ContextCensus`].
#[derive(Debug, Serialize)]
pub struct CensusSnapshot {
    /// The static system prompt.
    pub system_prompt: u32,
    /// The repository facts block.
    pub repo_facts: u32,
    /// Task and user items in the projection.
    pub user_items: u32,
    /// Replayed assistant history, excluding reasoning.
    pub assistant_history: u32,
    /// Replayed reasoning traces.
    pub reasoning_replay: u32,
    /// Tool results replayed from the transcript.
    pub tool_results: u32,
    /// Exposed tool definitions.
    pub tool_specs: u32,
    /// Authoritative total the budget check compares.
    pub total: u32,
}

impl From<&TaskSummary> for SummarySnapshot {
    fn from(summary: &TaskSummary) -> Self {
        Self {
            turns: summary.turns,
            model_requests: summary.model_requests,
            input_tokens: summary.input_tokens,
            output_tokens: summary.output_tokens,
            cached_input_tokens: summary.cached_input_tokens,
            successful_tool_calls: summary.successful_tool_calls,
            failed_tool_calls: summary.failed_tool_calls,
            wall_time_millis: summary.wall_time.as_millis() as u64,
            events_dropped: summary.events_dropped,
            last_context_census: CensusSnapshot::from(&summary.last_context_census),
        }
    }
}

impl From<&ContextCensus> for CensusSnapshot {
    fn from(census: &ContextCensus) -> Self {
        Self {
            system_prompt: census.system_prompt,
            repo_facts: census.repo_facts,
            user_items: census.user_items,
            assistant_history: census.assistant_history,
            reasoning_replay: census.reasoning_replay,
            tool_results: census.tool_results,
            tool_specs: census.tool_specs,
            total: census.total,
        }
    }
}

/// File names inside a task directory. Public because they are the
/// data contract: this module writes them, [`crate::trace_stats`]
/// reads them, and any external tool may rely on them.
pub const EVENTS_FILE_NAME: &str = "events.ndjson";
/// Where the manifest lives inside a task directory.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// Where the transcript lives inside a task directory.
pub const TRANSCRIPT_FILE_NAME: &str = "transcript.json";

/// Appends every event of one task to `<base>/<task_id>/events.ndjson`,
/// then writes `manifest.json` and `transcript.json` on [`finish`](Self::finish).
///
/// The task directory is created lazily on the first recorded event, so
/// a run that emitted nothing leaves no empty directories behind.
pub struct TraceWriter {
    /// Export root given via `--trace`.
    base: PathBuf,
    /// Set once the first envelope names its task.
    task_id: Option<TaskId>,
    /// Open handle to `events.ndjson`, or `None` until the first event
    /// (and permanently, if the file could not be created).
    events: Option<BufWriter<File>>,
    /// Envelopes successfully appended.
    events_written: u64,
    /// Appends that failed (I/O or serde); the run is unaffected.
    events_write_failures: u64,
}

impl TraceWriter {
    /// Creates a writer rooted at `base`; nothing touches disk yet.
    pub fn new(base: PathBuf) -> Self {
        Self {
            base,
            task_id: None,
            events: None,
            events_written: 0,
            events_write_failures: 0,
        }
    }

    /// Flushes buffered event lines to disk.
    ///
    /// [`finish`](Self::finish) flushes too; this exists so tests (and any
    /// future mid-run durability point) can force the log to disk.
    #[cfg(test)]
    fn flush(&mut self) {
        if let Some(file) = self.events.as_mut() {
            let _ = file.flush();
        }
    }

    /// Appends one envelope as an NDJSON line.
    ///
    /// The first envelope fixes the task directory; a later envelope
    /// naming a different task is counted as a failure rather than
    /// silently merged (a run drives exactly one task).
    pub fn record(&mut self, envelope: &EventEnvelope) {
        match self.task_id {
            None => {
                let dir = self.base.join(envelope.task_id.to_string());
                let opened = std::fs::create_dir_all(&dir)
                    .and_then(|()| {
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join(EVENTS_FILE_NAME))
                    })
                    .map(BufWriter::new)
                    .ok();
                self.events = opened;
                match self.events.is_some() {
                    true => self.task_id = Some(envelope.task_id),
                    false => self.events_write_failures += 1,
                }
            }
            Some(task) if task != envelope.task_id => {
                self.events_write_failures += 1;
                return;
            }
            Some(_) => {}
        }
        let Some(file) = self.events.as_mut() else {
            self.events_write_failures += 1;
            return;
        };
        let appended = serde_json::to_string(envelope)
            .map(|line| file.write_all(line.as_bytes()).is_ok() && file.write_all(b"\n").is_ok())
            .unwrap_or(false);
        match appended {
            true => self.events_written += 1,
            false => self.events_write_failures += 1,
        }
    }

    /// Writes `manifest.json` and `transcript.json` into the task
    /// directory and flushes the event log.
    ///
    /// `summary` is the engine's [`TaskSummary`] on success; nothing is
    /// written for a failed or cancelled run beyond the outcome block.
    /// Returns the task directory, or `None` when no event ever arrived
    /// (nothing was created and there is nothing to export).
    pub fn finish(
        mut self,
        session: &Session,
        meta: TraceMeta<'_>,
        outcome: Outcome,
        summary: Option<&TaskSummary>,
    ) -> std::io::Result<Option<PathBuf>> {
        let Some(task_id) = self.task_id else {
            return Ok(None);
        };
        let dir = self.base.join(task_id.to_string());

        if let Some(file) = self.events.as_mut() {
            file.flush()?;
        }
        let _ = self.events.take();

        std::fs::write(
            dir.join(MANIFEST_FILE_NAME),
            manifest(&self, session, &meta, &outcome, summary)?,
        )?;
        std::fs::write(dir.join(TRANSCRIPT_FILE_NAME), transcript(session)?)?;
        Ok(Some(dir))
    }
}

/// Builds the manifest body.
fn manifest(
    writer: &TraceWriter,
    session: &Session,
    meta: &TraceMeta<'_>,
    outcome: &Outcome,
    summary: Option<&TaskSummary>,
) -> std::io::Result<String> {
    let (status, end_reason): (&str, &str) = match outcome {
        Outcome::Completed => ("completed", "end_turn"),
        Outcome::Ended { status, end_reason } => (status, end_reason),
    };
    let mut body = json!({
        "loom_version": env!("CARGO_PKG_VERSION"),
        "event_schema_version": EVENT_SCHEMA_VERSION,
        "exported_at_millis": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "session_id": session.id.to_string(),
        "task_id": writer.task_id.map(|id| id.to_string()),
        "harness": {
            "provider": meta.provider,
            "model": meta.model,
            "reasoning_effort": meta.reasoning_effort.as_str(),
            "max_turns": meta.max_turns,
            "persisted": meta.persisted,
        },
        "outcome": {
            "status": status,
            "end_reason": end_reason,
        },
        "events_written": writer.events_written,
        "events_write_failures": writer.events_write_failures,
    });
    if let Some(summary) = summary {
        body["summary"] = json!(SummarySnapshot::from(summary));
    }
    to_pretty(&body)
}

/// Builds the transcript body: every committed item with its index, and
/// every task record, in stored order.
fn transcript(session: &Session) -> std::io::Result<String> {
    let items: Vec<_> = session
        .transcript
        .iter()
        .enumerate()
        .map(|(index, item)| json!({ "index": index as u32, "item": item }))
        .collect();
    let tasks = &session.tasks;
    let body = json!({
        "session_id": session.id.to_string(),
        "items": items,
        "tasks": tasks,
    });
    to_pretty(&body)
}

/// Serializes a JSON value; the only failure mode is I/O-free pretty
/// printing, which cannot fail for these shapes.
fn to_pretty(body: &serde_json::Value) -> std::io::Result<String> {
    serde_json::to_string_pretty(body)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::event::EventKind;
    use loom_kernel::id::SessionId;
    use loom_kernel::model::Usage;
    use loom_kernel::session::{Session, TaskRecord, TaskStatus};
    use loom_kernel::transcript::Item;

    fn envelope(sequence: u64, task_id: TaskId, kind: EventKind) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence,
            task_id,
            kind,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-trace-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn session_with_items() -> Session {
        let mut session = Session::new(SessionId::new());
        session
            .push_item(Item::User {
                text: "do it".into(),
            })
            .unwrap();
        session.push_task(TaskRecord {
            id: TaskId::new(),
            status: TaskStatus::Running,
            turns: 0,
            usage: Usage::default(),
            started_at_millis: 0,
            completed_at_millis: None,
            end_reason: None,
        });
        session
    }

    #[test]
    fn events_round_trip_and_dir_is_lazy() {
        let base = temp_dir("roundtrip");
        let task = TaskId::new();
        let mut writer = TraceWriter::new(base.clone());
        assert!(!base.exists(), "no directory before the first event");

        writer.record(&envelope(0, task, EventKind::TaskStarted));
        writer.record(&envelope(
            1,
            task,
            EventKind::TextDelta { text: "hi".into() },
        ));
        writer.record(&envelope(
            2,
            TaskId::new(),
            EventKind::TextDelta {
                text: "other task".into(),
            },
        ));

        writer.flush();
        let events_path = base.join(task.to_string()).join(EVENTS_FILE_NAME);
        let lines: Vec<_> = std::fs::read_to_string(&events_path)
            .unwrap()
            .lines()
            .map(serde_json::from_str::<EventEnvelope>)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].sequence, 0);
        assert_eq!(lines[1].sequence, 1);
        // The foreign-task envelope was refused, not merged.
        assert_eq!(writer.events_write_failures, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn finish_writes_manifest_and_transcript() {
        let base = temp_dir("finish");
        let task = TaskId::new();
        let mut writer = TraceWriter::new(base.clone());
        writer.record(&envelope(0, task, EventKind::TaskStarted));

        let session = session_with_items();
        let dir = writer
            .finish(
                &session,
                TraceMeta {
                    provider: "deepseek",
                    model: "deepseek-flash",
                    reasoning_effort: ReasoningEffort::High,
                    max_turns: 40,
                    persisted: true,
                },
                Outcome::Completed,
                None,
            )
            .unwrap()
            .expect("events were recorded");

        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(manifest["loom_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["event_schema_version"], EVENT_SCHEMA_VERSION);
        assert_eq!(manifest["task_id"], task.to_string());
        assert_eq!(manifest["harness"]["model"], "deepseek-flash");
        assert_eq!(manifest["harness"]["reasoning_effort"], "high");
        assert_eq!(manifest["outcome"]["status"], "completed");
        assert_eq!(manifest["outcome"]["end_reason"], "end_turn");
        assert!(manifest["summary"].is_null(), "no summary, no block");
        assert_eq!(manifest["events_written"], 1);

        let transcript: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(TRANSCRIPT_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(transcript["items"][0]["item"]["kind"], "user");
        assert_eq!(transcript["items"][0]["item"]["text"], "do it");
        assert_eq!(transcript["tasks"][0]["status"], "running");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn summary_snapshot_serializes_the_census() {
        let snapshot = SummarySnapshot {
            turns: 3,
            model_requests: 4,
            input_tokens: 1_000,
            output_tokens: 200,
            cached_input_tokens: Some(500),
            successful_tool_calls: 6,
            failed_tool_calls: 1,
            wall_time_millis: 2_500,
            events_dropped: 0,
            last_context_census: CensusSnapshot {
                system_prompt: 100,
                repo_facts: 50,
                user_items: 10,
                assistant_history: 400,
                reasoning_replay: 200,
                tool_results: 400,
                tool_specs: 40,
                total: 1_200,
            },
        };
        let body = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(body["turns"], 3);
        assert_eq!(body["cached_input_tokens"], 500);
        assert_eq!(body["wall_time_millis"], 2_500);
        assert_eq!(body["last_context_census"]["total"], 1_200);
    }

    #[test]
    fn no_events_leaves_nothing_and_returns_none() {
        let base = temp_dir("empty");
        let session = session_with_items();
        let writer = TraceWriter::new(base.clone());
        let dir = writer
            .finish(
                &session,
                TraceMeta {
                    provider: "deepseek",
                    model: "deepseek-flash",
                    reasoning_effort: ReasoningEffort::High,
                    max_turns: 40,
                    persisted: false,
                },
                Outcome::Ended {
                    status: "failed",
                    end_reason: "model_error".into(),
                },
                None,
            )
            .unwrap();
        assert!(dir.is_none());
        assert!(!base.exists());
    }
}
