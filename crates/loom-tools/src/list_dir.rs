//! The `list_dir` tool: sorted, one-level directory listings.

use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
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
            let outcome = tokio::task::spawn_blocking(move || list_entries(&path))
                .await
                .map_err(|err| ToolError::Execution(format!("list task failed: {err}")))?
                .map_err(|err| ToolError::Execution(format!("cannot list \"{display}\": {err}")))?;
            // A miss is an answer, not a fault: the caller asked what is
            // there, and "nothing" is the honest result (§13).
            Ok(match outcome {
                ListOutcome::Entries(entries) => ToolOutput::new(entries.join("\n")),
                ListOutcome::Missing => {
                    ToolOutput::new(format!("path \"{display}\" does not exist"))
                }
                ListOutcome::NotADirectory => {
                    ToolOutput::new(format!("path \"{display}\" is not a directory"))
                }
            })
        })
    }
}

/// What a listing found: real entries, nothing at the path, or a path that is
/// not a directory. The last two are answers the model can act on, not faults.
enum ListOutcome {
    Entries(Vec<String>),
    Missing,
    NotADirectory,
}

/// Collects and sorts one directory level; `/` for directories.
/// Sorting is by byte value, so the result is deterministic. A missing path
/// or a non-directory is reported as such rather than raised as an error.
fn list_entries(path: &std::path::Path) -> Result<ListOutcome, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ListOutcome::Missing);
        }
        Err(err) => return Err(err.to_string()),
    };
    if !metadata.is_dir() {
        return Ok(ListOutcome::NotADirectory);
    }
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
    Ok(ListOutcome::Entries(entries))
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
    async fn missing_dir_is_informational() {
        let (_guard, root) = temp_root();
        let output = list(&root, json!({"path": "nope"})).await.expect("ok");
        assert_eq!(output.content, "path \"nope\" does not exist");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn file_argument_is_informational() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.txt"), b"x").expect("write");
        let output = list(&root, json!({"path": "f.txt"})).await.expect("ok");
        assert_eq!(output.content, "path \"f.txt\" is not a directory");
        assert!(!output.truncated);
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
