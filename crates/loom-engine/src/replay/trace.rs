//! Parsing a recorded trace directory back into the inputs a replay
//! needs (see the `loom replay-context` design doc).

use std::path::Path;

use loom_kernel::context::PruneReport;
use loom_kernel::session::{Session, TaskRecord};
use loom_kernel::transcript::Item;

use crate::trace::{EVENTS_FILE_NAME, MANIFEST_FILE_NAME, TRANSCRIPT_FILE_NAME};

use super::options::ReplayError;

/// A trace directory's parsed contents (see [`TraceInput::read`]).
#[derive(Debug, Clone)]
pub struct TraceInput {
    /// The recorded session, rebuilt from `transcript.json`.
    pub session: Session,
    /// The manifest's task record when present, else the session's last.
    pub task: TaskRecord,
    /// Manifest harness provider (estimator family).
    pub provider: String,
    /// Manifest harness model.
    pub model: String,
    /// The trace's `event_schema_version`, for skew detection: a census
    /// recorded by an older schema came from an older compiler
    /// generation and is not comparable to this binary's projection.
    pub schema_version: u16,
    /// Manifest summary ground truth, when the manifest carries one.
    pub summary: Option<serde_json::Value>,
    /// `context_pruned` events from `events.ndjson` (turn, report), when
    /// the events file exists and parses.
    pub prune_events: Vec<(u32, PruneReport)>,
}

impl TraceInput {
    /// Loads and parses a task directory written by `loom run --trace`:
    /// `manifest.json` + `transcript.json` (events optional).
    pub fn read(dir: &Path) -> Result<Self, ReplayError> {
        let manifest_text =
            std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).map_err(|error| {
                ReplayError::InvalidTrace(format!(
                    "no {MANIFEST_FILE_NAME} in {}: {error}",
                    dir.display()
                ))
            })?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
            .map_err(|error| ReplayError::InvalidTrace(format!("manifest: {error}")))?;
        let provider = manifest["harness"]["provider"]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidTrace("manifest.harness.provider missing".into()))?
            .to_owned();
        let model = manifest["harness"]["model"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        let transcript_text =
            std::fs::read_to_string(dir.join(TRANSCRIPT_FILE_NAME)).map_err(|error| {
                ReplayError::InvalidTrace(format!(
                    "no {TRANSCRIPT_FILE_NAME} in {}: {error}",
                    dir.display()
                ))
            })?;
        let transcript: serde_json::Value = serde_json::from_str(&transcript_text)
            .map_err(|error| ReplayError::InvalidTrace(format!("transcript: {error}")))?;
        let session_id = transcript["session_id"]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidTrace("transcript.session_id missing".into()))?;
        let session_id: loom_kernel::id::SessionId = session_id.parse().map_err(|error| {
            ReplayError::InvalidTrace(format!("transcript session_id: {error}"))
        })?;
        let mut session = Session::new(session_id);
        for wrapper in transcript["items"].as_array().unwrap_or(&Vec::new()) {
            let item: Item = serde_json::from_value(wrapper["item"].clone())
                .map_err(|error| ReplayError::InvalidTrace(format!("transcript item: {error}")))?;
            session.transcript.push(item);
        }
        for task in transcript["tasks"].as_array().unwrap_or(&Vec::new()) {
            let task: TaskRecord = serde_json::from_value(task.clone())
                .map_err(|error| ReplayError::InvalidTrace(format!("transcript task: {error}")))?;
            session.tasks.push(task);
        }
        let task = manifest["task_id"]
            .as_str()
            .and_then(|id| session.tasks.iter().find(|task| task.id.to_string() == id))
            .or_else(|| session.tasks.last())
            .cloned()
            .ok_or_else(|| ReplayError::InvalidTrace("no task record in transcript".into()))?;

        let mut prune_events = Vec::new();
        if let Ok(events) = std::fs::read_to_string(dir.join(EVENTS_FILE_NAME)) {
            for line in events.lines() {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if event["type"] == "context_pruned"
                    && let Ok(report) =
                        serde_json::from_value::<PruneReport>(event["report"].clone())
                {
                    let turn = event["turn"].as_u64().unwrap_or_default() as u32;
                    prune_events.push((turn, report));
                }
            }
        }

        Ok(Self {
            session,
            task,
            provider,
            model,
            schema_version: manifest["event_schema_version"]
                .as_u64()
                .unwrap_or_default() as u16,
            summary: manifest["summary"]
                .as_object()
                .cloned()
                .map(serde_json::Value::Object),
            prune_events,
        })
    }
}
