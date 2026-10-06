//! The `read_file` tool: bounded, workspace-confined text reads.

use futures::future::BoxFuture;
use loom_kernel::agent::tools::MAX_TOOL_OUTPUT_BYTES;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use crate::confinement::resolve_within;

/// Reads one file's UTF-8 text, confined to the workspace root and
/// capped at [`MAX_TOOL_OUTPUT_BYTES`] (the engine bounds output a
/// second time; the cap here keeps the read itself bounded).
#[derive(Debug, Default)]
pub struct ReadFileTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
}

impl Tool for ReadFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "read_file".to_owned(),
            description: "Reads one text file from the workspace. Returns the file's \
                          full content, or the first 64 KiB with `truncated: true` \
                          when larger."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root."
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
            let input: ReadInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let path = resolve_within(&ctx.workspace_root, &input.path)?;
            let display = input.path;
            let bytes = tokio::task::spawn_blocking(move || read_capped(&path))
                .await
                .map_err(|err| ToolError::Execution(format!("read task failed: {err}")))?
                .map_err(|err| ToolError::Execution(format!("cannot read \"{display}\": {err}")))?;
            // A miss is an answer, not a fault: the caller asked for the
            // file, and "there is no such file" is the honest result (§13).
            let Some(bytes) = bytes else {
                return Ok(ToolOutput::new(format!(
                    "file \"{display}\" does not exist"
                )));
            };

            let truncated = bytes.len() > MAX_TOOL_OUTPUT_BYTES;
            let content =
                crate::text::decode_capped(bytes, MAX_TOOL_OUTPUT_BYTES).ok_or_else(|| {
                    ToolError::Execution(format!("cannot read \"{display}\": not valid UTF-8"))
                })?;
            Ok(ToolOutput { content, truncated })
        })
    }
}

/// Reads at most `MAX + 1` bytes (the extra byte only signals
/// truncation). Metadata is checked first so a FIFO or socket with no
/// writer cannot pin a blocking-pool thread indefinitely. A missing path
/// yields `Ok(None)`: "there is no such file" is an answer, not a fault.
fn read_capped(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_TOOL_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn ctx(root: &std::path::Path) -> ToolContext {
        ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
    }

    async fn read(root: &std::path::Path, args: Value) -> Result<ToolOutput, ToolError> {
        ReadFileTool.call(args, &ctx(root)).await
    }

    #[tokio::test]
    async fn reads_file_content() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("note.txt"), b"hello loom").expect("write");
        let output = read(&root, json!({"path": "note.txt"})).await.expect("ok");
        assert_eq!(output.content, "hello loom");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn missing_file_is_informational() {
        let (_guard, root) = temp_root();
        let output = read(&root, json!({"path": "gone.txt"})).await.expect("ok");
        assert_eq!(output.content, "file \"gone.txt\" does not exist");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn escape_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = read(&root, json!({"path": "../outside.txt"}))
            .await
            .expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[tokio::test]
    async fn invalid_utf8_is_execution_error() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("bin"), [0x66, 0x6f, 0x6f, 0xff]).expect("write");
        let err = read(&root, json!({"path": "bin"})).await.expect_err("utf8");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("UTF-8")));
    }

    #[tokio::test]
    async fn oversized_file_truncates() {
        let (guard, root) = temp_root();
        // 3-byte chars so the cut lands mid-char and must back off.
        std::fs::write(guard.path().join("big"), "✀".repeat(22_000)).expect("write");
        let output = read(&root, json!({"path": "big"})).await.expect("ok");
        assert!(output.truncated);
        assert_eq!(output.content.len(), MAX_TOOL_OUTPUT_BYTES - 1);
    }

    #[tokio::test]
    async fn missing_path_argument_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = read(&root, json!({})).await.expect_err("missing field");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("arguments")));
    }

    #[tokio::test]
    async fn directory_is_execution_error() {
        let (guard, root) = temp_root();
        std::fs::create_dir(guard.path().join("d")).expect("mkdir");
        let err = read(&root, json!({"path": "d"}))
            .await
            .expect_err("not a file");
        // Same branch a FIFO hits: metadata says "not a regular file"
        // before any blocking open.
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("not a regular file")));
    }
}
