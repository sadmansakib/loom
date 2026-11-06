//! The `write_file` tool: create or overwrite one workspace file.
//!
//! Mutating (architecture §10): the engine runs it sequentially. Path
//! handling goes through [`resolve_within`] exactly like the read-only
//! tools, so an escape is an execution-invariant violation, not policy.

use futures::future::BoxFuture;
use loom_kernel::tool::{
    BeforeImage, PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
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
pub struct WriteFileTool {
    /// Whether a failing fused cargo verify attempts one
    /// machine-applicable fix round under a never-worse gate (roadmap
    /// item 33). Off by default; set from the engine's opt-in config.
    autofix: bool,
}

impl WriteFileTool {
    /// Enables the fused-verify autofix round (roadmap item 33).
    #[must_use]
    pub fn with_autofix(mut self, autofix: bool) -> Self {
        self.autofix = autofix;
        self
    }
}

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
            let content = input.content;
            let written = content.len();
            let moved_path = path.clone();
            let moved_display = display.clone();
            // Tier-2 rewind capture rides the blocking write below: the
            // pre-image is read from the file the tool is about to
            // modify, before the write, and returned to the effects.
            let artifacts = ctx.artifacts.clone();
            let captured_relative = relative.clone();
            let before_images =
                tokio::task::spawn_blocking(move || -> Result<Vec<BeforeImage>, ToolError> {
                    // Syntax guard (roadmap item 5 phase B): refuse the
                    // candidate before anything reaches the disk. A file the
                    // grammar already cannot read (no file yet, or a legacy
                    // buffer that fails to parse) must not block the write:
                    // the guard only refuses a buffer it is making worse.
                    // One read feeds both the guard and the tier-2
                    // pre-image: raw bytes (UTF-8 loss degrades to the same
                    // empty previous the guard already tolerated).
                    let bytes = match std::fs::read(&moved_path) {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => {
                            return Err(ToolError::Execution(format!(
                                "cannot read \"{moved_display}\": {error}"
                            )));
                        }
                    };
                    let previous =
                        std::str::from_utf8(bytes.as_deref().unwrap_or(b"")).unwrap_or_default();
                    crate::syntax::guard_against(previous, &content, &moved_path).map_err(
                        |err| {
                            ToolError::InvalidInput(format!(
                                "refusing to write \"{moved_display}\": {err}"
                            ))
                        },
                    )?;
                    let before_images = crate::pre_image::capture_before_image(
                        artifacts.as_deref(),
                        &captured_relative,
                        bytes.as_deref(),
                    )
                    .into_iter()
                    .collect();
                    write_file(&moved_path, content.as_bytes()).map_err(|err| {
                        ToolError::Execution(format!("cannot write \"{moved_display}\": {err}"))
                    })?;
                    Ok(before_images)
                })
                .await
                .map_err(|err| ToolError::Execution(format!("write task failed: {err}")))??;
            let mut effects = ToolEffects::none().with_file_written(relative.clone());
            for image in before_images {
                effects = effects.with_before_image(image);
            }
            let mut content = format!("wrote {written} bytes to {display}");
            let mut truncated = false;
            // Trace-level observations from the fused verify (e.g. the
            // unconfined-runner notice) ride the host output.
            let mut notices = Vec::new();
            if let Some(scope) = scope {
                let (rendered, fused_effects) =
                    crate::fusion::run_fused(scope, ctx, &relative, self.autofix).await?;
                content = format!("{content}\n\n--- verify ---\n{}", rendered.content);
                truncated = rendered.truncated;
                notices = rendered.notices;
                // The fused run's command (and any kept autofix writes,
                // pre-images, and repair notes) fold into this call's
                // effects (roadmap item 33).
                effects = effects.merge(fused_effects);
            }
            let mut output = ToolOutput::new(content).with_effects(effects);
            output.notices = notices;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        WriteFileTool::default().call(args, &ctx(root)).await
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

    // env_lock serializes guard state; holding it across the awaited call is the point.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn broken_rust_is_refused_before_anything_reaches_disk() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        let err = write(
            &root,
            json!({"path": "src/lib.rs", "content": "pub fn one() -> u32 { 1 "}),
        )
        .await
        .expect_err("guard refuses unparsable Rust");
        assert!(
            matches!(&err, ToolError::InvalidInput(msg) if msg.contains("syntax error at line")),
            "{err:?}"
        );
        // Refused before the write: not even the parent directory was
        // created.
        assert!(!guard.path().join("src").exists());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn valid_rust_write_passes_the_guard() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        let output = write(
            &root,
            json!({"path": "src/lib.rs", "content": "pub fn one() -> u32 { 1 }\n"}),
        )
        .await
        .expect("clean parse passes");
        assert!(output.content.starts_with("wrote "), "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(guard.path().join("src/lib.rs")).expect("read"),
            "pub fn one() -> u32 { 1 }\n"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn overwriting_a_legacy_try_macro_file_is_allowed() {
        let _lock = crate::syntax::testing::env_lock();
        let (guard, root) = temp_root();
        // The previous text already fails this grammar, so the guard has
        // nothing to protect: refusing here would only lock the model out.
        std::fs::write(
            guard.path().join("a.rs"),
            b"fn a() {\n    let mmap = try!(self.mmap(&paths));\n}\n",
        )
        .expect("seed");
        write(
            &root,
            json!({"path": "a.rs", "content": "fn b() {\n    let mmap = try!(self.mmap(&paths));\n}\n"}),
        )
        .await
        .expect("a file the grammar cannot read must stay writable");
        assert_eq!(
            std::fs::read_to_string(guard.path().join("a.rs")).expect("read"),
            "fn b() {\n    let mmap = try!(self.mmap(&paths));\n}\n"
        );
    }

    #[tokio::test]
    async fn a_disabled_guard_allows_broken_rust() {
        let _disabled = crate::syntax::testing::disable_guard();
        let (guard, root) = temp_root();
        let broken = "pub fn one() -> u32 { 1 ";
        let output = write(&root, json!({"path": "src/lib.rs", "content": broken}))
            .await
            .expect("disabled guard allows the write");
        assert!(output.content.starts_with("wrote "), "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(guard.path().join("src/lib.rs")).expect("read"),
            broken
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
        // Give each invocation its own crate name: cargo's fingerprint
        // database is keyed by unit (name+version+target+profile), not by
        // source path, so two concurrent tests compiling same-named crates
        // into a shared CARGO_TARGET_DIR race — the second one finds the
        // first one's fingerprint and skips compiling its own (possibly
        // broken) content. A unique name per test makes the units distinct.
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        std::fs::create_dir(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"fusion-fixture-{seq}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 [lib]\npath = \"src/lib.rs\"\n"
            ),
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
        // This test deliberately writes unparsable Rust so the fused
        // cargo check has diagnostics to report; the syntax guard
        // would refuse the write, so it is disabled here only.
        let _disabled = crate::syntax::testing::disable_guard();
        let (_guard, root) = crate_root();
        // A syntax error: the fused check must fail while still reporting
        // the write itself as done.
        let output = WriteFileTool::default()
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
        let output = WriteFileTool::default()
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
