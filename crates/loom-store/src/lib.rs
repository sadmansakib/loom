//! Durable state substrate for Loom: SQLite implementations of the kernel's
//! [`SessionStore`](loom_kernel::store::SessionStore) and
//! [`ArtifactStore`](loom_kernel::store::ArtifactStore) seams (ADR-008).
//!
//! The kernel defines the traits and a dependency-free in-memory
//! implementation; this crate is the durable one. Frontends never link it
//! directly — [`loom-engine`](https://docs.rs/loom-engine) composes a
//! [`SqliteStore`] and hands it to the agent as a `Box<dyn SessionStore>`,
//! keeping the HTTP/keychain/SQLite dependencies below the facade
//! (ADR-021).
//!
//! # Shape
//!
//! One database file holds every session. The transcript is authoritative
//! (ADR-004): each item's serde form is stored verbatim in `items.body_json`
//! and is the row the loader reconstructs sessions from. Schema v1 ships
//! without FTS — `items.kind` is stored so a future search index (Iteration
//! 3, ADR-010) can be built without re-reading rows. Schema v2 adds the
//! `events` table: the observed event stream persisted alongside the
//! transcript (one envelope's serde form per row, insertion-ordered), so
//! `loom stats` can attribute savings per mechanism from a stored session.
//!
//! # Ordering
//!
//! The agent loop writes a turn with
//! [`SessionStore::append_turn`](loom_kernel::store::SessionStore::append_turn)
//! *before* mutating the in-memory transcript (ADR-012 decision 4). A SQLite
//! failure therefore aborts with
//! [`TaskError::Persistence`](loom_kernel::agent::TaskError::Persistence)
//! leaving the transcript — and the file — untouched. The old ordering could
//! record a side effect that the transcript never captured.
//!
//! # Concurrency
//!
//! One connection guarded by a mutex: writes are serialized in-process, WAL
//! lets the credential store share the same file, and a per-session lease
//! ([`SqliteStore::acquire_lease`]) guards against a second process writing
//! the same session.

mod schema;
mod store;

pub use store::Lease;
pub use store::SqliteStore;
