//! The engine-event → ACP-update bridge: one event in, zero or more
//! `session/update` payloads out.

use agent_client_protocol::v1::schema as acp;
use loom_core::kernel::agent::{TaskControl, TaskError};
use loom_core::kernel::event::{EventKind, EventSink};
use loom_core::kernel::id::TaskId;
use loom_core::kernel::session::{TodoItem, TodoStatus};
use loom_core::kernel::transcript::ToolStatus;
use tokio::sync::mpsc;

/// The engine-event → ACP-update bridge. `EventSink::emit` is sync and
/// runs inside the task future; updates use a bounded channel so a slow
/// client cannot cause unbounded memory growth. If the client falls
/// behind, cancel the task rather than continuing without a complete
/// update stream.
pub(crate) struct SessionUpdateSink {
    pub(crate) tx: mpsc::Sender<acp::SessionUpdate>,
    pub(crate) control: TaskControl,
}

impl EventSink for SessionUpdateSink {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        for update in updates_for(&kind) {
            if self.tx.try_send(update).is_err() {
                // A closed receiver means the peer disconnected; a full
                // queue means it cannot keep up. In either case, stop
                // work whose protocol updates cannot be delivered.
                self.control.cancel();
                return;
            }
        }
    }
}

/// Maps a task error onto an ACP stop reason. `None` means the task
/// died of something that is not a client-facing stop condition
/// (model, protocol, or engine-internal failure) and should surface
/// as a JSON-RPC error instead.
#[allow(clippy::wildcard_enum_match_arm)] // TaskError is #[non_exhaustive]
pub(crate) fn stop_reason(error: &TaskError) -> Option<acp::StopReason> {
    match error {
        TaskError::Cancelled => Some(acp::StopReason::Cancelled),
        TaskError::MaxTurns { .. } => Some(acp::StopReason::MaxTurnRequests),
        TaskError::BudgetExhausted { .. } => Some(acp::StopReason::MaxTokens),
        // Pause is a loom-internal boundary no ACP client triggers;
        // the committed turns read as a normal end of turn.
        TaskError::Paused => Some(acp::StopReason::EndTurn),
        // Context/model/protocol/internal errors carry no ACP stop
        // reason; the task error itself is reported upstream.
        _ => None,
    }
}

/// Flattens the prompt's content blocks into the plain task input the
/// engine takes. Non-text blocks (images, resources) are skipped:
/// loom's model wire is text today, and dropping is honest — the task
/// proceeds on the text it was given.
pub(crate) fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let acp::ContentBlock::Text(text_block) = block {
            text.push_str(&text_block.text);
        }
    }
    text
}

/// Maps one engine event onto zero or more ACP session updates.
///
/// Skipped by design (internal harness telemetry, not conversation
/// content): turn lifecycle, fast-path and capability facts, context
/// pruning/handoff, model request/retry/completion accounting,
/// autofix bookkeeping, steering, budget extensions, loop nudges,
/// terminal task state. An editor sees message chunks, thoughts, tool
/// activity, and the plan — the things a human reads.
#[allow(clippy::wildcard_enum_match_arm)] // EventKind is #[non_exhaustive]; unknown kinds forward nothing
pub(crate) fn updates_for(kind: &EventKind) -> Vec<acp::SessionUpdate> {
    let text_block = |text: &str| acp::ContentBlock::Text(acp::TextContent::new(text.to_owned()));
    match kind {
        EventKind::TextDelta { text } => {
            vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(text_block(text)),
            )]
        }
        EventKind::ReasoningDelta { text } => {
            vec![acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(text_block(text)),
            )]
        }
        EventKind::ToolStarted {
            name,
            call_id,
            arguments,
        } => {
            let input = serde_json::from_str(arguments)
                .ok()
                .filter(serde_json::Value::is_object);
            vec![acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(call_id.to_string()), name.clone())
                    .name(name.clone())
                    .kind(tool_kind(name))
                    .status(acp::ToolCallStatus::InProgress)
                    .raw_input(input),
            )]
        }
        EventKind::ToolCompleted {
            name,
            call_id,
            status,
            content_head,
        } => {
            let mut fields = acp::ToolCallUpdateFields::new();
            fields.title = Some(name.clone());
            fields.status = Some(tool_status(*status));
            fields.content = Some(vec![acp::ToolCallContent::Content(acp::Content::new(
                text_block(content_head),
            ))]);
            vec![acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(acp::ToolCallId::new(call_id.to_string()), fields),
            )]
        }
        EventKind::TodoUpdated { items, .. } => vec![acp::SessionUpdate::Plan(acp::Plan::new(
            items.iter().map(plan_entry).collect(),
        ))],
        // Unknown future event kinds forward nothing to the client.
        _ => Vec::new(),
    }
}

/// Maps a tool name onto the ACP tool kind for editor display.
pub(crate) fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read_file" | "list_dir" | "artifact_read" => acp::ToolKind::Read,
        "search" => acp::ToolKind::Search,
        "write_file" | "edit_file" | "replace_all" | "replace_symbol" => acp::ToolKind::Edit,
        "run_command" | "verify" => acp::ToolKind::Execute,
        "todo" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Maps a tool outcome status; a cancelled call reads as failed — ACP
/// has no cancelled tool status, and the cancelled task's stop reason
/// already says it.
#[allow(clippy::wildcard_enum_match_arm)] // ToolStatus is #[non_exhaustive]; unknown statuses read as failed
fn tool_status(status: ToolStatus) -> acp::ToolCallStatus {
    match status {
        ToolStatus::Success => acp::ToolCallStatus::Completed,
        _ => acp::ToolCallStatus::Failed,
    }
}

/// Maps a kernel todo item onto an ACP plan entry. An abandoned item
/// (`Cancelled`) reads as completed: ACP plans have no abandoned
/// state, and leaving it pending would resurrect work the loop gave
/// up on.
#[allow(clippy::wildcard_enum_match_arm)] // TodoStatus is #[non_exhaustive]
fn plan_entry(item: &TodoItem) -> acp::PlanEntry {
    acp::PlanEntry::new(
        item.text.clone(),
        acp::PlanEntryPriority::Medium,
        match item.status {
            TodoStatus::Pending => acp::PlanEntryStatus::Pending,
            TodoStatus::InProgress => acp::PlanEntryStatus::InProgress,
            // Done, Cancelled, and any future state read as completed:
            // ACP plans have no abandoned state, and leaving work
            // pending would resurrect something the loop gave up on.
            _ => acp::PlanEntryStatus::Completed,
        },
    )
}
