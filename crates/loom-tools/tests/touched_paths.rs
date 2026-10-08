//! Dependency declarations: each tool reports the paths it reads or writes
//! from its raw arguments, before execution (ADR-018).
//!
//! `None` means "unknown" and the scheduler treats it as a wildcard; an empty
//! `Some` means the call touches no workspace path. The kernel's wave
//! scheduler is only as good as these declarations, so they are asserted
//! here against the shipped tool structs.

use loom_kernel::tool::{PathTouch, Tool};
use serde_json::{Value, json};

fn touches(tool: &dyn Tool, input: Value) -> Option<Vec<PathTouch>> {
    tool.touched_paths(&input)
}

fn read(path: &str) -> PathTouch {
    PathTouch::read(path)
}

fn write(path: &str) -> PathTouch {
    PathTouch::write(path)
}

#[test]
fn read_only_tools_declare_their_read_path() {
    assert_eq!(
        touches(&loom_tools::ReadFileTool, json!({"path": "src/a.rs"})),
        Some(vec![read("src/a.rs")])
    );
    assert_eq!(
        touches(&loom_tools::ListDirTool, json!({"path": "src"})),
        Some(vec![read("src")])
    );
    // A search with no `path` reads the workspace root.
    assert_eq!(
        touches(&loom_tools::SearchTool, json!({"pattern": "fn"})),
        Some(vec![read(".")])
    );
    assert_eq!(
        touches(
            &loom_tools::SearchTool,
            json!({"pattern": "fn", "path": "src"})
        ),
        Some(vec![read("src")])
    );
}

#[test]
fn mutating_tools_declare_their_write_path() {
    assert_eq!(
        touches(
            &loom_tools::WriteFileTool,
            json!({"path": "src/a.rs", "content": "x"})
        ),
        Some(vec![write("src/a.rs")])
    );
    assert_eq!(
        touches(
            &loom_tools::EditFileTool,
            json!({"path": "src/a.rs", "old_string": "a", "new_string": "b"})
        ),
        Some(vec![write("src/a.rs")])
    );
}

#[test]
fn artifact_read_touches_no_workspace_path() {
    assert_eq!(
        touches(&loom_tools::ArtifactReadTool, json!({"id": "abc"})),
        Some(Vec::new())
    );
}

#[test]
fn run_command_and_verify_are_wildcards() {
    // Their effects are not knowable from the arguments, so they serialize.
    assert_eq!(
        touches(
            &loom_tools::RunCommandTool::new(),
            json!({"program": "cargo"})
        ),
        None
    );
    assert_eq!(
        touches(&loom_tools::VerifyTool::new(), json!({"scope": "test"})),
        None
    );
}

#[test]
fn unparseable_arguments_declare_nothing() {
    // An invalid call fails without touching anything, so it must not create a
    // false dependency.
    assert_eq!(
        touches(&loom_tools::ReadFileTool, json!({})),
        Some(Vec::new())
    );
    assert_eq!(
        touches(&loom_tools::WriteFileTool, json!({"path": "a"})),
        Some(Vec::new())
    );
}
