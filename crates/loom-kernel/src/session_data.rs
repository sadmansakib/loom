//! Task lifecycle and per-session todo data types.

use crate::id::TaskId;
use crate::model::Usage;
use serde::{Deserialize, Serialize};

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
    /// Terminal: the model ended its turn (`EndTurn`).
    Completed,
    /// Terminal: model error, context error, max turns, or a fatal stop
    /// reason (`MaxTokens` / `ContextLimit` / `ContentFilter`).
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
    /// Reasoning-effort level the engine armed for this task, as the
    /// request wire string (`"low"`, `"high"`, …); `None` when the
    /// engine sends no effort hint or the record predates the field.
    /// Recorded per task because a session can mix efforts across
    /// follow-up tasks; not part of the event wire format, so old
    /// persisted rows deserialize with `None`.
    #[serde(default)]
    pub effort: Option<String>,
    /// Short machine-readable token naming the cause of the ending, in
    /// `snake_case` (e.g. `"end_turn"`, `"max_turns"`, `"model_error"`,
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
    /// Terminal: the model ended its turn (`EndTurn`).
    Completed,
    /// Terminal: model error, context error, max turns, or a fatal stop
    /// reason (`MaxTokens` / `ContextLimit` / `ContentFilter`).
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
    #[must_use]
    pub fn new(id: TaskId, started_at_millis: u64) -> Self {
        Self {
            id,
            status: TaskStatus::Running,
            turns: 0,
            usage: Usage::default(),
            started_at_millis,
            completed_at_millis: None,
            effort: None,
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
