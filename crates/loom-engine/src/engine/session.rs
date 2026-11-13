//! Session lifecycle: fresh session handles, resumed sessions, and the
//! persisted session-id listing.

use loom_kernel::agent::TaskControl;
use loom_kernel::event::EventSink;
use loom_kernel::id::SessionId;
use loom_kernel::session::Session;

use super::{Continuation, Engine, EngineError};

impl Engine {
    /// Creates a fresh session handle for one run.
    ///
    /// The engine owns session *durability*; the frontend owns session
    /// *identity and rendering*. When the engine was opened with persistence,
    /// the id can later be handed to [`Engine::load_session`] to resume. A
    /// frontend that keeps the value can also pass it straight back to
    /// [`Engine::run_task`] for follow-up tasks in the same session.
    #[must_use]
    pub fn new_session(&self) -> Session {
        Session::new(SessionId::new())
    }

    /// Continues a persisted session: loads it, appends `input` as its
    /// next user item, and runs the task loop to the next stopping point.
    ///
    /// The first-class resume flow (the core seam of roadmap item 15): the
    /// advanced [`Session`] and the task's outcome travel back together in a
    /// [`Continuation`]. A failed, cancelled, or paused task is **not** an
    /// `Err` here — the session has still advanced (input committed, partial
    /// turns persisted) and the caller can continue again — only loading
    /// problems are errors: an unknown id (or no persistence) is
    /// [`EngineError::SessionNotFound`], a store failure
    /// [`EngineError::Session`].
    ///
    /// Blocking up front (SQLite load); same call-off-thread guidance as
    /// [`Self::load_session`].
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::SessionNotFound`] when no persisted session
    /// matches `id` (or persistence is off), [`EngineError::Session`] when
    /// the session store cannot be read, and the task's own [`TaskError`]
    /// (wrapped, see [`Continuation::outcome`]) under the conditions of
    /// [`Engine::run_task`].
    pub async fn continue_session(
        &self,
        id: &SessionId,
        input: impl Into<String>,
        sink: &dyn EventSink,
        control: &TaskControl,
    ) -> Result<Continuation, EngineError> {
        let mut session = self
            .load_session(id)?
            .ok_or(EngineError::SessionNotFound { id: *id })?;
        let outcome = self.run_task(&mut session, input, sink, control).await;
        Ok(Continuation { session, outcome })
    }

    /// Loads a persisted session so a frontend can resume it.
    ///
    /// Returns `Ok(None)` when persistence is disabled or the session is
    /// unknown. The store is the durable form of the transcript (ADR-008);
    /// `run_task` continues from whatever it returns.
    ///
    /// Blocking: reads SQLite synchronously, so an event-loop frontend should
    /// call it off its render thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Session`] when the session store cannot be
    /// read.
    pub fn load_session(&self, id: &SessionId) -> Result<Option<Session>, EngineError> {
        match self.agent.load_session(id) {
            Ok(session) => Ok(session),
            Err(error) => Err(EngineError::Session(error)),
        }
    }

    /// Lists the ids of every persisted session, oldest creation order.
    ///
    /// Blocking (SQLite); same call-off-thread guidance as [`Self::load_session`].
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Session`] when the session store cannot be
    /// read.
    pub fn session_ids(&self) -> Result<Vec<SessionId>, EngineError> {
        self.agent.session_ids().map_err(EngineError::Session)
    }
}
