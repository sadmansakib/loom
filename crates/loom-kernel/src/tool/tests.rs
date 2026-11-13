use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::paths::is_ancestor;
use super::*;
use crate::artifact::ArtifactRef;
use crate::id::ArtifactId;
use crate::session::TodoStatus;

/// Minimal read-only tool that echoes its input back as JSON —
/// exercises the whole seam without filesystem effects.
struct EchoTool {
    spec: ToolSpec,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "echo".into(),
                description: "Echoes its input.".into(),
                parameters: json!({"type": "object"}),
            },
        }
    }
}

#[test]
fn equivalent_path_spellings_overlap() {
    // Review E2: `./a` and `a` name the same file but used to
    // compare unequal, so one batch could touch it concurrently
    // (V13 scheduling) and a cache entry survived its own write
    // (V14) — the raw strings differ, the paths do not.
    assert!(paths_overlap("./a", "a"));
    assert!(paths_overlap("a//b", "a/b"));
    assert!(paths_overlap("src/", "src"));
    assert!(paths_overlap("././src/a.rs", "src/a.rs"));
    assert!(paths_overlap(".", "src/a.rs"));
    assert!(paths_overlap("src/./a.rs", "src/a.rs"));
}

#[test]
fn distinct_paths_still_do_not_overlap() {
    assert!(!paths_overlap("./src/a.rs", "src/b.rs"));
    assert!(!paths_overlap("src/a.rs", "srca.rs"));
    assert!(!paths_overlap("./a/b", "a/c"));
}

#[test]
fn the_root_spelling_matches_everything_it_should() {
    // `is_ancestor` sees already-normalized strings (paths_overlap
    // normalizes both sides first), so the root spellings are "."
    // and "" — while equivalent directory spellings still overlap
    // through paths_overlap.
    assert!(is_ancestor(".", "a.rs"));
    assert!(paths_overlap("./src", "src/a.rs"));
    assert!(!is_ancestor("a", "a"));
}

#[test]
fn before_images_round_trip_and_default_for_old_transcripts() {
    // Legacy effects JSON (recorded before tier-2 capture) has no
    // before_images key; serde's default keeps those transcripts
    // loadable rather than versioning the event schema.
    let legacy = json!({
        "files_written": ["a.rs"],
        "files_deleted": [],
        "commands_run": [],
        "repairs": [],
    });
    let effects: ToolEffects = serde_json::from_value(legacy).expect("legacy effects parse");
    assert_eq!(effects.before_images, Vec::new());

    // A captured image and an absent-file image both survive a
    // round trip verbatim — the absent one is a positive claim
    // ("did not exist"), not a missing entry.
    let reference = ArtifactRef::new(ArtifactId::new(), "pre_image", 12, "deadbeefdeadbeef");
    let effects = ToolEffects::none()
        .with_file_written("a.rs")
        .with_before_image(BeforeImage::captured("a.rs", reference))
        .with_before_image(BeforeImage::absent("b.txt"));
    let round: ToolEffects =
        serde_json::from_value(serde_json::to_value(&effects).expect("serialize"))
            .expect("deserialize");
    assert_eq!(round.before_images, effects.before_images);
    assert_eq!(
        round.before_images[1],
        BeforeImage {
            path: "b.txt".to_owned(),
            content: None,
        }
    );

    // Pre-images are restore metadata, not side effects: they never
    // make is_empty false (mirrors `repairs`).
    assert!(
        ToolEffects::none()
            .with_before_image(BeforeImage::absent("b.txt"))
            .is_empty()
    );
}

impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let content =
                serde_json::to_string(&input).map_err(|e| ToolError::Execution(e.to_string()))?;
            Ok(ToolOutput::new(content))
        })
    }
}

#[test]
fn tool_spec_round_trips() {
    let spec = ToolSpec {
        name: "read_file".into(),
        description: "Read a file's contents.".into(),
        parameters: json!({
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

#[test]
fn tool_trait_is_object_safe_and_callable() {
    // The engine holds tools as `Box<dyn Tool>`; calling through the
    // trait object must work.
    let tool: Box<dyn Tool> = Box::new(EchoTool::new());
    assert!(tool.is_read_only());
    assert_eq!(tool.spec().name, "echo");

    let ctx = ToolContext::new(
        PathBuf::from("/tmp/workspace"),
        CancellationToken::new(),
        Duration::from_secs(30),
    );
    let output = futures::executor::block_on(tool.call(json!({"a": 1}), &ctx)).unwrap();
    assert_eq!(output.content, r#"{"a":1}"#);
    assert!(!output.truncated);
}

#[test]
fn tool_context_wires_cancellation() {
    let cancellation = CancellationToken::new();
    let ctx = ToolContext::new(
        PathBuf::from("/w"),
        cancellation.clone(),
        Duration::from_secs(5),
    );
    assert!(!ctx.cancellation.is_cancelled());
    cancellation.cancel();
    assert!(ctx.cancellation.is_cancelled());
    assert_eq!(ctx.timeout, Duration::from_secs(5));
}

#[test]
fn tool_output_tracks_truncation() {
    let full = ToolOutput::new("body");
    assert_eq!(full.content, "body");
    assert!(!full.truncated);

    let cut = ToolOutput::new("body…").with_truncated();
    assert!(cut.truncated);
}

#[test]
fn tool_errors_distinguish_input_from_execution() {
    let bad = ToolError::InvalidInput("path must be relative".into());
    let boom = ToolError::Execution("permission denied".into());
    assert_eq!(bad.to_string(), "invalid input: path must be relative");
    assert_eq!(boom.to_string(), "execution failed: permission denied");
    assert_ne!(bad, boom);
}

#[test]
fn registry_registers_looks_up_and_orders_specs() {
    let mut registry = ToolRegistry::new();
    assert!(registry.is_empty());

    registry
        .register(Box::new(EchoTool::new()))
        .expect("first registration succeeds");
    registry
        .register(Box::new(SecondTool::new()))
        .expect("second registration succeeds");

    assert_eq!(registry.len(), 2);
    assert!(registry.get("echo").is_some());
    assert!(registry.get("nope").is_none());

    // Registration order is the spec order the model sees.
    let names: Vec<_> = registry.specs().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["echo", "second"]);
}

#[test]
fn registry_rejects_duplicate_names() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool::new())).unwrap();
    let err = registry.register(Box::new(EchoTool::new())).unwrap_err();
    assert_eq!(
        err,
        ToolRegistryError::DuplicateName {
            name: "echo".into()
        }
    );
    // The first registration is untouched.
    assert_eq!(registry.len(), 1);
}

/// A context with a todo list attached, plus the handle to read it
/// back after the call — the shape the agent loop uses.
fn todo_ctx() -> (ToolContext, Arc<Mutex<crate::session::TodoList>>) {
    let todos = Arc::new(Mutex::new(crate::session::TodoList::default()));
    let ctx = ToolContext::new(
        PathBuf::from("/tmp/workspace"),
        CancellationToken::new(),
        Duration::from_secs(30),
    )
    .with_todos(Some(todos.clone()));
    (ctx, todos)
}

fn call_todo(input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let tool: Box<dyn Tool> = Box::new(TodoTool::new());
    futures::executor::block_on(tool.call(input, ctx))
}

#[test]
fn todo_add_assigns_stable_ids_and_confirms_terse() {
    let (ctx, todos) = todo_ctx();
    let first =
        call_todo(json!({"op": "add", "text": "write the test"}), &ctx).expect("add succeeds");
    assert_eq!(first.content, "added 1");
    let second =
        call_todo(json!({"op": "add", "text": "fix the bug"}), &ctx).expect("add succeeds");
    assert_eq!(second.content, "added 2");
    let list = todos.lock().unwrap();
    assert_eq!(list.items.len(), 2);
    assert_eq!(list.items[0].id, "1");
    assert_eq!(list.items[1].text, "fix the bug");
}

#[test]
fn todo_create_replaces_and_read_renders_the_list() {
    let (ctx, todos) = todo_ctx();
    call_todo(json!({"op": "add", "text": "stale step"}), &ctx).unwrap();
    let created =
        call_todo(json!({"op": "create", "items": ["one", "two"]}), &ctx).expect("create succeeds");
    assert_eq!(created.content, "created list with 2 items");

    let read = call_todo(json!({"op": "read"}), &ctx).expect("read succeeds");
    assert_eq!(
        read.content, "## todo (kernel)\n- [ ] 2: one\n- [ ] 3: two",
        "read returns the rendered block; ids never restart"
    );
    assert!(todos.lock().unwrap().items[0].id != "1");
}

#[test]
fn todo_mark_updates_status_and_rejects_unknown_ids() {
    let (ctx, todos) = todo_ctx();
    call_todo(json!({"op": "add", "text": "step"}), &ctx).unwrap();
    for status in ["in_progress", "done", "cancelled", "pending"] {
        let marked = call_todo(json!({"op": "mark", "id": "1", "status": status}), &ctx)
            .expect("mark succeeds");
        assert_eq!(marked.content, format!("marked 1 as {status}"));
    }
    assert_eq!(todos.lock().unwrap().items[0].status, TodoStatus::Pending);

    let err = call_todo(json!({"op": "mark", "id": "9", "status": "done"}), &ctx)
        .expect_err("unknown id");
    assert!(err.to_string().contains("unknown todo id"), "{err:?}");
    // A rejected mark changed nothing.
    assert_eq!(todos.lock().unwrap().revision, 5);
}

#[test]
fn todo_rejects_a_bad_op_and_missing_fields() {
    let (ctx, _todos) = todo_ctx();
    for bad in [
        json!({"op": "delete"}),
        json!({"op": "add"}),
        json!({"op": "add", "text": "   "}),
        json!({"op": "mark", "id": "1"}),
        json!({"op": "mark", "id": "1", "status": "finished"}),
        json!({"text": "no op"}),
        json!({"op": "read", "unexpected": true}),
    ] {
        let err = call_todo(bad, &ctx).expect_err("input must be rejected");
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
    }
}

#[test]
fn todo_without_attached_state_fails_execution() {
    let ctx = ToolContext::new(
        PathBuf::from("/tmp/workspace"),
        CancellationToken::new(),
        Duration::from_secs(30),
    );
    let err = call_todo(json!({"op": "read"}), &ctx).expect_err("no todo state attached");
    assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
}

#[test]
fn todo_declares_one_virtual_write_cell_for_mutating_ops() {
    let tool = TodoTool::new();
    assert!(!tool.is_read_only());
    // Mutating ops serialize against each other via one shared path.
    for op in ["create", "add", "mark"] {
        let touched = tool.touched_paths(&json!({"op": op})).expect("declared");
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].access, PathAccess::Write);
    }
    // Read (and unparseable input) touches nothing.
    assert_eq!(tool.touched_paths(&json!({"op": "read"})), Some(Vec::new()));
    assert_eq!(tool.touched_paths(&json!({})), Some(Vec::new()));
}

#[test]
fn todo_spec_schema_matches_the_parser() {
    // The schema is hand-written (the kernel carries no schemars);
    // pin the shape the parser enforces so the two cannot drift.
    let tool = TodoTool::new();
    let parameters = tool.spec().parameters.clone();
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(parameters["required"], json!(["op"]));
    assert_eq!(
        parameters["properties"]["op"]["enum"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    assert_eq!(
        parameters["properties"]["status"]["enum"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    // Every advertised op parses; every op's fields exist in the schema.
    for field in ["op", "text", "items", "id", "status"] {
        assert!(parameters["properties"][field].is_object(), "{field}");
    }
}

/// Second distinct tool so ordering tests are meaningful.
struct SecondTool {
    spec: ToolSpec,
}

impl SecondTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: "second".into(),
                description: "Another tool.".into(),
                parameters: json!({"type": "object"}),
            },
        }
    }
}

impl Tool for SecondTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        _input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::new("ok")) })
    }
}
