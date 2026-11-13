//! The kernel-owned `todo` tool: the one concrete tool the kernel itself
//! implements, mutating the session's plan list through
//! [`ToolContext::todos`].

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use super::context::{Tool, ToolContext};
use super::output::{ToolError, ToolOutput};
use super::paths::PathTouch;
use super::spec::ToolSpec;
use crate::session::TodoStatus;

/// The virtual workspace-relative cell a mutating `todo` op writes.
///
/// The list lives in kernel session state, not the filesystem. Declaring
/// one shared write path lets the scheduler serialize concurrent `todo`
/// ops in model order — so assigned ids stay deterministic — while file
/// tools never contend with them (the path is not a real file any tool
/// reads or writes).
const TODO_STATE_PATH: &str = ".loom/todo";

/// The kernel-owned `todo` tool (roadmap item 8 phase B): create, add,
/// mark, and read the session's plan list.
///
/// This is the one concrete tool the kernel itself implements: its state
/// is per-session ([`crate::session::TodoList`]), so it must mutate
/// through the [`ToolContext::todos`] handle the agent loop seeds from
/// the session — a seam only the kernel can own. Every other tool stays
/// in `loom-tools` (ADR-001).
///
/// Results are deliberately terse (`added 3: write tests`); the loop,
/// not the tool, re-appends the full rendered list at the transcript
/// tail when a batch changed it, so the model sees the current plan in
/// every later projection without the tool result duplicating it. The
/// `read` op is the exception — it returns the rendered block itself.
///
/// The input schema is hand-written rather than generated (ADR-005
/// addendum 1 uses `schemars`, a dependency the kernel deliberately
/// does not carry); the parser below is the source of truth and the two
/// are pinned by unit test.
#[derive(Debug, Default)]
pub struct TodoTool;

impl TodoTool {
    /// Creates the tool. Stateless: the list arrives per call through
    /// [`ToolContext::todos`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// The `todo` tool's input, parsed with serde (`op` selects the rest).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoInput {
    /// Operation: `"create"`, `"add"`, `"mark"`, or `"read"`.
    op: String,
    /// Item text for `"add"`.
    #[serde(default)]
    text: Option<String>,
    /// Seed texts for `"create"` (a fresh list); defaults to empty.
    #[serde(default)]
    items: Option<Vec<String>>,
    /// Item id for `"mark"`.
    #[serde(default)]
    id: Option<String>,
    /// Target status for `"mark"`: `pending`, `in_progress`, `done`, or
    /// `cancelled`.
    #[serde(default)]
    status: Option<String>,
}

impl Tool for TodoTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "todo".to_owned(),
            description: "Manages the session's kernel-owned todo list: the plan \
                          you are executing. Create the list when a task has \
                          multiple steps, add steps as they are discovered, mark \
                          them in_progress when started and done/cancelled when \
                          settled, and read to see the current list. The current \
                          list is re-shown at the end of the context whenever it \
                          changes; keep it current."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["create", "add", "mark", "read"],
                        "description": "Operation: create a fresh list (optional \
                                        `items` seed texts), add a step (`text`), \
                                        set a step's status (`id` + `status`), or \
                                        read the current list."
                    },
                    "text": {
                        "type": "string",
                        "description": "The step text, for op=add."
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Seed step texts, for op=create. Optional."
                    },
                    "id": {
                        "type": "string",
                        "description": "The step id to update, for op=mark."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done", "cancelled"],
                        "description": "The status to set, for op=mark."
                    }
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        // A mutating op writes one virtual cell, serializing concurrent
        // todo ops in model order; `read` (and anything that will fail
        // validation) touches nothing.
        let op = input.get("op").and_then(Value::as_str);
        match op {
            Some("create" | "add" | "mark") => Some(vec![PathTouch::write(TODO_STATE_PATH)]),
            _ => Some(Vec::new()),
        }
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: TodoInput = serde_json::from_value(input)
                .map_err(|error| ToolError::InvalidInput(format!("invalid arguments: {error}")))?;
            let mut todos = ctx
                .todos
                .as_ref()
                .ok_or_else(|| {
                    ToolError::Execution("todo state is not attached to this call".to_owned())
                })?
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match input.op.as_str() {
                "create" => {
                    let items = input.items.unwrap_or_default();
                    let count = items.len();
                    todos.replace(items);
                    Ok(ToolOutput::new(format!(
                        "created list with {count} item{}",
                        if count == 1 { "" } else { "s" }
                    )))
                }
                "add" => {
                    let text = input.text.filter(|text| !text.trim().is_empty());
                    let Some(text) = text else {
                        return Err(ToolError::InvalidInput(
                            "op=add requires a non-empty `text`".to_owned(),
                        ));
                    };
                    let id = todos.add(text);
                    Ok(ToolOutput::new(format!("added {id}")))
                }
                "mark" => {
                    let Some(id) = input.id else {
                        return Err(ToolError::InvalidInput(
                            "op=mark requires an `id`".to_owned(),
                        ));
                    };
                    let Some(wire) = input.status else {
                        return Err(ToolError::InvalidInput(
                            "op=mark requires a `status` of pending, in_progress, done, \
                             or cancelled"
                                .to_owned(),
                        ));
                    };
                    let Some(status) = TodoStatus::parse(&wire) else {
                        return Err(ToolError::InvalidInput(format!(
                            "unknown status {wire:?}; use pending, in_progress, done, \
                             or cancelled"
                        )));
                    };
                    if !todos.mark(&id, status) {
                        return Err(ToolError::InvalidInput(format!(
                            "unknown todo id {id:?}; use `read` to see the list"
                        )));
                    }
                    Ok(ToolOutput::new(format!("marked {id} as {wire}")))
                }
                "read" => Ok(ToolOutput::new(if todos.is_empty() {
                    "(todo list is empty)".to_owned()
                } else {
                    todos.render()
                })),
                other => Err(ToolError::InvalidInput(format!(
                    "unknown op {other:?}; use create, add, mark, or read"
                ))),
            }
        })
    }
}
