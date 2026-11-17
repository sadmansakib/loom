//! The task event stream: best-effort observability protocol.
//!
//! Events are serialisable protocol objects for clients, NDJSON traces,
//! replay of *what was observed*, and future evaluation. They are **not**
//! the source of truth for session state — the transcript is (ADR-004);
//! rebuilding a session never requires events.
//!
//! Delivery is best-effort and non-fatal: observability must not silently
//! alter task semantics. [`EventSink::emit`] returns `()`, so a broken
//! consumer (closed channel, failed NDJSON writer, full disk) can never
//! turn a successful task into a failed one or vice versa.
//! [`EventEnvelope::sequence`] numbers are unique and strictly increasing
//! per sink, and the sink holds its send-side lock across numbering and
//! send — so **arrival order equals sequence order** and streaming
//! consumers can write events as they arrive. Loss is surfaced by the
//! sink's drop count ([`EventSink::dropped`], reported at task end):
//! with the unbounded channel, a dropped event means the receiver is
//! closed, so every later event drops too and a trace simply ends early
//! rather than showing a mid-stream gap.
//!
//! Streaming deltas appear here as [`EventKind::TextDelta`] /
//! [`EventKind::ReasoningDelta`], mirroring the model stream. The same
//! provisional-until-completed rule applies: consumers must discard every
//! delta received since the preceding `ModelRequestStarted` / `ModelRetry`
//! when a [`EventKind::ModelRetry`] fires; only
//! [`EventKind::ModelCompleted`] is authoritative.

mod persist;
mod sink;
mod status;
mod strict;
#[cfg(test)]
mod tests;
mod wire;

pub use persist::PersistingEventSink;
pub use sink::{ChannelEventSink, EventSink, FanoutSink};
pub use status::{StatusRecorder, StatusSnapshot, TaskPhase};
pub use strict::StrictSink;
pub(crate) use wire::head_bytes;
pub use wire::{
    ARGUMENTS_EVENT_HEAD, CONTENT_EVENT_HEAD, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind,
    RetryPolicySnapshot, TurnBudgetReason, text_head,
};
