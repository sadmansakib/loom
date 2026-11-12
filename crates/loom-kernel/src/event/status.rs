//! The pollable progress picture: the phase enum, the snapshot, and the
//! recording sink that folds the event stream into it.

use super::sink::EventSink;
use super::wire::EventKind;
use crate::id::{SessionId, TaskId};
use crate::transcript::ToolCallId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
/// Coarse task phase for [`StatusSnapshot`], folded from terminal events.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// No task observed yet, or the engine went idle awaiting input.
    #[default]
    Idle,
    /// Between `TaskStarted` and a terminal event.
    Running,
    /// Terminal success (`TaskCompleted`).
    Completed,
    /// Terminal failure (`TaskFailed`).
    Failed,
    /// Cancelled mid-flight (`TaskCancelled`).
    Cancelled,
    /// Paused between turns (`TaskPaused`); the session is resumable.
    Paused,
}

/// A pollable progress picture of one task, folded from the event stream.
///
/// Derived state, never the source of truth (ADR-004 — the transcript
/// is): rebuild it at any time by replaying events. Maintained by
/// [`StatusRecorder`]; frontends poll
/// [`snapshot`](StatusRecorder::snapshot) on their own cadence (a TUI
/// status line, a GUI progress pane). The fields answer "where is this
/// task right now": which turn, which tools are executing, which
/// request attempt, and what phase the task ended in.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// The task this snapshot describes, from the latest envelope seen.
    pub task_id: Option<TaskId>,
    /// The idle session after [`EventKind::AwaitingInput`], once
    /// observed — the id a resume loop sends the next input to.
    pub session_id: Option<SessionId>,
    /// 1-based turn now running (the latest `TurnStarted`).
    pub turn: Option<u32>,
    /// Names of tool calls started but not yet completed, in start
    /// order — usually one; more when a batch runs concurrently.
    pub in_flight_tools: Vec<String>,
    /// The running model-request attempt (the latest
    /// `ModelRequestStarted`), cleared on `ModelCompleted`.
    pub attempt: Option<u32>,
    /// Coarse task phase.
    pub phase: TaskPhase,
}

/// An [`EventSink`] that folds the stream into a pollable
/// [`StatusSnapshot`] — the progress surface frontends poll while a
/// task runs (current turn, in-flight tools, running attempt, phase).
///
/// Clone it and hand one clone to the driving API — directly, or as a
/// [`FanoutSink`][crate::event::FanoutSink] child via
/// `with(recorder.clone())` — and keep the other for polling: clones
/// share one snapshot, so the retained handle observes everything
/// emitted. It is a pure in-memory tally: it cannot drop events, so it
/// is a transparent inner choice for
/// [`StrictSink`][crate::event::StrictSink].
///
/// One recorder tracks one task's stream. Frontends driving concurrent
/// tasks give each run its own recorder (its own fan-out branch);
/// envelopes from interleaved tasks would otherwise overwrite each
/// other's fields.
#[derive(Clone, Default)]
pub struct StatusRecorder {
    inner: Arc<StatusInner>,
}

/// Shared mutable fold state; the lock is held only across one `emit`
/// and never across an await.
#[derive(Default)]
struct StatusInner {
    state: std::sync::Mutex<RecorderState>,
}

/// What [`StatusRecorder`] actually tracks; projected into
/// [`StatusSnapshot`] on poll.
#[derive(Default)]
struct RecorderState {
    task_id: Option<TaskId>,
    session_id: Option<SessionId>,
    turn: Option<u32>,
    attempt: Option<u32>,
    phase: TaskPhase,
    /// Correlation id + name per started-but-not-completed tool call,
    /// so concurrent same-name calls stay distinct.
    in_flight: Vec<(ToolCallId, String)>,
}

impl StatusRecorder {
    /// Creates a recorder whose snapshot starts `Idle` with no fields
    /// set — the state before any event is observed.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current progress picture (a cheap clone of small state).
    pub fn snapshot(&self) -> StatusSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        StatusSnapshot {
            task_id: state.task_id,
            session_id: state.session_id,
            turn: state.turn,
            in_flight_tools: state
                .in_flight
                .iter()
                .map(|(_, name)| name.clone())
                .collect(),
            attempt: state.attempt,
            phase: state.phase,
        }
    }
}

impl EventSink for StatusRecorder {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.task_id = Some(task_id);
        match kind {
            EventKind::TaskStarted => state.phase = TaskPhase::Running,
            EventKind::TurnStarted { turn } => state.turn = Some(turn),
            EventKind::ModelRequestStarted { attempt } => state.attempt = Some(attempt),
            EventKind::ModelCompleted { .. } => state.attempt = None,
            EventKind::ToolStarted {
                name,
                call_id,
                arguments: _,
            } => {
                state.in_flight.push((call_id, name));
            }
            EventKind::ToolCompleted { call_id, .. } => {
                state.in_flight.retain(|(id, _)| *id != call_id);
            }
            EventKind::TaskCompleted => state.phase = TaskPhase::Completed,
            EventKind::TaskFailed { .. } => state.phase = TaskPhase::Failed,
            EventKind::TaskCancelled => state.phase = TaskPhase::Cancelled,
            EventKind::TaskPaused => state.phase = TaskPhase::Paused,
            EventKind::AwaitingInput { session_id } => {
                state.session_id = Some(session_id);
                state.phase = TaskPhase::Idle;
            }
            // Deltas and prune reports do not move the coarse picture.
            _ => {}
        }
    }
}
