//! The `list_dir` tool: sorted, one-level directory listings.

use futures::future::BoxFuture;
use loom_core::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::OnceLock;

use crate::confinement::resolve_within;

/// Lists one directory level: entries sorted lexicographically, `/`
/// suffixed for directories. Confined to the workspace root.
#[derive(Debug, Default)]
pub struct ListDirTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    path: String,
}

impl Tool for ListDirTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "list_dir".to_owned(),
            description: "Lists one directory level (non-recursive). Entries are \
                          sorted; directories carry a trailing `/`."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path, relative to the workspace root."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: ListInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let path = resolve_within(&ctx.workspace_root, &input.path)?;
            let display = input.path;
            let entries = tokio::task::spawn_blocking(move || list_entries(&path))
                .await
                .map_err(|err| ToolError::Execution(format!("list task failed: {err}")))?
                .map_err(|err| ToolError::Execution(format!("cannot list \"{display}\": {err}")))?;
            Ok(ToolOutput {
                content: entries.join("\n"),
                truncated: false,
            })
        })
    }
}

/// Collects and sorts one directory level; `/` for directories.
/// Sorting is by byte value, so the result is deterministic.
fn list_entries(path: &std::path::Path) -> Result<Vec<String>, String> {
    let dir = std::fs::read_dir(path).map_err(|err| err.to_string())?;
    let mut entries = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|err| err.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // `file_type` does not follow symlinks: a symlink lists as a
        // plain name (where it points is not this tool's business).
        let is_dir = entry.file_type().map_err(|err| err.to_string())?.is_dir();
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::tool::Tool;
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

    async fn list(root: &Path, args: Value) -> Result<ToolOutput, ToolError> {
        ListDirTool.call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn lists_sorted_with_dir_suffix() {
        let (guard, root) = temp_root();
        let base = guard.path();
        std::fs::create_dir(base.join("zebra")).expect("mkdir");
        std::fs::create_dir(base.join("alpha")).expect("mkdir");
        std::fs::write(base.join("m.txt"), b"m").expect("write");
        let output = list(&root, json!({"path": "."})).await.expect("ok");
        assert_eq!(output.content, "alpha/\nm.txt\nzebra/");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn missing_dir_is_execution_error() {
        let (_guard, root) = temp_root();
        let err = list(&root, json!({"path": "nope"}))
            .await
            .expect_err("missing");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("nope")));
    }

    #[tokio::test]
    async fn file_argument_is_execution_error() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.txt"), b"x").expect("write");
        let err = list(&root, json!({"path": "f.txt"}))
            .await
            .expect_err("not a dir");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("f.txt")));
    }

    #[tokio::test]
    async fn escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = list(&root, json!({"path": ".."}))
            .await
            .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }
}
