//! The `artifact_read` tool: refetch a tool result that was stored as an
//! artifact.
//!
//! A tool result too large for the inline budget is written to the artifact
//! store (content-addressed, ADR-012 decision 3) and the transcript keeps a
//! compact `ArtifactRef`; the model sees a note naming the artifact id. This
//! tool fetches that body back and renders it with the same numbered window
//! as `read_file` (ADR-014), so the model never needs the whole body at once.

use futures::future::BoxFuture;
use loom_kernel::id::ArtifactId;
use loom_kernel::tool::{PathTouch, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::budget::ToolBudgets;
use crate::window::{self, DEFAULT_LIMIT};

/// Refetches a stored tool-output artifact as a numbered line window.
#[derive(Debug, Default)]
pub struct ArtifactReadTool {
    /// Output budgets (item 20.2): artifact bodies render through the
    /// same read window as files. `Default` keeps the constants.
    budgets: ToolBudgets,
}

impl ArtifactReadTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// entry point).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct ArtifactReadInput {
    /// Artifact id from a `stored as artifact <id>` note.
    id: String,
    /// First line to return, 1-based.
    #[serde(default)]
    #[schemars(range(min = 1))]
    offset: Option<usize>,
    /// Maximum lines to return; defaults to the whole body.
    #[serde(default)]
    #[schemars(range(min = 1))]
    limit: Option<usize>,
}

impl Tool for ArtifactReadTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "artifact_read".to_owned(),
            description: "Reads a tool result that was too large for the inline budget \
                          and was stored as an artifact. Pass the id from the `stored as \
                          artifact <id>` note. Returns numbered lines; page with `offset` \
                          and `limit` like `read_file`."
                .to_owned(),
            parameters: crate::schema::parameters::<ArtifactReadInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, _input: &Value) -> Option<Vec<PathTouch>> {
        // Reads from the artifact store, not the workspace: no path conflict.
        Some(Vec::new())
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: ArtifactReadInput = serde_json::from_value(input)
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
            let id = input.id.parse::<ArtifactId>().map_err(|err| {
                ToolError::InvalidInput(format!("invalid artifact id \"{}\": {err}", input.id))
            })?;
            // A run without an artifact store cannot have spilled anything, so
            // an id could not have come from anywhere (ADR-012 decision 3).
            let Some(store) = &ctx.artifacts else {
                return Err(ToolError::Execution(
                    "no artifact store is configured for this run".to_owned(),
                ));
            };
            // Blocking SQLite read off the async worker, like the file tools.
            let store = Arc::clone(store);
            let fetched = tokio::task::spawn_blocking(move || store.get(&id))
                .await
                .map_err(|err| ToolError::Execution(format!("artifact read task failed: {err}")))?
                .map_err(|err| ToolError::Execution(format!("cannot read artifact: {err}")))?;
            let Some(artifact) = fetched else {
                return Ok(ToolOutput::new(format!(
                    "artifact {} does not exist",
                    input.id
                )));
            };
            let Some((text, complete)) = window::decode_body(artifact.body) else {
                return Err(ToolError::Execution(format!(
                    "artifact {} is not valid UTF-8",
                    input.id
                )));
            };
            let label = format!("artifact {} [{}]", input.id, artifact.kind);
            let window = window::Window::from_text(&text, offset, limit, complete);
            let (content, truncated) =
                window::render_window(&label, &window, offset, self.budgets.read_file_head_bytes);
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
    use loom_kernel::artifact::store_artifact;
    use loom_kernel::store::MemoryArtifactStore;
    use loom_kernel::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx_with(store: Arc<MemoryArtifactStore>) -> ToolContext {
        ToolContext::new(
            PathBuf::new(),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .with_artifacts(Some(store))
    }

    fn ctx_without() -> ToolContext {
        ToolContext::new(
            PathBuf::new(),
            CancellationToken::new(),
            Duration::from_secs(10),
        )
    }

    fn store(body: &[u8]) -> (Arc<MemoryArtifactStore>, ArtifactId) {
        let store = Arc::new(MemoryArtifactStore::new());
        let reference = store_artifact(store.as_ref(), "tool_output", body).expect("store");
        (store, reference.id)
    }

    #[tokio::test]
    async fn reads_a_stored_artifact_with_numbered_lines() {
        let (store, id) = store(b"alpha\nbeta\n");
        let output = ArtifactReadTool::default()
            .call(json!({"id": id.to_string()}), &ctx_with(store))
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            format!("artifact {id} [tool_output]: 2 lines\n1\talpha\n2\tbeta\n")
        );
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn window_selects_lines_and_points_at_the_next() {
        let body = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (store, id) = store(body.as_bytes());
        let output = ArtifactReadTool::default()
            .call(
                json!({"id": id.to_string(), "offset": 4, "limit": 3}),
                &ctx_with(store),
            )
            .await
            .expect("ok");
        assert_eq!(
            output.content,
            format!(
                "artifact {id} [tool_output]: 10 lines\n4\tline 4\n5\tline 5\n6\tline 6\n\
                 ... [4 more lines; read with offset=7]\n"
            )
        );
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn a_missing_artifact_is_informational() {
        let (store, _) = store(b"body");
        let missing = ArtifactId::new();
        let output = ArtifactReadTool::default()
            .call(json!({"id": missing.to_string()}), &ctx_with(store))
            .await
            .expect("ok");
        assert_eq!(output.content, format!("artifact {missing} does not exist"));
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn no_store_is_an_execution_error() {
        let (_, id) = store(b"body");
        let err = ArtifactReadTool::default()
            .call(json!({"id": id.to_string()}), &ctx_without())
            .await
            .expect_err("no store");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("artifact store")));
    }

    #[tokio::test]
    async fn an_invalid_id_is_invalid_input() {
        let (store, _) = store(b"body");
        let err = ArtifactReadTool::default()
            .call(json!({"id": "not-an-id"}), &ctx_with(store))
            .await
            .expect_err("bad id");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("artifact id")));
    }

    #[tokio::test]
    async fn a_zero_offset_is_invalid_input() {
        let (store, id) = store(b"body");
        let err = ArtifactReadTool::default()
            .call(json!({"id": id.to_string(), "offset": 0}), &ctx_with(store))
            .await
            .expect_err("zero");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("offset")));
    }

    #[test]
    fn the_schema_and_the_parser_agree() {
        crate::schema::agreement::check::<ArtifactReadInput>(json!({
            "id": "0190f0e0-0000-7000-8000-000000000000"
        }));
    }
}
