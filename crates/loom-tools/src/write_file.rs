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
    /// Workspace-relative file path.
    path: String,
    /// File content to write.
    content: String,
    /// Optional cargo run after the write: check or test.
    #[serde(default)]
    verify: Option<crate::verify::Scope>,
}

impl Tool for WriteFileTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "write_file".to_owned(),
            description: "Creates or overwrites one text file, creating parent \
                          directories. Read first if it exists; use edit_file for a \
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
            let scope = input.verify;
            let bytes = input.content.into_bytes();
            let written = bytes.len();
            let moved_path = path.clone();
            tokio::task::spawn_blocking(move || write_file(&moved_path, &bytes))
                .await
                .map_err(|err| ToolError::Execution(format!("write task failed: {err}")))?
                .map_err(|err| {
                    ToolError::Execution(format!("cannot write \"{display}\": {err}"))
                })?;
            let mut effects = ToolEffects::none().with_file_written(relative.clone());
            let mut content = format!("wrote {written} bytes to {display}");
            let mut truncated = false;
            if let Some(scope) = scope {
                let (rendered, command) = crate::fusion::run_fused(scope, ctx, &relative).await?;
                content = format!("{content}\n\n--- verify ---\n{}", rendered.content);
                truncated = rendered.truncated;
                if let Some(command) = command {
                    effects = effects.with_command(command);
                }
            }
            let mut output = ToolOutput::new(content).with_effects(effects);
            if truncated {
                output = output.with_truncated();
            }
            Ok(output)
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
        crate::schema::agreement::check::<WriteInput>(
            json!({"path": "note.txt", "content": "", "verify": "check"}),
        );
    }

    #[tokio::test]
    async fn missing_content_argument_is_invalid_input() {
        let (_guard, root) = temp_root();
        let err = write(&root, json!({"path": "f.txt"}))
            .await
            .expect_err("missing field");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("arguments")));
    }

    /// A workspace that is a tiny cargo crate, so fused verification has
    /// something real to compile.
    fn crate_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        std::fs::create_dir(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fusion-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
        )
        .expect("Cargo.toml");
        std::fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").expect("lib.rs");
        (dir, root)
    }

    fn crate_ctx(root: &Path) -> ToolContext {
        ToolContext::new(
            root.to_path_buf(),
            CancellationToken::new(),
            Duration::from_secs(120),
        )
    }

    #[tokio::test]
    async fn fused_check_reports_diagnostics_in_the_same_call() {
        let (_guard, root) = crate_root();
        // A syntax error: the fused check must fail while still reporting
        // the write itself as done.
        let output = WriteFileTool
            .call(
                json!({
                    "path": "src/lib.rs",
                    "content": "pub fn one() -> u32 { 1 ",
                    "verify": "check"
                }),
                &crate_ctx(&root),
            )
            .await
            .expect("ok");
        assert!(output.content.starts_with("wrote "), "{}", output.content);
        assert!(
            output.content.contains("--- verify ---"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("unclosed delimiter"),
            "{}",
            output.content
        );
        assert_eq!(output.effects.files_written, vec!["src/lib.rs".to_string()]);
        assert_eq!(output.effects.commands_run.len(), 1);
    }

    #[tokio::test]
    async fn fused_check_pass_reports_exit_zero() {
        let (_guard, root) = crate_root();
        let output = WriteFileTool
            .call(
                json!({
                    "path": "src/new.rs",
                    "content": "pub fn two() -> u32 { 2 }\n",
                    "verify": "check"
                }),
                &crate_ctx(&root),
            )
            .await
            .expect("ok");
        // A file cargo never compiles still verifies green (exit 0).
        assert!(
            output.content.contains("exit status: 0"),
            "{}",
            output.content
        );
        assert!(!output.truncated);
    }
}
