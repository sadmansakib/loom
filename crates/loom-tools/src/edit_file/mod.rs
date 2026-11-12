//! The `edit_file` tool: exact-match replacement in workspace files.
//!
//! Mutating (architecture §10). Each match must be unique: an ambiguous
//! edit is refused as invalid input rather than guessing which
//! occurrence the model meant. A call is either the classic single edit
//! (`old_string`/`new_string` against `path`) or an atomic batch
//! (`edits: [{old_string, new_string, path?}]`, applied in order — the
//! optional per-edit `path` defaults to the top-level one). Batch edits
//! apply against the evolving text, and nothing is written unless every
//! edit matches exactly once. The result echoes each edited region
//! (±[`ToolBudgets::edit_echo_context_lines`] numbered lines, final file
//! state), so the model never re-reads to confirm what it just wrote.

mod echo;
mod matching;
mod tool;

#[cfg(test)]
mod tests;

pub use tool::EditFileTool;

#[cfg(test)]
use loom_kernel::tool::{ToolContext, ToolError, ToolOutput};
#[cfg(test)]
use matching::MAX_EDIT_FILE_BYTES;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use tool::EditInput;
