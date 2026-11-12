//! Durable state seams: `SessionStore` and `ArtifactStore` (ADR-008, ADR-012).
//!
//! The agent loop commits a turn in memory, but ADR-012 requires the turn to
//! be **durable before it is in memory** (persist-then-commit): a crash after
//! a mutating tool ran must never lose the record of that side effect. The
//! loop therefore plans a turn ([`Session::plan_turn`](crate::session::Session::plan_turn)), hands it to a
//! [`SessionStore`], and only then applies it in memory.
//!
//! These traits live in the neutral kernel; the concrete SQLite implementation
//! lives in `loom-store` so the loop keeps no database dependency. Errors are
//! surfaced as [`StoreError`] and mapped to [`TaskError::Persistence`](crate::agent::TaskError::Persistence)
//! by the loop — a store failure fails the task; an event-sink failure never
//! does (ADR-004, unchanged).

mod artifact;
mod error;
mod event;
mod memory;
mod session;

#[cfg(test)]
mod tests;

pub use artifact::{ArtifactStat, ArtifactStore, StoredArtifact, verify_body_hash};
pub use error::StoreError;
pub use event::{EventStore, SharedEventStore};
pub use memory::{MemoryArtifactStore, MemoryEventStore, MemorySessionStore};
pub use session::{ForkProvenance, Lease, SessionStore, StartCheckpoint};
