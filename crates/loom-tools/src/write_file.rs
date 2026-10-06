//! The `write_file` tool: create or overwrite one workspace file.
//!
//! Mutating (architecture §10): the engine runs it sequentially. Path
//! handling goes through [`resolve_within`] exactly like the read-only
//! tools, so an escape is an execution-invariant violation, not policy.

use futures::future::BoxFuture;
use loom_kernel::tool::{
    PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;

use crate::confinement::{resolve_within, workspace_relative};

/// Creates or overwrites one UTF-8 text file, confined to the workspace
/// root, creating missing parent directories.
#[derive(Debug, Default)]
pub struct WriteFileTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct WriteInput {
    /// File path, relative to the workspace root.
    path: String,
    /// The complete file content to write.
    content: String,
}

impl Tool for WriteFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "write_file".to_owned(),
            description: "Creates or overwrites one text file in the workspace, creating \
                          parent directories as needed. Overwrites the whole file, so \
                          read it first when it already exists; use edit_file for a \
                          targeted change."
                .to_owned(),
            parameters: crate::schema::parameters::<WriteInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        Some(match serde_json::from_value::<WriteInput>(input.clone()) {
            Ok(parsed) => vec![PathTouch::write(parsed.path)],
            Err(_) => Vec::new(),
        })
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: WriteInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let display = input.path;
            let path = resolve_within(&ctx.workspace_root, &display)?;
            let relative = workspace_relative(&ctx.workspace_root, &path);
            let bytes = input.content.into_bytes();
            let written = bytes.len();
            tokio::task::spawn_blocking(move || write_file(&path, &bytes))
                .await
                .map_err(|err| ToolError::Execution(format!("write task failed: {err}")))?
                .map_err(|err| {
                    ToolError::Execution(format!("cannot write \"{display}\": {err}"))
                })?;
            Ok(
                ToolOutput::new(format!("wrote {written} bytes to {display}"))
                    .with_effects(ToolEffects::none().with_file_written(relative)),
            )
        })
    }
}

/// Creates parents, then writes. Side effects are not assumed atomic
/// (architecture §17 invariant 7).
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use serde_json::json;
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

    async fn write(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        WriteFileTool.call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn creates_file_and_parent_directories() {
        let (guard, root) = temp_root();
        let output = write(
            &root,
            json!({"path": "nested/dir/note.txt", "content": "hello loom"}),
        )
        .await
        .expect("ok");
        assert_eq!(output.content, "wrote 10 bytes to nested/dir/note.txt");
        assert!(!output.truncated);
        assert_eq!(
            output.effects.files_written,
            vec!["nested/dir/note.txt".to_string()]
        );
        assert!(output.effects.files_deleted.is_empty());
        assert!(output.effects.commands_run.is_empty());
        assert_eq!(
            std::fs::read_to_string(guard.path().join("nested/dir/note.txt")).expect("read"),
            "hello loom"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.txt"), b"old").expect("seed");
        write(&root, json!({"path": "f.txt", "content": "new"}))
            .await
            .expect("ok");
        assert_eq!(
            std::fs::read_to_string(guard.path().join("f.txt")).expect("read"),
            "new"
        );
    }

    #[tokio::test]
    async fn writes_unicode_exactly() {
        let (guard, root) = temp_root();
        let text = "héllo — ✀ ✀";
        write(&root, json!({"path": "u.txt", "content": text}))
            .await
            .expect("ok");
        assert_eq!(
            std::fs::read_to_string(guard.path().join("u.txt")).expect("read"),
            text
        );
    }

    #[tokio::test]
    async fn escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = write(&root, json!({"path": "../escape.txt", "content": "x"}))
            .await
            .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<WriteInput>(json!({"path": "note.txt", "content": ""}));
    }

    #[tokio::test]
    async fn missing_content_argument_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = write(&root, json!({"path": "f.txt"}))
            .await
            .expect_err("missing field");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("arguments")));
    }
}
