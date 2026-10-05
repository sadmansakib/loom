//! Agent orchestration: driving the model–tool loop deterministically.
//!
//! The only resident today is the tool engine ([`tools`]) — the single
//! choke point through which every model-requested tool call passes
//! (architecture.md §17). The turn loop itself lands with iteration
//! planning; it will consume [`tools::ToolBatch`] and the model/event
//! seams committed alongside the transcript.

pub mod tools;
