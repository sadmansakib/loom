//! The task loop (§13): `run_task`, the zero-model-call fast path,
//! finalization, and the turn loop that compiles, streams, executes,
//! and commits each turn.

use super::Agent;
use super::budget::turn_extension_reason;
use super::continuation::{MAX_TOKENS_CONTINUATION_PROMPT, MAX_TOKENS_CONTINUATIONS};
use super::control::{LEASE_TTL_MILLIS, LeaseGuard, lease_holder_for};
use super::error::persistence;
use super::feedback::{
    CompletionGate, EditStalls, FailureLedger, late_budget_due, recent_changes, zero_progress_due,
};
use super::summary::{Accounting, stop_reason_token};
use super::{
    FAILED_CALL_TURNS_LIMIT, FAILED_CALL_TURNS_NUDGE, FastPathSolve,
    MALFORMED_ARGUMENT_TURNS_LIMIT, TaskControl, TaskError, TaskSummary,
};
use crate::artifact_gc::now_millis;
use crate::context::{ContextCensus, ContextError, PruneCheckpoint};
use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::model::StopReason;
use crate::session::{Session, TaskRecord, TerminalTaskStatus};
use crate::transcript::{AssistantMessage, Item, ToolStatus};
use std::sync::{Arc, Mutex};
use std::time::Instant;

impl Agent {
    /// Runs one task against `session`, appending its input and committing
    /// each turn as it completes.
    ///
    /// Returns a [`TaskSummary`] on terminal success (`EndTurn`). Every other
    /// terminal state — including cancellation and a graceful pause —
    /// returns a [`TaskError`]; the [`TaskRecord`] in `session` carries the
    /// same outcome.
    ///
    /// The loop never blocks on the event sink and never treats delivery
    /// failure as a task failure (§11): events are best-effort, the
    /// transcript is the durable record.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError`] when the session is leased by another
    /// writer, a store write fails, the model or a tool fails after
    /// retries, or the loop hits an internal invariant breach.
    /// Cancellation and a graceful pause also surface as
    /// [`TaskError`] variants rather than success.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        let started = Instant::now();

        // 0. The single-writer lease (ADR-008): everything this run
        //    persists happens under it, and it releases when the run
        //    ends — so a second process driving the same session fails
        //    fast below instead of interleaving transcript writes.
        //    Held through the epilogue by the guard's drop.
        let lease = self.acquire_session_lease(&session.id)?;

        // 1. The input becomes the transcript's next user item. Persisted to
        //    the store *before* the in-memory push (ADR-012,
        //    persist-then-commit). `push_item` only fails for an assistant
        //    item carrying tool calls, so the mapped branch is unreachable
        //    here; mapping it keeps the loop panic-free and the failure
        //    honest if it ever became reachable.
        let user = Item::User { text: input.into() };
        if let Some(store) = &self.store {
            store.ensure_session(&session.id).map_err(persistence)?;
            store
                .append_item(&session.id, session.next_index(), &user)
                .map_err(persistence)?;
        }
        session
            .push_item(user)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 2. The task record, persisted then mirrored before its start event.
        let task_id = TaskId::new();
        let record = TaskRecord::new(task_id, now_millis());
        self.persist_task(session, &record)?;
        session.push_task(record);
        sink.emit(task_id, EventKind::TaskStarted);

        // 3. The turn loop; the epilogue below always finalizes the record.
        let mut accounting = Accounting::default();
        let outcome = self
            .run_turns(
                session,
                task_id,
                sink,
                control,
                &mut accounting,
                started,
                lease.as_ref(),
            )
            .await;

        // 4. Finalize and summarize.
        self.finalize_task(session, task_id, &accounting, outcome, started, sink)
    }

    /// Records a deterministic solve that used zero model calls (roadmap
    /// item 3's fast paths).
    ///
    /// The engine analyzed the instruction, applied mechanical edits,
    /// and verified them green before any provider traffic existed; this
    /// method gives that solve the same transcript and bookkeeping
    /// discipline as a model-driven one — persist-then-commit items, a
    /// task record finished with `end_reason = "fast_path"`, the
    /// [`EventKind::FastPathHit`] marker, and a [`TaskSummary`] whose
    /// zeroed accounting is the proof of the zero-invocation claim. The
    /// solve's note becomes the task's single assistant item, so
    /// continuation sessions see what changed and why.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError`] when the session is leased by another
    /// writer, a store write fails, or transcript bookkeeping hits an
    /// internal invariant breach.
    // Public embedder API is awaited like `run_task`; kept `async` so
    // callers' `.await`s (and future awaits inside) stay signature-stable.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn record_fast_path(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        solve: FastPathSolve,
        sink: &dyn EventSink,
    ) -> Result<TaskSummary, TaskError> {
        let started = Instant::now();

        // 0. Same single-writer discipline as `run_task`: the whole
        //    recording happens under the session's lease.
        let _lease = self.acquire_session_lease(&session.id)?;

        // 1. The input becomes the transcript's next user item,
        //    persist-then-commit, exactly as `run_task` does.
        let user = Item::User { text: input.into() };
        if let Some(store) = &self.store {
            store.ensure_session(&session.id).map_err(persistence)?;
            store
                .append_item(&session.id, session.next_index(), &user)
                .map_err(persistence)?;
        }
        session
            .push_item(user)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 2. The task record and its start events; the marker names the
        //    deterministic classification for attribution.
        let task_id = TaskId::new();
        let record = TaskRecord::new(task_id, now_millis());
        self.persist_task(session, &record)?;
        session.push_task(record);
        sink.emit(task_id, EventKind::TaskStarted);
        sink.emit(
            task_id,
            EventKind::FastPathHit {
                kind: solve.kind,
                files_changed: solve.files_changed,
            },
        );

        // 3. The solve's proof text is the task's only assistant item —
        //    no tool calls ran through the loop, so nothing else joins
        //    the transcript.
        let assistant = Item::Assistant(AssistantMessage {
            content: solve.note,
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        });
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &assistant)
                .map_err(persistence)?;
        }
        session
            .push_item(assistant)
            .map_err(|error| TaskError::Internal(error.to_string()))?;

        // 4. Finalize: zeroed usage, `fast_path` end reason, terminal
        //    event, and the summary carrying the zero-invocation proof.
        if let Some(record) = session.task_mut(task_id) {
            record.turns = 0;
            record.finish(
                TerminalTaskStatus::Completed,
                now_millis(),
                "fast_path".to_owned(),
            );
        }
        if let Some(record) = session.tasks.iter().find(|task| task.id == task_id) {
            self.persist_task(session, record)?;
        }
        sink.emit(task_id, EventKind::TaskCompleted);
        let summary = TaskSummary {
            task_id,
            turns: 0,
            model_requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: None,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            wall_time: started.elapsed(),
            events_dropped: sink.dropped(),
            remaining_budget: self
                .budget
                .remaining(&Accounting::default(), started.elapsed()),
            last_context_census: ContextCensus::default(),
        };
        Ok(summary)
    }

    /// Finalizes a finished task: finish + persist the record, emit the
    /// terminal event, and build the [`TaskSummary`]. The record is
    /// finished before the summary is returned so callers can read
    /// either; `events_dropped` is sampled after the terminal emit so a
    /// closed sink's drop of that event is counted too (§11).
    pub(super) fn finalize_task(
        &self,
        session: &mut Session,
        task_id: TaskId,
        accounting: &Accounting,
        outcome: Result<ContextCensus, TaskError>,
        started: Instant,
        sink: &dyn EventSink,
    ) -> Result<TaskSummary, TaskError> {
        let wall_time = started.elapsed();
        let status = match &outcome {
            Ok(_) => TerminalTaskStatus::Completed,
            Err(error) if error.is_cancelled() => TerminalTaskStatus::Cancelled,
            Err(error) if error.is_paused() => TerminalTaskStatus::Paused,
            Err(_) => TerminalTaskStatus::Failed,
        };
        let end_reason = match &outcome {
            Ok(_) => "end_turn".to_owned(),
            Err(error) => error.end_reason().to_owned(),
        };
        if let Some(record) = session.task_mut(task_id) {
            record.turns = accounting.turns;
            record.usage = accounting.usage;
            record.finish(status, now_millis(), end_reason);
        }
        // Persist the terminal record before announcing it: a store failure
        // fails the task, and no terminal event claims a completion the store
        // never saw.
        if let Some(record) = session.tasks.iter().find(|task| task.id == task_id) {
            self.persist_task(session, record)?;
        }
        match &outcome {
            Ok(_) => sink.emit(task_id, EventKind::TaskCompleted),
            Err(error) if error.is_cancelled() => sink.emit(task_id, EventKind::TaskCancelled),
            Err(error) if error.is_paused() => sink.emit(task_id, EventKind::TaskPaused),
            Err(error) => sink.emit(
                task_id,
                EventKind::TaskFailed {
                    reason: error.to_string(),
                },
            ),
        }
        let events_dropped = sink.dropped();
        let last_context_census = outcome.as_ref().ok().copied().unwrap_or_default();

        let summary = TaskSummary {
            task_id,
            turns: accounting.turns,
            model_requests: accounting.model_requests,
            input_tokens: accounting.usage.input_tokens,
            output_tokens: accounting.usage.output_tokens,
            cached_input_tokens: accounting.usage.cached_input_tokens,
            successful_tool_calls: accounting.successful_tool_calls,
            failed_tool_calls: accounting.failed_tool_calls,
            wall_time,
            events_dropped,
            remaining_budget: self.budget.remaining(accounting, wall_time),
            last_context_census,
        };

        outcome.map(|_| summary)
    }

    /// The turn loop proper (§13). Returns the final compiled request's
    /// census on `EndTurn`, or the error that ended the task.
    #[allow(clippy::too_many_arguments)]
    // the loop's full state; one call site
    // Splitting this state machine would scatter the interleaved
    // persist-then-commit invariants; a refactor is out of scope here.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn run_turns(
        &self,
        session: &mut Session,
        task_id: TaskId,
        sink: &dyn EventSink,
        control: &TaskControl,
        accounting: &mut Accounting,
        started: Instant,
        lease: Option<&LeaseGuard<'_>>,
    ) -> Result<ContextCensus, TaskError> {
        // Capabilities are resolved once: in Iteration 1 the model is fixed
        // for the task (§6.4). An unknown model fails before the first
        // request rather than guessing limits.
        let capabilities =
            self.provider
                .capabilities(&self.model)
                .ok_or_else(|| TaskError::UnknownModel {
                    model: self.model.clone(),
                })?;
        let budget = self.task_budget(&capabilities)?;

        // Repository facts for this task (2E, ADR-015): captured exactly once,
        // before the first compile, so the projected block is byte-stable for
        // the whole task (§8.1.1) and "what changed" never costs a model turn.
        let facts = self.scan_facts().await?;

        // Pruning checkpoint for this task's turn loop (ADR-011): the
        // compiler returns a frozen decision set when its trigger fires,
        // and the loop feeds it back on later turns so the projection stays
        // byte-stable between triggers (one cache miss per trigger). It is
        // per-task loop state, not agent state.
        let mut prune_checkpoint: Option<PruneCheckpoint> = None;

        // Loop-owned reaction to tool failures (§13): the loop, not the
        // model, decides whether a failed attempt may be repeated. An
        // identical call is refused until some tool call succeeds, so a
        // failure forces a change of approach instead of trial and error.
        let mut failures = FailureLedger::default();

        // Item 30 stall-breaker state: per-file consecutive edit-family
        // failures, answered (once per stall) by injecting the file's
        // symbol outline — deterministic context, not a model call.
        let mut edit_stalls = EditStalls::default();

        // Completion gate (roadmap item 8): an EndTurn is refused
        // while unverified mutations are outstanding (bounded by
        // COMPLETION_REFUSAL_CAP), so "done" always refers to a state
        // some green check actually covered.
        let mut completion = CompletionGate::default();
        // Bounded `max_tokens` continuations (§5.1): each one injects a
        // continue reminder instead of failing the task.
        let mut continuations: u8 = 0;

        // The hard-stop token the stream and tool paths select on
        // (§6.5–§6.6); the pause is a turn-boundary decision and never
        // races against I/O.
        let hard = control.hard_token();

        // Zero-progress nudge state (item 1.3): consecutive turns without
        // a successful mutating call, and whether this stall was nudged.
        let mut turns_since_mutation: u32 = 0;
        let mut nudged = false;
        // Turn-budget nudge state: whether this run has been told how much
        // of its turn budget it has spent.
        let mut late_nudged = false;

        // Malformed-salvage bookkeeping: consecutive turns in which
        // every tool call carried unparseable (salvaged) arguments.
        let mut malformed_turns: u32 = 0;
        // A pending failed-call nudge, applied after the turn commits.
        let mut failed_call_nudge: Option<String> = None;

        // Failed-call bookkeeping (2026-10-31): consecutive turns in
        // which every tool call failed with a non-success status.
        let mut failed_call_turns: u32 = 0;
        let mut failure_nudged: bool = false;

        // Staged turn budget (roadmap item 26): the effective limit the
        // loop may extend on progress evidence, capped by max_turns.
        let mut effective_turns = match self.turn_budget.initial_turns {
            0 => self.max_turns,
            initial => initial.min(self.max_turns).max(1),
        };
        let mut turn_extensions: u32 = 0;
        // Mutation epoch when the current grant was decided: the
        // evidence window for the next extension decision.
        let mut mutations_at_grant: u64 = completion.mutations;

        for turn in 1..=self.max_turns {
            // Single-writer lease (ADR-008): renew before spending the
            // turn, so a task longer than the TTL stays covered. A lost
            // renewal (expired and taken over) ends the task rather than
            // interleaving two writers' turns.
            if let (Some(store), Some(guard)) = (&self.store, lease) {
                let held = store
                    .renew_lease(&session.id, guard.lease.owner(), LEASE_TTL_MILLIS)
                    .map_err(persistence)?;
                if !held {
                    return Err(lease_holder_for(store.as_ref(), &session.id));
                }
            }

            // A pause requested before the task or during the previous
            // turn's tool batch stops here: the in-flight turn has fully
            // committed, nothing is dropped, and the session is
            // resumable. The user's signal wins over further spending.
            if control.is_paused() {
                return Err(TaskError::Paused);
            }

            // Enforce the task's resource envelope before spending anything
            // further (§14). Checked once per turn, so the unit in flight when
            // a ceiling was crossed still completed and stays committed.
            if let Some(limit) = self.budget.exhausted(accounting, started.elapsed()) {
                return Err(TaskError::BudgetExhausted { limit });
            }

            // The staged-budget gate (item 26): the effective grant is
            // spent. Extend only on deterministic progress evidence —
            // never a model call — or stop as a turn-limit failure.
            // With the default budget this arm is unreachable.
            if turn > effective_turns {
                match turn_extension_reason(
                    &self.turn_budget,
                    turn_extensions,
                    effective_turns,
                    self.max_turns,
                    &completion,
                    mutations_at_grant,
                ) {
                    Some(reason) => {
                        turn_extensions += 1;
                        mutations_at_grant = completion.mutations;
                        effective_turns = effective_turns
                            .saturating_add(self.turn_budget.extension_turns)
                            .min(self.max_turns);
                        sink.emit(
                            task_id,
                            EventKind::TurnBudgetExtended {
                                turn,
                                reason,
                                new_limit: effective_turns,
                            },
                        );
                    }
                    None => {
                        return Err(TaskError::MaxTurns {
                            max_turns: effective_turns,
                        });
                    }
                }
            }

            sink.emit(task_id, EventKind::TurnStarted { turn });

            // The dynamic tool surface (slice D2): announce the turn,
            // then snapshot this turn's registry view — base plus the
            // surface's loaded tools, append-only, so every compile,
            // dispatch, and mutation check in this turn agrees on one
            // tool list.
            if let Some(surface) = &self.surface {
                surface.begin_turn(turn);
            }
            let turn_tools = self.combined_registry();

            // Queued user steering (item 25): drained once per turn
            // boundary and appended — persist-then-commit, exactly as a
            // task's opening input — so this turn's compile sees it.
            // Never mid-turn: the in-flight reply is never reordered.
            let steered: Vec<String> = match control.steering.lock() {
                // poisoned: drop the batch, like the event store's skips
                Ok(mut queue) => std::mem::take(&mut *queue),
                Err(_) => Vec::new(),
            };
            if !steered.is_empty() {
                // Queue depth is bounded by the embedder's steering rate —
                // far below `u32::MAX`.
                #[allow(clippy::cast_possible_truncation)]
                let steered_count = steered.len() as u32;
                for text in steered {
                    let user = Item::User { text };
                    if let Some(store) = &self.store {
                        store
                            .append_item(&session.id, session.next_index(), &user)
                            .map_err(persistence)?;
                    }
                    session
                        .push_item(user)
                        .map_err(|error| TaskError::Internal(error.to_string()))?;
                }
                sink.emit(
                    task_id,
                    EventKind::SteeringApplied {
                        turn,
                        messages: steered_count,
                    },
                );
            }
            // The zero-progress nudge (item 1.3): once this run has spent
            // ZERO_PROGRESS_TURNS turns without changing a file, say so
            // once, in the model's own input stream — persist-then-commit
            // like steering, so this turn's compile sees it.
            let stalled = zero_progress_due(turns_since_mutation, nudged);
            if stalled {
                nudged = true;
                let notice = Item::User {
                    text: format!(
                        "No file has changed in the last {turns_since_mutation} turns. Stop \
                         exploring and land the best patch you can now: change the workspace \
                         with write_file or edit_file, then verify it with cargo."
                    ),
                };
                let nudge_text = match &notice {
                    Item::User { text } => text.clone(),
                    Item::Assistant(_) | Item::ToolResult(_) => String::new(),
                };
                sink.emit(task_id, EventKind::LoopNudge { text: nudge_text });
                if let Some(store) = &self.store {
                    store
                        .append_item(&session.id, session.next_index(), &notice)
                        .map_err(persistence)?;
                }
                session
                    .push_item(notice)
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
            }
            // The turn-budget nudge: the zero-progress nudge above can never
            // fire while the model keeps editing, because every successful
            // edit resets its counter, so a run that churns through edits
            // hears nothing at all. This notice counts turns only and fires
            // at most once, while a change of approach is still affordable.
            if !stalled && late_budget_due(turn, effective_turns, late_nudged) {
                late_nudged = true;
                let notice = Item::User {
                    text: format!(
                        "You have used {turn} of {effective_turns} turns. If the same check is \
                         still failing, stop repeating the same edit: re-read the whole failure \
                         output, say what it proves, and change approach.",
                    ),
                };
                let nudge_text = match &notice {
                    Item::User { text } => text.clone(),
                    Item::Assistant(_) | Item::ToolResult(_) => String::new(),
                };
                sink.emit(task_id, EventKind::LoopNudge { text: nudge_text });
                if let Some(store) = &self.store {
                    store
                        .append_item(&session.id, session.next_index(), &notice)
                        .map_err(persistence)?;
                }
                session
                    .push_item(notice)
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
            }
            // Item 30 stall-breaker: two consecutive failed edit-family
            // calls on one file mean the model is fighting the file's
            // shape, not one unlucky call. The loop answers with that
            // file's symbol outline — deterministic context fetched
            // without a model call (TokenEconomizer, arXiv 2610.22732) —
            // and only when `symbols_overview` is registered, so the
            // frozen baseline surface is unchanged. Injected once per
            // stall; a success on the file resets the counter.
            if let Some((path, count)) = edit_stalls.stalled_file()
                && let Some(outline) = self.symbol_outline(&turn_tools, &path, &hard).await
            {
                edit_stalls.resolved(&path);
                let notice = Item::User {
                    text: format!(
                        "{count} consecutive edits failed on {path}. Here is the file's \
                         symbol outline:\n{outline}"
                    ),
                };
                let nudge_text = match &notice {
                    Item::User { text } => text.clone(),
                    Item::Assistant(_) | Item::ToolResult(_) => String::new(),
                };
                sink.emit(task_id, EventKind::LoopNudge { text: nudge_text });
                if let Some(store) = &self.store {
                    store
                        .append_item(&session.id, session.next_index(), &notice)
                        .map_err(persistence)?;
                }
                session
                    .push_item(notice)
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
            }
            // a. Compile the model request from durable state (§8). The
            //    compiler owns every context decision; the loop only passes
            //    it the inputs. When even the compiler's rescue arm cannot
            //    fit the projection and handoff compaction is armed, the
            //    loop spends one model call on a goal-directed summary and
            //    retries the compile on the handoff view (roadmap item
            //    12's last resort — once per task; a second overflow is
            //    the task's error).
            let compiled = match self.compile_turn(
                session,
                task_id,
                budget,
                &capabilities,
                &facts,
                prune_checkpoint.as_ref(),
                &turn_tools,
            ) {
                Ok(compiled) => compiled,
                Err(overrun @ TaskError::Context(ContextError::BudgetExceeded { .. }))
                    if self.config.handoff_compaction
                        && prune_checkpoint
                            .as_ref()
                            .is_none_or(|frozen| frozen.handoff.is_none()) =>
                {
                    let Some(checkpoint) = self
                        .attempt_handoff(session, task_id, sink, &hard, accounting, turn)
                        .await?
                    else {
                        return Err(overrun);
                    };
                    prune_checkpoint = Some(checkpoint);
                    self.compile_turn(
                        session,
                        task_id,
                        budget,
                        &capabilities,
                        &facts,
                        prune_checkpoint.as_ref(),
                        &turn_tools,
                    )?
                }
                Err(error) => return Err(error),
            };
            // Carry the pruning checkpoint forward (ADR-011): it is task
            // state, not request state, so it lives on the loop and is fed
            // back in on the next turn. Between triggers it is unchanged,
            // so the projection stays byte-stable for prefix caching.
            prune_checkpoint = compiled.prune_checkpoint;
            // A new freeze is an observable mechanism decision (item
            // 20.1): attribute it at the moment it happens, not on every
            // later turn the frozen stubs merely re-apply.
            if let Some(report) = compiled.prune_report {
                sink.emit(task_id, EventKind::ContextPruned { turn, report });
            }
            sink.emit(
                task_id,
                EventKind::ContextProgress {
                    turn,
                    context_budget: budget,
                    census: compiled.census,
                    remaining_budget: self.budget.remaining(accounting, started.elapsed()),
                },
            );
            // Partial move: `request` moves out, `census` (Copy) stays readable
            // for the `EndTurn` return.
            let stubbed_calls = compiled.stubbed_calls;
            let request = compiled.request;

            // b. Stream with bounded retry (§6.3). Only a `Completed` event
            //    produces a response; failed attempts leave no trace.
            let response = self
                .stream_with_retry(&request, task_id, sink, &hard, accounting)
                .await?;
            accounting.usage = accounting.usage + response.usage;
            let assistant = response.message;
            let has_calls = !assistant.tool_calls.is_empty();

            // c. Inspect the stop reason (§5.1, §13).
            match assistant.stop_reason {
                StopReason::EndTurn => {
                    if has_calls {
                        return Err(TaskError::Protocol(
                            "EndTurn response carried tool calls".to_owned(),
                        ));
                    }
                    // Completion gate (roadmap item 8): refuse the finish
                    // once when files changed since the last green check,
                    // quoting the changed paths and the original task.
                    // One refusal only: the model may insist, and the
                    // loop never deadlocks with a task that cannot be
                    // verified.
                    if self.refuse_completion_once(
                        session,
                        task_id,
                        turn,
                        &mut completion,
                        accounting,
                        &assistant,
                        sink,
                    )? {
                        self.reconcile_surface();
                        continue;
                    }
                    self.commit(session, assistant, Vec::new())?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    return Ok(compiled.census);
                }
                StopReason::ToolCalls => {
                    if !has_calls {
                        return Err(TaskError::Protocol(
                            "ToolCalls stop reason without tool calls".to_owned(),
                        ));
                    }
                    // The session's todo list travels through the batch as
                    // a shared handle the `todo` tool mutates (roadmap
                    // item 8 phase B); the snapshot below is copied back
                    // after the turn commits.
                    let todo_revision = session.todos.revision;
                    let todos = Arc::new(Mutex::new(session.todos.clone()));
                    let results = self
                        .execute_batch(
                            &assistant.tool_calls,
                            &request,
                            &stubbed_calls,
                            task_id,
                            sink,
                            &hard,
                            accounting,
                            &mut failures,
                            &recent_changes(session),
                            Arc::clone(&todos),
                            &turn_tools,
                        )
                        .await;
                    // The gate learns from every result before the turn
                    // commits: mutations bump the epoch, green checks pin
                    // it. A fused edit (write + green check in one result)
                    // lands verified.
                    for result in &results {
                        completion.note(result);
                    }
                    // Zero-progress bookkeeping (item 1.3): a successful
                    // mutating call is progress; anything else is another
                    // turn spent looking. Computed before the commit, which
                    // takes the assistant message and the results by value.
                    let mutated =
                        assistant
                            .tool_calls
                            .iter()
                            .zip(&results)
                            .any(|(call, result)| {
                                result.status == ToolStatus::Success
                                    && turn_tools
                                        .get(&call.name)
                                        .is_some_and(|tool| !tool.is_read_only())
                            });
                    // Item 30 stall-breaker bookkeeping: per-file
                    // consecutive edit-family failures feed the outline
                    // injection next turn.
                    for (call, result) in assistant.tool_calls.iter().zip(&results) {
                        edit_stalls.note(call, result);
                    }
                    // A batch that was entirely salvaged (unparseable)
                    // calls counts toward the malformed ceiling; three
                    // in a row fails the task before the commit, per
                    // the failed-attempts-leave-no-trace rule (§6.3).
                    let all_malformed = !assistant.tool_calls.is_empty()
                        && assistant
                            .tool_calls
                            .iter()
                            .all(super::super::transcript::ToolCall::arguments_are_malformed);
                    if all_malformed {
                        malformed_turns += 1;
                        if malformed_turns >= MALFORMED_ARGUMENT_TURNS_LIMIT {
                            return Err(TaskError::Protocol(
                                "model emitted malformed tool-call arguments in \
                                 3 consecutive turns"
                                    .into(),
                            ));
                        }
                    } else {
                        malformed_turns = 0;
                    }

                    // The failed-call loop breaker: well-formed calls the
                    // tools rejected (wrong shape, denied) are
                    // recoverable — nudge with the error text — but a
                    // model still failing everything after the nudge is
                    // stuck (13398: 130 all-failed turns, uncounted).
                    let any_succeeded = !results.is_empty()
                        && results
                            .iter()
                            .any(|result| result.status == ToolStatus::Success);
                    if !results.is_empty() && !any_succeeded && !all_malformed {
                        failed_call_turns += 1;
                        if failed_call_turns >= FAILED_CALL_TURNS_LIMIT {
                            return Err(TaskError::Protocol(
                                "every tool call failed in \
                                 FAILED_CALL_TURNS_LIMIT consecutive turns"
                                    .into(),
                            ));
                        }
                        if failed_call_turns >= FAILED_CALL_TURNS_NUDGE && !failure_nudged {
                            failure_nudged = true;
                            let sample = results
                                .iter()
                                .find(|result| result.status != ToolStatus::Success)
                                .map_or_default(|result| {
                                    result.content.chars().take(300).collect::<String>()
                                });
                            // Applied after this turn commits, so the
                            // transcript keeps assistant-then-nudge order.
                            failed_call_nudge = Some(format!(
                                "Every tool call in the last {failed_call_turns} turns \
                                 failed. Latest error: {sample}Stop guessing: re-read the \
                                 tool's expected fields from its description, then send one \
                                 correctly shaped call. For shell pipelines, run the \
                                 pipeline through `bash -c \"…\"` rather than passing \
                                 shell syntax where a bare program name is expected."
                            ));
                        }
                    } else if any_succeeded {
                        failed_call_turns = 0;
                        failure_nudged = false;
                    }
                    // A batch interrupted by cancellation still commits its
                    // completed results plus cancelled fillers (§6.1, §10).
                    self.commit(session, assistant, results)?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    // The failed-call corrective nudge lands after the
                    // commit: transcript order stays assistant-then-
                    // nudge, and the next model request sees it.
                    if let Some(text) = failed_call_nudge.take() {
                        let notice = Item::User { text: text.clone() };
                        sink.emit(task_id, EventKind::LoopNudge { text });
                        if let Some(store) = &self.store {
                            store
                                .append_item(&session.id, session.next_index(), &notice)
                                .map_err(persistence)?;
                        }
                        session
                            .push_item(notice)
                            .map_err(|error| TaskError::Internal(error.to_string()))?;
                    }
                    if mutated {
                        turns_since_mutation = 0;
                        nudged = false;
                    } else {
                        turns_since_mutation += 1;
                    }
                    // Copy the (possibly mutated) todo state back, and —
                    // only when it changed — re-append the rendered list
                    // at the transcript tail. This runs before the
                    // cancellation check on purpose: a changed list is
                    // durable state the next session load must see.
                    let todos = todos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if todos.revision != todo_revision {
                        session.todos = todos;
                        self.append_todo_block(session)?;
                        sink.emit(
                            task_id,
                            EventKind::TodoUpdated {
                                revision: session.todos.revision,
                                items: session.todos.items.clone(),
                            },
                        );
                    }
                    if control.is_cancelled() {
                        return Err(TaskError::Cancelled);
                    }
                    // A pause that fired during the batch stops here too:
                    // the batch committed, and the user's signal wins
                    // over the max-turns bookkeeping below.
                    if control.is_paused() {
                        return Err(TaskError::Paused);
                    }
                    if turn == self.max_turns {
                        return Err(TaskError::MaxTurns {
                            max_turns: self.max_turns,
                        });
                    }
                    // Otherwise continue to the next turn; the surface
                    // first promotes this turn's queued loads and evicts
                    // idle tools.
                    self.reconcile_surface();
                }
                StopReason::MaxTokens if !has_calls && continuations < MAX_TOKENS_CONTINUATIONS => {
                    // Standard continue pattern: commit the truncated
                    // reply, then ask the model to resume exactly where
                    // it stopped. Bounded — a model that can never finish
                    // within the cap still surfaces the failure.
                    continuations += 1;
                    self.commit(session, assistant, Vec::new())?;
                    let reminder = Item::User {
                        text: MAX_TOKENS_CONTINUATION_PROMPT.to_owned(),
                    };
                    if let Some(store) = &self.store {
                        store
                            .append_item(&session.id, session.next_index(), &reminder)
                            .map_err(persistence)?;
                    }
                    session
                        .push_item(reminder)
                        .map_err(|error| TaskError::Internal(error.to_string()))?;
                    sink.emit(
                        task_id,
                        EventKind::LoopNudge {
                            text: MAX_TOKENS_CONTINUATION_PROMPT.to_owned(),
                        },
                    );
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    if turn == self.max_turns {
                        return Err(TaskError::MaxTurns {
                            max_turns: self.max_turns,
                        });
                    }
                    self.reconcile_surface();
                }
                // `context_limit` / `content_filter` (and any future
                // reason), or `max_tokens` past the continuation cap.
                // The reply is committed first so it stays inspectable,
                // then the task fails.
                other @ (StopReason::MaxTokens
                | StopReason::ContextLimit
                | StopReason::ContentFilter) => {
                    if has_calls {
                        return Err(TaskError::Protocol(format!(
                            "{} response carried tool calls",
                            stop_reason_token(other)
                        )));
                    }
                    self.commit(session, assistant, Vec::new())?;
                    accounting.turns = turn;
                    accounting.sync(session, task_id);
                    return Err(TaskError::UnrecoverableStop {
                        stop_reason: stop_reason_token(other),
                    });
                }
            }
        }

        // The loop only exits without returning when every turn requested
        // tools and none was the last — impossible: `turn == max_turns` is
        // handled inside, so reaching here means `max_turns` was zero, which
        // the builder forbids. Report it as a turn-limit failure regardless.
        Err(TaskError::MaxTurns {
            max_turns: self.max_turns,
        })
    }
}
