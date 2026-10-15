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
    /// The kernel-owned todo list (roadmap item 8 phase B), carried as
    /// per-session state so the `todo` tool's mutations and the loop's
    /// tail re-append rule read and write one value. `#[serde(default)]`
    /// keeps pre-todo session files loading. Item-keyed stores rebuild
    /// sessions from the transcript alone, so a reloaded session starts
    /// with an empty list while the transcript's appended
    /// [`TodoList::render`] blocks keep the model's view intact; the
    /// event stream's `TodoUpdated` events reconstruct the list for
    /// replay consumers.
    #[serde(default)]
    pub todos: TodoList,
}

impl Session {
    /// Creates an empty session with the given id.
    pub fn new(id: crate::id::SessionId) -> Self {
        Self {
            id,
            transcript: Vec::new(),
            tasks: Vec::new(),
            todos: TodoList::default(),
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
    /// of §6.1's "commit = turn" guarantee.
    ///
    /// Returns `[assistant_start, results_start]`. When the turn has no
    /// tool calls (EndTurn), `results_start` equals the index one past the
    /// assistant item — i.e. [`Session::next_index`] at commit time, a
    /// slot no item of this turn occupies. Persistence should slice
    /// `[assistant_start, next_index())`, not read through
    /// `results_start`.
    ///
    /// This is plan-then-apply in one call. The agent loop instead calls
    /// [`Session::plan_turn`], persists the [`PlannedTurn`], and then
    /// [`Session::apply_planned_turn`] — persist-then-commit (ADR-012).
    pub fn append_turn(
        &mut self,
        assistant: AssistantMessage,
        results: Vec<ToolResult>,
    ) -> Result<[ItemIndex; 2], SessionError> {
        let planned = self.plan_turn(assistant, results)?;
        Ok(self.apply_planned_turn(planned))
    }

    /// Validates one turn and computes the indices it will occupy, without
    /// mutating the transcript.
    ///
    /// Split from [`Session::append_turn`] so a caller can write the turn to
    /// durable storage **before** committing it in memory (ADR-012,
    /// persist-then-commit): the store persists the returned [`PlannedTurn`],
    /// then [`Session::apply_planned_turn`] commits it. Validation is
    /// identical to the old `append_turn`, so a planned turn always applies.
    pub fn plan_turn(
        &self,
        assistant: AssistantMessage,
        results: Vec<ToolResult>,
    ) -> Result<PlannedTurn, SessionError> {
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
        let results_at = ItemIndex::from(assistant_at.as_u32() + 1);
        Ok(PlannedTurn {
            assistant_at,
            results_at,
            assistant,
            results,
        })
    }

    /// Applies a turn produced by [`Session::plan_turn`].
    ///
    /// Infallible: planning validated the call↔result pairing and fixed the
    /// indices. Returns `[assistant_start, results_start]` like
    /// [`Session::append_turn`].
    ///
    /// # Panics
    ///
    /// In debug builds only, if `planned` was not produced against the
    /// current transcript tip (its `assistant_at` is not
    /// [`Session::next_index`]); a `PlannedTurn` from this session's
    /// immediately preceding [`Session::plan_turn`] always holds.
    pub fn apply_planned_turn(&mut self, planned: PlannedTurn) -> [ItemIndex; 2] {
        let PlannedTurn {
            assistant_at,
            results_at,
            assistant,
            results,
        } = planned;
        debug_assert_eq!(
            assistant_at,
            self.next_index(),
            "planned turn applied against a moved transcript tip"
        );
        self.transcript.push(Item::Assistant(assistant));
        for result in results {
            self.transcript.push(Item::ToolResult(result));
        }
        [assistant_at, results_at]
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

/// A validated turn, planned before it is written anywhere.
///
/// [`Session::plan_turn`] returns one, fixing the indices it will occupy;
/// a store persists it and [`Session::apply_planned_turn`] commits it to the
/// in-memory transcript. Because the indices are frozen at plan time, storing
/// the turn and applying it cannot disagree (persist-then-commit, ADR-012).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTurn {
    assistant_at: ItemIndex,
    results_at: ItemIndex,
    assistant: AssistantMessage,
    results: Vec<ToolResult>,
}

impl PlannedTurn {
    /// Index the assistant item occupies.
    pub fn assistant_at(&self) -> ItemIndex {
        self.assistant_at
    }

    /// Index the first tool result occupies. For a turn with no tool calls
    /// (EndTurn) no item occupies it; it equals [`PlannedTurn::end_index`].
    pub fn results_at(&self) -> ItemIndex {
        self.results_at
    }

    /// Index one past the last item of this turn.
    pub fn end_index(&self) -> ItemIndex {
        ItemIndex::from(self.results_at.as_u32() + self.results.len() as u32)
    }

    /// The assistant message.
    pub fn assistant(&self) -> &AssistantMessage {
        &self.assistant
    }

    /// The tool results, in original call order.
    pub fn results(&self) -> &[ToolResult] {
        &self.results
    }

    /// The `(index, item)` pairs in append order: the assistant item first,
    /// then one item per tool result.
    pub fn items(&self) -> impl Iterator<Item = (ItemIndex, Item)> + '_ {
        let assistant =
            std::iter::once((self.assistant_at, Item::Assistant(self.assistant.clone())));
        let results = self.results.iter().enumerate().map(|(offset, result)| {
            let index = ItemIndex::from(self.results_at.as_u32() + offset as u32);
            (index, Item::ToolResult(result.clone()))
        });
        assistant.chain(results)
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
    /// Terminal: the operator paused between turns. The in-flight turn
    /// committed fully; the session resumes from here.
    Paused,
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
    /// Terminal: the operator paused between turns; the in-flight turn
    /// committed fully and the session is resumable.
    Paused,
}

impl From<TerminalTaskStatus> for TaskStatus {
    fn from(value: TerminalTaskStatus) -> Self {
        match value {
            TerminalTaskStatus::Completed => TaskStatus::Completed,
            TerminalTaskStatus::Failed => TaskStatus::Failed,
            TerminalTaskStatus::Cancelled => TaskStatus::Cancelled,
            TerminalTaskStatus::Paused => TaskStatus::Paused,
        }
    }
}

/// The kernel-owned todo list's stable header (roadmap item 8 phase B).
///
/// Every projected block starts with this line, so consumers (and the
/// agent loop's handoff digest) can recognize a todo block on sight.
pub const TODO_BLOCK_HEADER: &str = "## todo (kernel)";

/// Lifecycle of one todo item.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Done,
    /// Abandoned; kept for the record.
    Cancelled,
}

impl TodoStatus {
    /// The checkbox marker this status renders as in a
    /// [`TodoList::render`] line.
    #[must_use]
    pub fn marker(self) -> char {
        match self {
            Self::Pending => ' ',
            Self::InProgress => '~',
            Self::Done => 'x',
            Self::Cancelled => '-',
        }
    }

    /// Parses the wire form the `todo` tool accepts (`"pending"`,
    /// `"in_progress"`, `"done"`, `"cancelled"`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// One todo item: a stable id, its text, and its status.
///
/// Ids are decimal strings assigned from a per-session counter that only
/// grows, so an id stays stable for the life of the session even across
/// [`TodoList::replace`] — the model can mark `"2"` without wondering
/// which list it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Stable, session-unique decimal id (`"1"`, `"2"`, …).
    pub id: String,
    /// The item's text, exactly as added.
    pub text: String,
    /// Lifecycle state.
    pub status: TodoStatus,
}

/// The session's todo list (roadmap item 8 phase B): kernel-owned plan
/// state the `todo` tool mutates and the agent loop projects.
///
/// `revision` bumps on every successful mutation, which is the loop's
/// only signal for the tail re-append rule: when a turn's tool batch
/// changed the list, the rendered block is appended to the transcript
/// **tail** — so the current plan is re-surfaced at the point of change
/// while the transcript stays append-only and the projected prefix
/// stays byte-identical for provider caching. Unchanged turns append
/// nothing; the freshest block simply rides in history until the next
/// change replaces it at the tail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    /// Items in list order.
    #[serde(default)]
    pub items: Vec<TodoItem>,
    /// Monotonic id counter; the next item takes `next_id + 1`.
    #[serde(default)]
    pub next_id: u32,
    /// Bumped on every successful mutation; never reset.
    #[serde(default)]
    pub revision: u64,
}

impl TodoList {
    /// Whether the list holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Appends a pending item and returns its stable id.
    pub fn add(&mut self, text: impl Into<String>) -> String {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id.to_string();
        self.items.push(TodoItem {
            id: id.clone(),
            text: text.into(),
            status: TodoStatus::Pending,
        });
        self.revision = self.revision.saturating_add(1);
        id
    }

    /// Replaces the whole list with fresh pending items (the `create`
    /// op). Ids keep counting up, so ids from the replaced list are
    /// never reused.
    pub fn replace(&mut self, texts: Vec<String>) {
        self.items = texts
            .into_iter()
            .map(|text| {
                self.next_id = self.next_id.saturating_add(1);
                TodoItem {
                    id: self.next_id.to_string(),
                    text,
                    status: TodoStatus::Pending,
                }
            })
            .collect();
        self.revision = self.revision.saturating_add(1);
    }

    /// Sets one item's status by id. Returns `false` — without mutating
    /// — when the id is unknown.
    pub fn mark(&mut self, id: &str, status: TodoStatus) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        item.status = status;
        self.revision = self.revision.saturating_add(1);
        true
    }

    /// Renders the compact, byte-stable markdown block the model sees:
    /// the stable [`TODO_BLOCK_HEADER`] line, then one
    /// `- [<marker>] <id>: <text>` line per item in list order. An empty
    /// list renders as the empty string — there is nothing to project.
    #[must_use]
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let mut lines = vec![TODO_BLOCK_HEADER.to_owned()];
        lines.extend(
            self.items
                .iter()
                .map(|item| format!("- [{}] {}: {}", item.status.marker(), item.id, item.text)),
        );
        lines.join("\n")
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
        ToolResult::new(
            ToolCallId::try_from(call_id.to_string()).unwrap(),
            ToolStatus::Success,
            "ok",
            false,
        )
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
                thinking_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
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

    #[test]
    fn todo_ids_are_stable_and_sequential_across_replaces() {
        let mut todos = TodoList::default();
        assert_eq!(todos.add("first"), "1");
        assert_eq!(todos.add("second"), "2");
        todos.replace(vec!["fresh".to_owned()]);
        // Ids keep counting: the replaced list's ids are never reused.
        assert_eq!(
            todos
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["3"]
        );
        assert_eq!(todos.add("another"), "4");
    }

    #[test]
    fn todo_mark_sets_status_or_reports_unknown_id() {
        let mut todos = TodoList::default();
        todos.add("write the test");
        assert!(todos.mark("1", TodoStatus::InProgress));
        assert_eq!(todos.items[0].status, TodoStatus::InProgress);
        let revision = todos.revision;
        assert!(!todos.mark("9", TodoStatus::Done), "unknown id");
        assert_eq!(
            todos.revision, revision,
            "a failed mark must not bump the revision"
        );
        assert_eq!(todos.items[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn todo_render_is_compact_markdown_with_a_stable_header() {
        let mut todos = TodoList::default();
        assert_eq!(todos.render(), "", "an empty list projects nothing");
        todos.add("write the test");
        todos.add("fix the bug");
        todos.mark("1", TodoStatus::Done);
        todos.mark("2", TodoStatus::InProgress);
        assert_eq!(
            todos.render(),
            "## todo (kernel)\n- [x] 1: write the test\n- [~] 2: fix the bug"
        );
    }

    #[test]
    fn todo_status_parses_its_wire_form_and_rejects_the_rest() {
        for (wire, status) in [
            ("pending", TodoStatus::Pending),
            ("in_progress", TodoStatus::InProgress),
            ("done", TodoStatus::Done),
            ("cancelled", TodoStatus::Cancelled),
        ] {
            assert_eq!(TodoStatus::parse(wire), Some(status));
        }
        assert_eq!(TodoStatus::parse("Complete"), None);
    }

    #[test]
    fn session_todo_state_round_trips_and_defaults_for_old_files() {
        // `#[serde(default)]`: a session serialized before todo state
        // existed still deserializes, with an empty list.
        let mut session = Session::new(crate::id::SessionId::new());
        session.todos.add("plan");
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.todos, session.todos);

        // The pre-todo wire shape: the same object without a `todos` key.
        let mut value = serde_json::to_value(&session).unwrap();
        value.as_object_mut().unwrap().remove("todos");
        let old: Session = serde_json::from_value(value).unwrap();
        assert!(old.todos.is_empty());
    }
}
