//! Per-turn compile and commit plumbing: the context budget, fact
//! scanning, request compilation (§8), and the persist-then-commit
//! writers for items, task records, and todo blocks (§6.1, ADR-012).

use super::Agent;
use super::error::persistence;
use super::{TaskError, derive_reserved_output_tokens};
use crate::context::{CompiledContext, ContextBudget, ContextInput, PruneCheckpoint};
use crate::id::TaskId;
use crate::model::ModelCapabilities;
use crate::session::{PlannedTurn, Session, TaskRecord};
use crate::tool::ToolRegistry;
use crate::transcript::{AssistantMessage, Item, ToolResult};
use std::sync::Arc;

impl Agent {
    /// The task's input budget: the window less the reserved output
    /// (§6.4), derived once per task.
    pub(super) fn task_budget(
        &self,
        capabilities: &ModelCapabilities,
    ) -> Result<ContextBudget, TaskError> {
        let reserved_output_tokens =
            derive_reserved_output_tokens(capabilities, self.output_reservation);
        Ok(ContextBudget::from_window(
            capabilities.max_context_tokens,
            reserved_output_tokens,
        )?)
    }

    /// Scans repository facts on the blocking pool (2E, ADR-015): the
    /// scan walks the tree and shells out to git — blocking work, never
    /// on this async worker.
    pub(super) async fn scan_facts(&self) -> Result<crate::context::KnownFacts, TaskError> {
        let source = Arc::clone(&self.facts);
        tokio::task::spawn_blocking(move || source.scan())
            .await
            .map_err(|_| TaskError::Internal("facts scan panicked".to_owned()))
    }

    /// Compiles one turn's model request from durable state (§8): the
    /// compiler owns every context decision; the loop only passes it
    /// the inputs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_turn(
        &self,
        session: &Session,
        task_id: TaskId,
        budget: ContextBudget,
        capabilities: &ModelCapabilities,
        facts: &crate::context::KnownFacts,
        prune_checkpoint: Option<&PruneCheckpoint>,
        turn_tools: &ToolRegistry,
    ) -> Result<CompiledContext, TaskError> {
        let task = session
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| TaskError::Protocol("task record missing".to_owned()))?;
        self.compiler
            .compile(ContextInput {
                session,
                tools: turn_tools,
                task,
                budget,
                capabilities,
                model: &self.model,
                config: &self.config,
                facts,
                prune_checkpoint,
            })
            .map_err(std::convert::Into::into)
    }

    /// The single atomic commit point of a turn (§6.1): the assistant item
    /// and one result per call, or nothing on a validation failure.
    ///
    /// With a store attached this is persist-then-commit: the planned turn is
    /// written first (one store transaction), then applied in memory. A store
    /// failure returns [`TaskError::Persistence`] and the transcript is
    /// untouched (ADR-012).
    pub(super) fn commit(
        &self,
        session: &mut Session,
        assistant: AssistantMessage,
        results: Vec<ToolResult>,
    ) -> Result<(), TaskError> {
        let planned: PlannedTurn = session
            .plan_turn(assistant, results)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        if let Some(store) = &self.store {
            store
                .append_turn(&session.id, &planned)
                .map_err(persistence)?;
        }
        session.apply_planned_turn(planned);
        Ok(())
    }

    /// Mirrors a task record to the store, if one is attached.
    pub(super) fn persist_task(
        &self,
        session: &Session,
        record: &TaskRecord,
    ) -> Result<(), TaskError> {
        if let Some(store) = &self.store {
            store
                .upsert_task(&session.id, record)
                .map_err(persistence)?;
        }
        Ok(())
    }

    /// Re-appends the current todo list at the transcript tail (roadmap
    /// item 8 phase B): the loop's half of the tail projection rule.
    ///
    /// Called only when a turn's tool batch changed the list, so the
    /// freshest plan block lands at the tail exactly at the moments it
    /// changed — never duplicated on quiet turns. Because the transcript
    /// is append-only, the projected prefix stays byte-identical for
    /// provider caching, and every earlier block simply rides in
    /// history (where the pruning compiler's user elision may eventually
    /// reclaim it — the last user item, which a fresh block is, is
    /// protected). Persist-then-commit like every other item; an empty
    /// list projects nothing.
    pub(super) fn append_todo_block(&self, session: &mut Session) -> Result<(), TaskError> {
        if session.todos.is_empty() {
            return Ok(());
        }
        let block = Item::User {
            text: session.todos.render(),
        };
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &block)
                .map_err(persistence)?;
        }
        session
            .push_item(block)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        Ok(())
    }
}
