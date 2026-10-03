//! Tool definitions and the tool-execution seam.
//!
//! Grown incrementally: this file currently defines only [`ToolSpec`] —
//! the model-facing description of a tool that
//! [`ModelRequest`](crate::model::ModelRequest) carries.
//! The `Tool` trait, `ToolContext`, `ToolError`, and `ToolRegistry`
//! (definitions only, no execution policy — per the architecture's module
//! table) land with the tool-system step.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The model-facing definition of one tool (its name, what it does, and
/// the arguments it accepts).
///
/// This is what a provider adapter renders into its wire tool format
/// (e.g. OpenAI `function` definitions); it contains no execution
/// state and no provider details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model uses to invoke the tool. Stable per task —
    /// renaming tools mid-task invalidates the cached prompt prefix.
    pub name: String,
    /// One or two sentences telling the model when to use the tool.
    pub description: String,
    /// JSON Schema (an `object`-typed schema) describing the tool's
    /// arguments. Kept as an opaque [`Value`] so core never depends on a
    /// JSON-Schema library; tools own their schema.
    pub parameters: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_spec_round_trips() {
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "Read a file's contents.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }
}
