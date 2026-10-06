//! The `edit_file` tool: exact-match replacement in one workspace file.
//!
//! Mutating (architecture §10). The match must be unique: an ambiguous
//! edit is refused as invalid input rather than guessing which
//! occurrence the model meant.

use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::OnceLock;

use crate::confinement::resolve_within;

/// Edits larger than this are refused: the whole file is read into memory and
/// rewritten, so an unbounded read is a memory spike (a large generated file
/// is the realistic case). 8 MiB covers source files with room to spare.
const MAX_EDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Replaces one unique exact-string match in an existing UTF-8 file,
/// confined to the workspace root.
#[derive(Debug, Default)]
pub struct EditFileTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
}

impl Tool for EditFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "edit_file".to_owned(),
            description: "Replaces an exact string in one workspace file. `old_string` \
                          must appear exactly once — include enough surrounding text to \
                          make it unique. Use write_file to create a file."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace; must occur exactly once."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: EditInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let display = input.path;
            let path = resolve_within(&ctx.workspace_root, &display)?;
            let old = input.old_string;
            let new = input.new_string;
            tokio::task::spawn_blocking(move || apply_edit(&path, &old, &new))
                .await
                .map_err(|err| ToolError::Execution(format!("edit task failed: {err}")))?
                .map(|detail| ToolOutput::new(format!("edited {display}: {detail}")))
        })
    }
}

/// Reads, matches exactly once, replaces, writes back. Returns a short
/// human/model-readable detail on success.
fn apply_edit(path: &Path, old: &str, new: &str) -> Result<String, ToolError> {
    if old.is_empty() {
        return Err(ToolError::InvalidInput(
            "old_string must not be empty".to_owned(),
        ));
    }
    // Unlike the read tools, a missing file here is an execution error, not an
    // informational "does not exist": a write path that targets nothing is a
    // model mistake (read first, or use write_file), and the loop's failure
    // ledger should refuse a blind repeat (ADR-020).
    let size = std::fs::metadata(path)
        .map_err(|err| ToolError::Execution(format!("cannot read \"{}\": {err}", path.display())))?
        .len();
    if size > MAX_EDIT_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "file is too large to edit ({size} bytes; limit {MAX_EDIT_FILE_BYTES} bytes)"
        )));
    }
    let bytes = std::fs::read(path).map_err(|err| {
        ToolError::Execution(format!("cannot read \"{}\": {err}", path.display()))
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution(String::from("file is not valid UTF-8")))?;
    match text.matches(old).count() {
        0 => Err(ToolError::InvalidInput(String::from(
            "old_string not found in file",
        ))),
        1 => {
            let updated = text.replacen(old, new, 1);
            std::fs::write(path, updated)
                .map_err(|err| ToolError::Execution(format!("cannot write: {err}")))?;
            Ok(String::from("replaced 1 occurrence"))
        }
        n => Err(ToolError::InvalidInput(format!(
            "old_string appears {n} times; include more surrounding text to make it unique"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn ctx(root: &Path) -> ToolContext {
        ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
    }

    async fn edit(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        EditFileTool.call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn replaces_unique_occurrence() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let output = edit(
            &root,
            json!({"path": "f.rs", "old_string": "fn add() {}", "new_string": "fn add() { 1 }"}),
        )
        .await
        .expect("ok");
        assert_eq!(output.content, "edited f.rs: replaced 1 occurrence");
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.rs")).expect("read"),
            "fn add() { 1 }\n"
        );
    }

    #[tokio::test]
    async fn not_found_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"fn add() {}\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "missing", "new_string": "x"}),
        )
        .await
        .expect_err("not found");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("not found")));
    }

    #[tokio::test]
    async fn ambiguous_match_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"a\na\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("ambiguous");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("2 times")));
    }

    #[tokio::test]
    async fn empty_old_string_is_invalid_input() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.rs"), b"a\n").expect("seed");
        let err = edit(
            &root,
            json!({"path": "f.rs", "old_string": "", "new_string": "b"}),
        )
        .await
        .expect_err("empty");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("must not be empty")));
    }

    #[tokio::test]
    async fn escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = edit(
            &root,
            json!({"path": "../f.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[tokio::test]
    async fn missing_file_is_execution_error() {
        let (_guard, root) = temp_root();
        let err = edit(
            &root,
            json!({"path": "gone.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("missing");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("gone.rs")));
    }

    #[tokio::test]
    async fn oversized_file_is_execution_error() {
        let (guard, root) = temp_root();
        let path = guard.path().join("big.rs");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(MAX_EDIT_FILE_BYTES + 1).expect("set length");
        let err = edit(
            &root,
            json!({"path": "big.rs", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("too large");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("too large")));
    }

    #[tokio::test]
    async fn invalid_utf8_is_execution_error() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("bin"), [0x61, 0xff]).expect("seed");
        let err = edit(
            &root,
            json!({"path": "bin", "old_string": "a", "new_string": "b"}),
        )
        .await
        .expect_err("utf8");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("UTF-8")));
    }
}
