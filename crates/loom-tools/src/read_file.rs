//! The `read_file` tool: bounded, workspace-confined, windowed text reads.
//!
//! Output is a *window* of numbered lines with a total-lines header, so the
//! prompt's "read the smallest relevant range" is actionable (ADR-014, V7).
//! The window is bounded by lines (`limit`) and by bytes
//! ([`READ_FILE_HEAD_BYTES`]); both cuts are explicit.

use futures::future::BoxFuture;
use loom_kernel::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::OnceLock;

use crate::confinement::resolve_within;
use crate::window::{self, DEFAULT_LIMIT};

/// Reads one file's UTF-8 text, confined to the workspace root, returning a
/// numbered window of lines with a total-lines header.
#[derive(Debug, Default)]
pub struct ReadFileTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct ReadInput {
    /// File path, relative to the workspace root.
    path: String,
    /// First line to return, 1-based (default 1).
    #[serde(default)]
    #[schemars(range(min = 1))]
    offset: Option<usize>,
    /// Maximum lines to return (default 300).
    #[serde(default)]
    #[schemars(range(min = 1))]
    limit: Option<usize>,
}

impl Tool for ReadFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "read_file".to_owned(),
            description: "Reads one text file from the workspace as numbered lines \
                          with a total-lines header. Read the smallest relevant range: \
                          pass `offset` (first line, 1-based) and `limit` (line count, \
                          default 300). Large ranges are cut with an explicit marker."
                .to_owned(),
            parameters: crate::schema::parameters::<ReadInput>(),
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
            let offset = match input.offset {
                None => 1,
                Some(0) => {
                    return Err(ToolError::InvalidInput(
                        "offset must be at least 1".to_owned(),
                    ));
                }
                Some(n) => n,
            };
            let limit = match input.limit {
                None => DEFAULT_LIMIT,
                Some(0) => {
                    return Err(ToolError::InvalidInput(
                        "limit must be at least 1".to_owned(),
                    ));
                }
                Some(n) => n,
            };
            let path = resolve_within(&ctx.workspace_root, &input.path)?;
            let display = input.path;
            let window =
                tokio::task::spawn_blocking(move || window::read_file_window(&path, offset, limit))
                    .await
                    .map_err(|err| ToolError::Execution(format!("read task failed: {err}")))?
                    .map_err(|err| {
                        ToolError::Execution(format!("cannot read \"{display}\": {err}"))
                    })?;
            // A miss is an answer, not a fault: the caller asked for the
            // file, and "there is no such file" is the honest result (§13).
            let Some(window) = window else {
                return Ok(ToolOutput::new(format!(
                    "file \"{display}\" does not exist"
                )));
            };
            let (content, truncated) = window::render_window(&display, &window, offset);
            let mut output = ToolOutput::new(content);
            if truncated {
                output = output.with_truncated();
            }
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::Tool;
    use serde_json::json;
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
    async fn reads_numbered_lines_with_a_total_header() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("note.txt"), b"alpha\nbeta\n").expect("write");
        let output = read(&root, json!({"path": "note.txt"})).await.expect("ok");
        assert_eq!(output.content, "note.txt: 2 lines\n1\talpha\n2\tbeta\n");
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
    async fn empty_file_says_so() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("empty.txt"), b"").expect("write");
        let output = read(&root, json!({"path": "empty.txt"})).await.expect("ok");
        assert_eq!(output.content, "empty.txt is empty");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn window_returns_the_requested_lines_and_points_at_the_next() {
        let (guard, root) = temp_root();
        let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(guard.path().join("f.txt"), body).expect("write");
        let output = read(&root, json!({"path": "f.txt", "offset": 4, "limit": 3}))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            "f.txt: 10 lines\n4\tline 4\n5\tline 5\n6\tline 6\n\
             ... [4 more lines; read with offset=7]\n"
        );
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn default_limit_bounds_a_long_file() {
        let (guard, root) = temp_root();
        let body: String = (1..=1000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(guard.path().join("f.txt"), body).expect("write");
        let output = read(&root, json!({"path": "f.txt"})).await.expect("ok");
        assert!(output.truncated);
        assert!(output.content.starts_with("f.txt: 1000 lines\n1\tline 1\n"));
        assert!(
            output
                .content
                .contains("[700 more lines; read with offset=301]")
        );
    }

    #[tokio::test]
    async fn offset_past_the_end_is_explained() {
        let (guard, root) = temp_root();
        std::fs::write(guard.path().join("f.txt"), b"one\ntwo\n").expect("write");
        let output = read(&root, json!({"path": "f.txt", "offset": 20}))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            "f.txt: 2 lines\n... [offset 20 is past the last line]"
        );
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn a_zero_offset_or_limit_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = read(&root, json!({"path": "f.txt", "offset": 0}))
            .await
            .expect_err("zero offset");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("offset")));
        let err = read(&root, json!({"path": "f.txt", "limit": 0}))
            .await
            .expect_err("zero limit");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("limit")));
    }

    #[tokio::test]
    async fn byte_budget_cuts_a_few_very_long_lines() {
        let (guard, root) = temp_root();
        let line = format!("{}\n", "x".repeat(1000));
        std::fs::write(guard.path().join("wide.txt"), line.repeat(100)).expect("write");
        let output = read(&root, json!({"path": "wide.txt"})).await.expect("ok");
        assert!(output.truncated);
        assert!(
            output.content.contains("bytes omitted"),
            "the byte cut must be explicit"
        );
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

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<ReadInput>(json!({"path": "src/lib.rs"}));
    }
}
