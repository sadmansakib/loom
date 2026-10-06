//! Session: transcript plus task records.
//!
//! A [`Session`] is the durable unit of conversational state (ADR-004). Its
//! transcript — an append-only `Vec` of [`Item`]s — is the **source of
//! truth**: rebuilding a session means loading its items in index order,
//! never replaying the event stream. Task records carry per-run metadata
//! only; the task's user input lives in the transcript, not duplicated in
//! the record.
//!
//! ## Turn atomicity
//!
//! [`Session::append_turn`] is the only way a tool-call turn should enter
//! the transcript. It appends the assistant item and one result per tool
//! call **as one unit**, validating that the results answer the calls in
//! original order *before* appending anything — so a failed validation
//! leaves the transcript untouched. Persistence wraps the same boundary in
//! one SQLite transaction (§6.1).

use crate::id::TaskId;
use crate::model::Usage;
use crate::transcript::{AssistantMessage, Item, ItemIndex, ToolResult};
use serde::{Deserialize, Serialize};

/// Errors from mutating a [`Session`] transcript.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A turn's tool results did not answer its tool calls one-to-one, in
    /// original call order. No items were appended.
    #[error("tool results do not match tool calls in order (first problem at call `{call_id}`)")]
    ToolResultsMismatch {
        /// The call id where the mismatch was detected.
        call_id: String,
    },
    /// An assistant item carrying tool calls was pushed outside
    /// [`Session::append_turn`], which would commit tool calls without
    /// their results. Nothing was appended.
    #[error("assistant items with tool calls must be committed via append_turn")]
    AssistantToolCallsNeedAppendTurn,
}

/// One conversation: its durable transcript plus metadata for every task
/// run inside it.
///
/// Items are addressed by [`ItemIndex`]; because the transcript only
/// grows, an index handed out by [`Session::push_item`] or
/// [`Session::append_turn`] stays valid for the life of the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Identity of this session (used as the persistence key).
    pub id: crate::id::SessionId,
    /// The append-only conversation record; source of truth for context.
    pub transcript: Vec<Item>,
    /// Metadata for each task run, in start order.
    pub tasks: Vec<TaskRecord>,
}

impl Session {
    /// Creates an empty session with the given id.
    pub fn new(id: crate::id::SessionId) -> Self {
        Self {
            id,
            transcript: Vec::new(),
            tasks: Vec::new(),
        }
    }

    /// The index the next appended item will receive.
    pub fn next_index(&self) -> ItemIndex {
        // A u32 would overflow after 4 billion items; not a real risk for
        // an append-only conversation held in memory.
        ItemIndex::from(self.transcript.len() as u32)
    }

    /// Appends a single item and returns its stable transcript index.
    ///
    /// For turns that contain tool calls use [`Session::append_turn`]:
    /// it is the atomicity boundary. This method rejects an assistant
    /// item carrying tool calls in **every** build — committing one
    /// without its results would break the turn invariant.
    pub fn push_item(&mut self, item: Item) -> Result<ItemIndex, SessionError> {
        if let Item::Assistant(assistant) = &item
            && !assistant.tool_calls.is_empty()
        {
            return Err(SessionError::AssistantToolCallsNeedAppendTurn);
        }
        let index = self.next_index();
        self.transcript.push(item);
        Ok(index)
    }

    /// Atomically commits one agent turn: the assistant message followed
    /// by exactly one result per tool call, in original call order.
    ///
    /// Validation happens fully before the first append, so on
    /// [`SessionError`] the transcript is unchanged — the in-memory mirror
    /// of §6.1's "commit = turn" guarantee. Callers persist the new items
    /// in one transaction right after this returns.
    ///
    /// Returns `[assistant_start, results_start]`. When the turn has no
    /// tool calls (EndTurn), `results_start` equals the index one past the
    /// assistant item — i.e. [`Session::next_index`] at commit time, a
    /// slot no item of this turn occupies. Persistence should slice
    /// `[assistant_start, next_index())`, not read through
    /// `results_start`.
    pub fn append_turn(
        &mut self,
        assistant: AssistantMessage,
        results: Vec<ToolResult>,
    ) -> Result<[ItemIndex; 2], SessionError> {
        // Pairwise check first: results must answer the calls 1:1, in order.
        for (call, result) in assistant.tool_calls.iter().zip(results.iter()) {
            if call.id != result.call_id {
                return Err(SessionError::ToolResultsMismatch {
                    call_id: call.id.to_string(),
                });
            }
        }
        if assistant.tool_calls.len() != results.len() {
            // zip stopped at the shorter side without mismatching; the
            // first item past the shorter side is the offender. Missing
            // results: the first unanswered call. Surplus results: the
            // first result with no call. (Indexing is safe: lengths
            // differ, so exactly one side extends past the other.)
            let call_id = match assistant.tool_calls.get(results.len()) {
                Some(call) => call.id.to_string(),
                None => results[assistant.tool_calls.len()].call_id.to_string(),
            };
            return Err(SessionError::ToolResultsMismatch { call_id });
        }

        let assistant_at = self.next_index();
        self.transcript.push(Item::Assistant(assistant));
        let results_at = self.next_index();
        for result in results {
            self.transcript.push(Item::ToolResult(result));
        }
        Ok([assistant_at, results_at])
    }

    /// Records a task run in this session. The task's input is *not*
    /// stored here — it is already an `Item::User` in the transcript.
    pub fn push_task(&mut self, record: TaskRecord) {
        self.tasks.push(record);
    }

    /// Mutable access to a task record by id, if present.
    pub fn task_mut(&mut self, id: TaskId) -> Option<&mut TaskRecord> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }
}

/// Lifecycle of one task run (a single execution of the agent loop).
///
/// `Running` is the only non-terminal state; the other variants are
/// terminal and record how the run ended (§5). Note that "no tool calls"
/// is not what decides completion — the model's `StopReason` does.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The loop is still executing.
    Running,
    /// Terminal: the model ended its turn (EndTurn).
    Completed,
    /// Terminal: model error, context error, max turns, or a fatal stop
    /// reason (MaxTokens / ContextLimit / ContentFilter).
    Failed,
    /// Terminal: the user (or a shutdown) cancelled the run.
    Cancelled,
}

/// Metadata for one task run inside a session.
///
/// Deliberately does **not** duplicate the task input: the transcript is
/// the source of truth (§4.1). Accumulated fields (`turns`, `usage`) are
/// updated by the agent loop as the task progresses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Identity of this task run.
    pub id: TaskId,
    /// Lifecycle state; `Running` until the loop terminates.
    pub status: TaskStatus,
    /// Model turns consumed so far.
    pub turns: u32,
    /// Token usage accumulated across all requests of this task.
    pub usage: Usage,
    /// When the task started: Unix epoch milliseconds, UTC.
    pub started_at_millis: u64,
    /// When the task reached a terminal state, if it has.
    pub completed_at_millis: Option<u64>,
    /// Short machine-readable token naming the cause of the ending, in
    /// snake_case (e.g. `"end_turn"`, `"max_turns"`, `"model_error"`,
    /// `"user_cancelled"`, `"shutdown"`). Convention: **always set on
    /// terminal records** — even when the status alone implies the cause
    /// (`"end_turn"` on [`TaskStatus::Completed`]) — so stored values stay
    /// uniform and greppable. `None` only while running.
    pub end_reason: Option<String>,
}

/// A [`TaskStatus`] restricted to its terminal states: the parameter type
/// of [`TaskRecord::finish`], so passing `Running` cannot compile and
/// "first ending wins" holds in every build.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalTaskStatus {
    /// Terminal: the model ended its turn (EndTurn).
    Completed,
    /// Terminal: model error, context error, max turns, or a fatal stop
    /// reason (MaxTokens / ContextLimit / ContentFilter).
    Failed,
    /// Terminal: the user (or a shutdown) cancelled the run.
    Cancelled,
}

impl From<TerminalTaskStatus> for TaskStatus {
    fn from(value: TerminalTaskStatus) -> Self {
        match value {
            TerminalTaskStatus::Completed => TaskStatus::Completed,
            TerminalTaskStatus::Failed => TaskStatus::Failed,
            TerminalTaskStatus::Cancelled => TaskStatus::Cancelled,
        }
    }
}

impl TaskRecord {
    /// Creates a fresh record in the [`TaskStatus::Running`] state.
    pub fn new(id: TaskId, started_at_millis: u64) -> Self {
        Self {
            id,
            status: TaskStatus::Running,
            turns: 0,
            usage: Usage::default(),
            started_at_millis,
            completed_at_millis: None,
            end_reason: None,
        }
    }

    /// Moves the record to a terminal state, recording the ending's cause
    /// token. No-op on a record that is already terminal — the first
    /// ending wins, matching the loop's single terminal transition. The
    /// status parameter is [`TerminalTaskStatus`], so a non-terminal
    /// target is a type error rather than a runtime guard.
    pub fn finish(&mut self, status: TerminalTaskStatus, at_millis: u64, end_reason: String) {
        if self.status == TaskStatus::Running {
            self.status = status.into();
            self.completed_at_millis = Some(at_millis);
            self.end_reason = Some(end_reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StopReason;
    use crate::transcript::{ToolArguments, ToolCall, ToolCallId, ToolStatus};

    fn assistant_with_calls(call_ids: &[&str]) -> AssistantMessage {
        AssistantMessage {
            content: "working".into(),
            reasoning: None,
            tool_calls: call_ids
                .iter()
                .map(|id| ToolCall {
                    id: ToolCallId::try_from((*id).to_string()).unwrap(),
                    name: "read_file".into(),
                    arguments: ToolArguments::try_from(serde_json::json!({"path": "x"})).unwrap(),
                })
                .collect(),
            stop_reason: StopReason::ToolCalls,
        }
    }

    fn result_for(call_id: &str) -> ToolResult {
        ToolResult {
            call_id: ToolCallId::try_from(call_id.to_string()).unwrap(),
            status: ToolStatus::Success,
            content: "ok".into(),
            truncated: false,
        }
    }

    #[test]
    fn push_item_assigns_sequential_indices() {
        let mut session = Session::new(crate::id::SessionId::new());
        let a = session
            .push_item(Item::User {
                text: "hello".into(),
            })
            .unwrap();
        let b = session
            .push_item(Item::User {
                text: "again".into(),
            })
            .unwrap();
        assert_eq!(a.as_u32(), 0);
        assert_eq!(b.as_u32(), 1);
        assert_eq!(session.next_index().as_u32(), 2);
    }

    #[test]
    fn append_turn_writes_assistant_then_results_in_order() {
        let mut session = Session::new(crate::id::SessionId::new());
        let [assistant_at, results_at] = session
            .append_turn(
                assistant_with_calls(&["call_1", "call_2"]),
                vec![result_for("call_1"), result_for("call_2")],
            )
            .unwrap();

        assert_eq!(assistant_at.as_u32(), 0);
        assert_eq!(results_at.as_u32(), 1);
        // Assistant first, then one result per call, call order preserved.
        assert!(matches!(session.transcript[0], Item::Assistant(_)));
        assert!(matches!(session.transcript[1], Item::ToolResult(_)));
        assert!(matches!(session.transcript[2], Item::ToolResult(_)));
        let Item::ToolResult(second) = &session.transcript[2] else {
            panic!("expected tool result");
        };
        assert_eq!(second.call_id.as_str(), "call_2");
    }

    #[test]
    fn append_turn_rejects_wrong_order_and_keeps_transcript_untouched() {
        let mut session = Session::new(crate::id::SessionId::new());
        session.push_item(Item::User { text: "hi".into() }).unwrap();
        let before = session.transcript.len();

        let err = session
            .append_turn(
                assistant_with_calls(&["call_1", "call_2"]),
                // Right ids, wrong order — must fail before any append.
                vec![result_for("call_2"), result_for("call_1")],
            )
            .unwrap_err();
        assert!(matches!(err, SessionError::ToolResultsMismatch { .. }));
        assert_eq!(session.transcript.len(), before, "atomic: nothing appended");
    }

    #[test]
    fn append_turn_rejects_missing_and_surplus_results() {
        let mut session = Session::new(crate::id::SessionId::new());

        // Missing results, none given: the first unanswered call (c1).
        let err = session
            .append_turn(assistant_with_calls(&["call_1"]), vec![])
            .unwrap_err();
        assert!(
            matches!(&err, SessionError::ToolResultsMismatch { call_id } if call_id == "call_1"),
            "got: {err:?}"
        );

        // Missing results, some given: the *unanswered* call (c2), not
        // the correctly answered c1.
        let err = session
            .append_turn(
                assistant_with_calls(&["call_1", "call_2"]),
                vec![result_for("call_1")],
            )
            .unwrap_err();
        assert!(
            matches!(&err, SessionError::ToolResultsMismatch { call_id } if call_id == "call_2"),
            "got: {err:?}"
        );

        // Surplus result: the extra result's id (c9), not the answered c1.
        let err = session
            .append_turn(
                assistant_with_calls(&["call_1"]),
                vec![result_for("call_1"), result_for("call_9")],
            )
            .unwrap_err();
        assert!(
            matches!(&err, SessionError::ToolResultsMismatch { call_id } if call_id == "call_9"),
            "got: {err:?}"
        );

        // A no-tool-call turn (EndTurn) commits fine with no results.
        session
            .append_turn(assistant_with_calls(&[]), vec![])
            .unwrap();
        assert_eq!(session.transcript.len(), 1);
    }

    #[test]
    fn push_item_rejects_assistant_with_tool_calls_in_every_build() {
        let mut session = Session::new(crate::id::SessionId::new());
        let err = session
            .push_item(Item::Assistant(assistant_with_calls(&["call_1"])))
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::AssistantToolCallsNeedAppendTurn
        ));
        assert!(session.transcript.is_empty(), "nothing appended");
    }

    #[test]
    fn task_record_lifecycle() {
        let mut record = TaskRecord::new(TaskId::new(), 1_000);
        assert_eq!(record.status, TaskStatus::Running);
        assert_eq!(record.usage, Usage::default());

        record.turns += 1;
        record.usage = record.usage
            + Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: Some(2),
            };
        record.finish(TerminalTaskStatus::Completed, 2_000, "end_turn".into());

        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(record.completed_at_millis, Some(2_000));
        assert_eq!(record.end_reason.as_deref(), Some("end_turn"));

        // Terminal state is final: a second finish is ignored.
        record.finish(TerminalTaskStatus::Failed, 3_000, "oops".into());
        assert_eq!(record.status, TaskStatus::Completed);
    }

    #[test]
    fn session_round_trips_through_serde() {
        let mut session = Session::new(crate::id::SessionId::new());
        session
            .push_item(Item::User {
                text: "build it".into(),
            })
            .unwrap();
        session
            .append_turn(
                assistant_with_calls(&["call_1"]),
                vec![result_for("call_1")],
            )
            .unwrap();
        let mut task = TaskRecord::new(TaskId::new(), 42);
        task.finish(TerminalTaskStatus::Cancelled, 99, "user_cancelled".into());
        session.push_task(task);

        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(session, back);
    }

    #[test]
    fn task_lookup_by_id() {
        let mut session = Session::new(crate::id::SessionId::new());
        let id = TaskId::new();
        session.push_task(TaskRecord::new(id, 0));
        assert!(session.task_mut(id).is_some());
        assert!(session.task_mut(TaskId::new()).is_none());
    }
}
