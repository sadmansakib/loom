use crate::id::SessionId;

/// A durable-storage failure.
///
/// Transport-thin: the kernel does not care *how* storage failed, only that
/// it did. The message is for the operator, not for control flow.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The storage backend failed (I/O, SQL, a poisoned lock).
    #[error("storage backend error: {0}")]
    Backend(String),
    /// A record read back did not match the shape the kernel writes
    /// (deserialisation failure, an index gap, an unknown session).
    #[error("corrupt stored state: {0}")]
    Corrupt(String),
    /// A live single-writer lease (ADR-008) still covers the session, so
    /// the caller must not race the writer that holds it.
    #[error("live lease on {session} (owner {owner}); refusing to race it")]
    Leased {
        /// The session the live lease covers.
        session: SessionId,
        /// The writer that holds the lease and must finish first.
        owner: String,
    },
}
