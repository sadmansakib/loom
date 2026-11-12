//! Tool definitions and the tool-execution seam.
//!
//! This module holds the **definitions** a tool contributes and the seam
//! the engine uses to invoke it — no execution policy. Scheduling,
//! concurrency, ordering, timeouts-as-status, and cancellation-as-status
//! (architecture §6.5/§6.6/§10) live in the tool engine, not here and not
//! in tool implementations.
//!
//! The split of responsibility per §9 of the architecture:
//!
//! - [`ToolSpec`] — the model-facing description carried in
//!   [`ModelRequest`](crate::model::ModelRequest).
//! - [`Tool`] — the implementation seam: tools parse `input` into their
//!   own typed struct (ADR-005); a parse or semantic failure is
//!   [`ToolError::InvalidInput`]. Cancellable, workspace-bound.
//! - [`ToolContext`] — per-call inputs (workspace root, cancellation
//!   token, timeout budget).
//! - [`ToolOutput`]/[`ToolError`] — what a call returns; the engine maps
//!   these onto transcript `ToolResult`s.
//! - [`ToolEffects`] — what a call changed outside Loom (files, commands),
//!   recorded at execution; never declared by the model (ADR-012).
//! - [`ToolRegistry`] — name-keyed set of tools the engine dispatches
//!   against.

mod context;
mod effects;
mod output;
mod paths;
mod registry;
mod spec;
mod todo;

pub use context::{MAX_RECENT_CHANGES, Tool, ToolContext};
pub use effects::{AutofixEffect, BeforeImage, CommandEffect, ToolEffects};
pub use output::{ToolError, ToolNotice, ToolOutput};
pub use paths::{PathAccess, PathTouch};
pub use registry::{ToolRegistry, ToolRegistryError};
pub use spec::ToolSpec;
pub use todo::TodoTool;

pub(crate) use paths::paths_overlap;

#[cfg(test)]
mod tests;
