//! The task run: [`Engine::run_task`], its stall-escalation fork
//! ([`Engine::escalate_stall`]), and the driving loop underneath.

use loom_kernel::agent::{TaskControl, TaskError, TaskSummary};
use loom_kernel::event::{EventKind, EventSink, PersistingEventSink};
use loom_kernel::id::{SessionId, TaskId};
use loom_kernel::session::Session;
use loom_kernel::store::SessionStore;
use loom_kernel::transcript::ItemIndex;
use loom_store::SqliteStore;

use super::Engine;

/// Counts [`EventKind::LoopNudge`] emissions for one run while
/// forwarding every event untouched (roadmap item 36): escalation
/// fires only for stalls the nudge machinery already tried to
/// rescue, which is exactly the deterministic signature a fork
/// retry can plausibly beat.
struct StallObserver<'a> {
    inner: &'a dyn EventSink,
    nudges: std::sync::atomic::AtomicU32,
}

impl<'a> StallObserver<'a> {
    fn new(inner: &'a dyn EventSink) -> Self {
        Self {
            inner,
            nudges: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn nudges(&self) -> u32 {
        self.nudges.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl EventSink for StallObserver<'_> {
    fn emit(&self, task_id: TaskId, kind: EventKind) {
        if matches!(kind, EventKind::LoopNudge { .. }) {
            self.nudges
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.inner.emit(task_id, kind);
    }

    fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
}

impl Engine {
    /// Runs one task against `session`, emitting events to `sink`.
    ///
    /// When the engine was opened with persistence, the sink is wrapped
    /// so every event is also appended to the session's durable event
    /// stream (append-on-emit, best-effort) before `sink` sees it — the
    /// same events, the same order, one durable copy.
    ///
    /// Cancellation is cooperative through `cancellation`. Success returns a
    /// [`TaskSummary`]; every other terminal state — including cancellation —
    /// returns a [`TaskError`], and the persisted [`Session`] carries the same
    /// outcome.
    ///
    /// After the task's terminal event the engine emits
    /// [`EventKind::AwaitingInput`] naming `session`: the session is idle and
    /// accepts more input, whether through another `run_task` call or
    /// [`Engine::continue_session`]. Stream-only frontends key their resume
    /// loop on it.
    ///
    /// `control` carries the operator's two stop granularities:
    /// [`TaskControl::cancel`] aborts mid-flight (the session survives —
    /// every committed item was persisted), [`TaskControl::pause`] stops at
    /// the next turn boundary with the in-flight turn fully committed.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError`] for every terminal failure: context
    /// compilation refusing the request, a model request failing after its
    /// retries, a protocol violation in the model's response, the turn or
    /// task budget being exhausted, or cancellation. None of these lose
    /// committed state — the session keeps everything persisted so far.
    pub async fn run_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        // Event persistence (roadmap item 1's open tail): when the
        // engine persists sessions it persists the observed event
        // stream alongside them, append-on-emit, best-effort. The
        // wrapper sits above everything this method (and the loop
        // under it) emits, so the stored stream is the complete run —
        // including the engine-level markers below.
        //
        // Escalation-by-fork (item 36) needs to know whether the run
        // nudged before it failed, so an observer counts `LoopNudge`
        // emissions while forwarding everything untouched — below the
        // persistence wrapper, so stored streams stay byte-identical.
        let input_text: String = input.into();
        let observer = StallObserver::new(sink);
        let outcome = match &self.events {
            Some(store) => {
                let persisting = PersistingEventSink::new(store.clone(), session.id, &observer);
                self.drive_task(session, input_text.clone(), &persisting, control)
                    .await
            }
            None => {
                self.drive_task(session, input_text.clone(), &observer, control)
                    .await
            }
        };
        // Never-worse gate: escalation fires only for the deterministic
        // stall signature — max turns or budget exhausted *after* a
        // loop nudge. Cancels, model errors, and protocol failures
        // return exactly as before.
        let stalled = matches!(
            &outcome,
            Err(TaskError::MaxTurns { .. } | TaskError::BudgetExhausted { .. })
        ) && observer.nudges() > 0;
        if !stalled {
            return outcome;
        }
        match self
            .escalate_stall(session, &input_text, sink, control)
            .await
        {
            Some(summary) => Ok(summary),
            None => outcome,
        }
    }

    /// Roadmap item 36's retry: fork the stalled session at its tip
    /// and run the escalation agent on the fork. The original session
    /// is never modified; a failed escalation returns `None` so the
    /// caller reports the original error (never-worse). Requires a
    /// persisting engine — a fork needs the durable store.
    async fn escalate_stall(
        &self,
        session: &mut Session,
        input: &str,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Option<TaskSummary> {
        let escalation = self.stall_escalation.as_ref()?;
        let db_path = self.db_path.as_ref()?.clone();
        let fork_id = SessionId::new();
        #[allow(clippy::cast_possible_truncation)] // transcript lengths stay far below u32::MAX
        let boundary = ItemIndex::from(session.transcript.len() as u32);
        let store = SqliteStore::open(db_path).ok()?;
        store.fork(&session.id, boundary, &fork_id).ok()?;
        let mut forked = self.load_session(&fork_id).ok()??;
        // Attribution lands in the decision footer (the routing
        // notice's channel); the fork id makes the retry resumable.
        if let Some(record) = session.tasks.last() {
            sink.emit(
                record.id,
                EventKind::HarnessNotice {
                    category: "escalation".to_string(),
                    detail: format!(
                        "stalled after nudge; forked to {fork_id}, retrying with {}",
                        escalation.model
                    ),
                },
            );
        }
        escalation
            .agent
            .run_task(&mut forked, input.to_string(), sink, control)
            .await
            .ok()
    }

    /// [`Engine::run_task`]'s body, driven against whichever sink the
    /// persistence wrapper produced.
    async fn drive_task(
        &self,
        session: &mut Session,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<TaskSummary, TaskError> {
        let input: String = input.into();
        // The fast paths (roadmap item 3): a strict analyzer may solve a
        // mechanical instruction deterministically — edits applied,
        // compiler-verified green — with zero model calls. Any miss,
        // declined guard, or reverted failure falls through to the
        // agent loop over an untouched workspace, so the loop is never
        // worse off for trying.
        let mut fast_path_decline: Option<&'static str> = None;
        if self.fast_paths {
            let attempt = loom_tools::fastpath::try_fast_path(
                &self.workspace,
                &input,
                &self.policy,
                &self.lsp_servers,
            )
            .await;
            match attempt.outcome {
                Some(solve) => {
                    let summary = self
                        .agent
                        .record_fast_path(
                            session,
                            input,
                            loom_kernel::agent::FastPathSolve {
                                kind: solve.kind.to_owned(),
                                files_changed: solve.files_changed,
                                note: solve.note,
                            },
                            sink,
                        )
                        .await?;
                    // Same idle marker the loop-driven path emits: the
                    // session accepts follow-up input either way.
                    if let Some(record) = session.tasks.last() {
                        sink.emit(
                            record.id,
                            EventKind::AwaitingInput {
                                session_id: session.id,
                            },
                        );
                    }
                    return Ok(summary);
                }
                None => fast_path_decline = attempt.decline,
            }
        }
        // Routing slice zero (roadmap item 9): pick the tier agent
        // before the loop; Medium — or a tier with no wired model —
        // keeps the run's own agent. The choice surfaces post-hoc
        // below, attributed to the task like the fast-path decline.
        // (The fast-path branch above never reaches a model, so it
        // needs no routing.)
        let tier = crate::routing::classify(&input);
        let routed_agent = match tier {
            crate::routing::Tier::Easy => self.routing_easy.as_ref(),
            crate::routing::Tier::Hard => self.routing_hard.as_ref(),
            crate::routing::Tier::Medium => None,
        };
        let routed_notice = routed_agent.map(|routed| (tier, routed.model.clone()));
        // Roadmap item 13: while the loop runs, a watcher polls the
        // background-job registry and steers completion notices in —
        // they land at the next turn boundary as user items, exactly
        // like a human steering mid-task. Prior completions (jobs that
        // finished between tasks) are drained up front so the first
        // turn of this task sees them. Notices that land after the
        // final turn stay queued for the next run_task — nothing is
        // lost, and the frozen eval protocol never sees any of this
        // (background_commands is off there).
        let background_watcher = self
            .background
            .as_ref()
            .map(|jobs| spawn_background_watcher(jobs, control));
        let outcome = match routed_agent {
            Some(routed) => routed.agent.run_task(session, input, sink, control).await,
            None => self.agent.run_task(session, input, sink, control).await,
        };
        if let Some(watcher) = background_watcher {
            watcher.abort();
        }
        // v8 surfaced decisions, attributed to the task that just ran.
        self.emit_surfaced_decisions(session, sink, fast_path_decline, routed_notice.as_ref());
        // The engine is idle again once the task's terminal event has been
        // emitted. The marker carries the ended task's id (envelope shape)
        // and the session (payload); a session whose very first user item
        // failed to persist has no task record, and no marker either.
        if let Some(record) = session.tasks.last() {
            sink.emit(
                record.id,
                EventKind::AwaitingInput {
                    session_id: session.id,
                },
            );
        }
        outcome
    }

    /// v8 surfaced decisions, attributed to the task that just ran:
    /// the fast-path attempt that fell through (post-hoc — the
    /// attempt happens before a task record exists), then the
    /// engine's one-shot resolution facts.
    fn emit_surfaced_decisions(
        &self,
        session: &Session,
        sink: &dyn EventSink,
        fast_path_decline: Option<&'static str>,
        routed_notice: Option<&(crate::routing::Tier, String)>,
    ) {
        let Some(record) = session.tasks.last() else {
            return;
        };
        if let Some(reason) = fast_path_decline {
            sink.emit(
                record.id,
                EventKind::FastPathDeclined {
                    reason: reason.to_owned(),
                },
            );
        }
        if let Some((tier, model)) = routed_notice {
            sink.emit(
                record.id,
                EventKind::HarnessNotice {
                    category: String::from("routing"),
                    detail: format!("task routed to {model} (tier {})", tier.as_str()),
                },
            );
        }
        if self.notices_flushed.set(()).is_ok()
            && let Some((source, model)) = &self.resolution
        {
            sink.emit(
                record.id,
                EventKind::CapabilitiesResolved {
                    model: model.clone(),
                    source: (*source).to_owned(),
                },
            );
            sink.emit(
                record.id,
                EventKind::HarnessNotice {
                    category: String::from("sandbox"),
                    detail: loom_tools::sandbox::baseline().to_owned(),
                },
            );
        }
    }
}

/// Polls the background-job registry while the loop runs (roadmap
/// item 13): a completion notice is steered into the task at the next
/// turn boundary, exactly like a human steering mid-task.
fn spawn_background_watcher(
    jobs: &loom_tools::BackgroundJobs,
    control: &TaskControl,
) -> tokio::task::JoinHandle<()> {
    for notice in jobs.take_finished_notices() {
        control.steer(notice);
    }
    let jobs = jobs.clone();
    let control = control.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            for notice in jobs.take_finished_notices() {
                control.steer(notice);
            }
        }
    })
}
