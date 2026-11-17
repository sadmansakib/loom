//! Cooperative stop control and the session's single-writer lease
//! (§6, ADR-008): the operator's cancel/pause/steer handle and the
//! lease plumbing the run holds for its whole lifetime.

use super::TaskError;
use crate::id::SessionId;
use crate::store::{Lease, SessionStore};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Cooperative stop control for one task run: the operator's two
/// granularities (§6).
///
/// [`cancel`](TaskControl::cancel) is the hard stop that already existed:
/// the in-flight model stream is dropped, tool calls receive cancelled
/// fillers, and the task ends as
/// [`Cancelled`](TaskError::Cancelled). The session survives — every
/// committed item was persisted before the abort — so "abort the call"
/// never means "lose the session".
///
/// [`pause`](TaskControl::pause) is the graceful stop: the in-flight
/// turn (model reply and tool batch) runs to completion and commits,
/// and the loop stops at the next turn boundary with
/// [`Paused`](TaskError::Paused). A second pause adds nothing; a later
/// [`cancel`](TaskControl::cancel) still wins mid-turn.
///
/// Clones share state, so a UI thread can hold one copy while the
/// driving task consumes another.
#[derive(Clone, Debug)]
pub struct TaskControl {
    /// The hard stop honored by the stream and tool paths (§6.5–§6.6).
    pub(super) hard: CancellationToken,
    /// The graceful stop, observed only at turn boundaries.
    pub(super) soft: CancellationToken,
    /// Queued user steering (item 25): appended to the transcript at
    /// the next turn boundary. `Arc` so clones share one queue,
    /// matching token semantics.
    pub(super) steering: Arc<Mutex<Vec<String>>>,
    /// Operator-armed compaction (`/compact`): consumed by the turn
    /// loop before the first compile, which runs the handoff eagerly
    /// instead of waiting for a budget overrun. `Arc` so clones share
    /// one latch, matching token semantics.
    pub(super) compaction: Arc<AtomicBool>,
    /// Retry-now wake-up for an in-flight model-request backoff. The value is
    /// a generation counter, so a press made before a wait begins is consumed
    /// by that wait's initial `borrow_and_update` and cannot skip a later one.
    pub(super) retry: Arc<watch::Sender<u64>>,
}

impl Default for TaskControl {
    fn default() -> Self {
        let (retry, _) = watch::channel(0);
        Self {
            hard: CancellationToken::new(),
            soft: CancellationToken::new(),
            steering: Arc::new(Mutex::new(Vec::new())),
            compaction: Arc::new(AtomicBool::new(false)),
            retry: Arc::new(retry),
        }
    }
}

impl TaskControl {
    /// A fresh control with neither stop fired.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Abort now: mid-stream, mid-tool, wherever the task is.
    pub fn cancel(&self) {
        self.hard.cancel();
    }

    /// Stop after the current turn completes. Checked at turn
    /// boundaries only — the in-flight turn always finishes.
    pub fn pause(&self) {
        self.soft.cancel();
    }

    /// Whether the hard stop has fired.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.hard.is_cancelled()
    }

    /// Whether a graceful pause has been requested.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.soft.is_cancelled()
    }

    /// Skip the rest of an in-flight retry backoff and start the next attempt
    /// immediately. A no-op when no backoff is running; the signal never
    /// cancels the request or restarts the turn.
    pub fn retry_now(&self) {
        self.retry
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// A receiver that wakes when [`retry_now`](TaskControl::retry_now) fires.
    pub(super) fn retry_receiver(&self) -> watch::Receiver<u64> {
        self.retry.subscribe()
    }

    /// Queue a user message for the running task (item 25). Applied at
    /// the next turn boundary — appended to the transcript as a user
    /// item before the turn's compile — never mid-turn. Messages
    /// queued while paused survive until the resumed task's next
    /// boundary.
    pub fn steer(&self, message: impl Into<String>) {
        let Ok(mut queue) = self.steering.lock() else {
            return; // poisoned: a silent skip, like the event store
        };
        queue.push(message.into());
    }
    /// The hard-stop token, for callers that wire a cancellation into
    /// existing `CancellationToken` consumers (tool contexts, stream
    /// selects). The pause has no token form on purpose: it is a
    /// turn-boundary decision, not something to race against I/O.
    #[must_use]
    pub fn hard_token(&self) -> CancellationToken {
        self.hard.clone()
    }

    /// Arm an eager compaction (`/compact`): the turn loop runs the
    /// handoff summary before its next compile instead of waiting for
    /// a context budget overrun. One-shot — `Agent::run_task` consumes
    /// the request even when the handoff reply is unusable, mirroring
    /// the overrun path's once-per-task limit.
    pub fn request_compaction(&self) {
        self.compaction.store(true, Ordering::Relaxed);
    }

    /// Whether a compaction was armed, clearing the request. Kernel
    /// internal: the turn loop calls this at its compile boundary.
    pub(super) fn take_compaction_request(&self) -> bool {
        self.compaction.swap(false, Ordering::Relaxed)
    }
}

impl From<CancellationToken> for TaskControl {
    /// Wraps a bare hard-stop token; the result can never pause.
    fn from(hard: CancellationToken) -> Self {
        Self {
            hard,
            soft: CancellationToken::new(),
            steering: Arc::new(Mutex::new(Vec::new())),
            compaction: Arc::new(AtomicBool::new(false)),
            retry: Arc::new(watch::channel(0).0),
        }
    }
}

/// How long a session's single-writer lease survives without renewal
/// (ADR-008). The loop renews every turn, so a live task holds its lease
/// for its whole run; a crashed writer blocks resumption for at most
/// this long.
pub(super) const LEASE_TTL_MILLIS: u64 = 5 * 60 * 1000;

/// Identifies one lease holder. The pid alone would let a second handle
/// in the same process re-acquire as the "same" owner and silently
/// interleave, so each acquisition gets a unique sequence suffix.
pub(super) fn lease_owner_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("pid-{}-{}", std::process::id(), sequence)
}

/// Releases the session lease when the run leaves `run_task`, whatever
/// the path out — success, error, cancellation, or panic. A failed
/// release is best-effort: the lease still expires by TTL.
pub(super) struct LeaseGuard<'a> {
    pub(super) store: &'a dyn SessionStore,
    pub(super) lease: Lease,
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .store
            .release_lease(self.lease.session(), self.lease.owner());
    }
}

/// The `(owner, expires_at_millis)` an error message should name when
/// the lease was denied, or a placeholder when the store cannot say.
pub(super) fn lease_holder_for(store: &dyn SessionStore, session: &SessionId) -> TaskError {
    let (owner, expires_at_millis) = store
        .lease_holder(session)
        .ok()
        .flatten()
        .unwrap_or_else(|| ("another writer".to_owned(), 0));
    TaskError::SessionLeased {
        owner,
        expires_at_millis,
    }
}
