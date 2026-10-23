//! The `load_skill` tool (iteration 5): serves the body of one skill
//! from `.agents/skills/` on demand.
//!
//! The skills seam is two halves: `FsFactsSource::scan_skills` lists
//! skill **names** (plus one-line descriptions) in the facts block —
//! the model sees the menu for free at task start — and this tool turns
//! a menu pick into the full body. Bodies are capped at
//! [`SKILL_BODY_HEAD_BYTES`] with the standard omission marker, so a
//! runaway file costs one bounded turn, not the context.
//!
//! The tool is read-only and confined: a skill name resolves to exactly
//! `.agents/skills/<name>.md` inside the workspace (the same
//! confinement check the read tools use), and an unknown name is an
//! `InvalidInput` error that lists what is actually on the menu — the
//! model self-corrects in one turn instead of guessing.
//!
//! Registration is gated (`EngineConfig::skills`, default off) so the
//! frozen SWE-bench prompt bytes are unchanged until a frontend opts in.

use crate::budget::{ToolBudgets, head_only};
use crate::confinement::resolve_within;
use futures::future::BoxFuture;
use loom_kernel::tool::{PathTouch, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Loads one skill body by menu name (iteration 5).
#[derive(Debug, Default)]
pub struct LoadSkillTool {
    /// Output budgets (item 20.2): the skill-body head cap.
    pub(crate) budgets: ToolBudgets,
}

impl LoadSkillTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// registration path; item 20.2).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

/// Input shape: the skill name exactly as the facts-block menu shows it.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSkillInput {
    /// Skill name — the `.md` file stem under `.agents/skills/`, as
    /// listed in the facts block's skills menu.
    name: String,
}

impl Tool for LoadSkillTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "load_skill".to_owned(),
            description: "Loads the full body of one skill from the \
                          workspace's `.agents/skills/` directory. Call it \
                          with a name exactly as the skills menu in the \
                          context shows; one skill per call."
                .to_owned(),
            parameters: crate::schema::parameters::<LoadSkillInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        Some(
            match serde_json::from_value::<LoadSkillInput>(input.clone()) {
                Ok(parsed) if is_plain_name(&parsed.name) => vec![PathTouch::read(format!(
                    ".agents/skills/{}.md",
                    parsed.name
                ))],
                _ => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "load_skill takes a skill name from the skills menu in the \
                 context (a `.md` file stem under `.agents/skills/`); one \
                 name per call, no paths or extensions."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: LoadSkillInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            if !is_plain_name(&input.name) {
                return Err(ToolError::InvalidInput(
                    "skill names are plain menu names: no paths, no `..`, \
                     no extension"
                        .to_owned(),
                ));
            }
            let root = ctx.workspace_root.clone();
            let name = input.name;
            let relative = format!(".agents/skills/{name}.md");
            let path = resolve_within(&root, &relative)?;
            let head_bytes = self.budgets.skill_body_head_bytes;
            let (content, listed) =
                tokio::task::spawn_blocking(move || read_skill(&root, &path, &name, head_bytes))
                    .await
                    .map_err(|err| {
                        ToolError::Execution(format!("load_skill task failed: {err}"))
                    })??;
            let _ = listed;
            Ok(ToolOutput::new(content))
        })
    }
}

/// A menu name must be a single path component: non-empty, no
/// separators, no `..` (the confinement check enforces the same, this
/// gives the sharper error).
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != ".." && name != "."
}

/// Reads one skill body and shapes it. Unknown names list the menu so
/// the model corrects in one turn; missing directory says so plainly
/// (the facts block then simply showed no menu).
fn read_skill(
    root: &Path,
    path: &PathBuf,
    name: &str,
    head_bytes: usize,
) -> Result<(String, bool), ToolError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let (shaped, cut) = head_only(&text, head_bytes);
            let header = if cut {
                format!("# skill: {name} (body cut to budget)\n")
            } else {
                format!("# skill: {name}\n")
            };
            Ok((format!("{header}{shaped}"), true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let available = available_skills(root);
            let listing = if available.is_empty() {
                "the workspace has no `.agents/skills/` directory (or it is \
                 empty)"
                    .to_owned()
            } else {
                format!("available skills: {}", available.join(", "))
            };
            Err(ToolError::InvalidInput(format!(
                "no skill named `{name}` — {listing}"
            )))
        }
        Err(error) => Err(ToolError::Execution(format!(
            "reading skill `{name}` failed: {error}"
        ))),
    }
}

/// Stems of the skills on the menu, for unknown-name errors.
fn available_skills(root: &Path) -> Vec<String> {
    let dir = root.join(".agents").join("skills");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut stems: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stem = path.file_stem()?.to_str()?.to_owned();
            (path.extension().and_then(|ext| ext.to_str()) == Some("md")).then_some(stem)
        })
        .collect();
    stems.sort();
    stems.dedup();
    stems
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::ToolContext;

    fn ctx_at(root: &Path) -> ToolContext {
        ToolContext::new(
            root.to_owned(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_secs(30),
        )
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skills = dir.join(".agents").join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join(format!("{name}.md")), body).unwrap();
    }

    fn temp_root(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = tag;
        let path = dir.path().to_owned();
        (dir, path)
    }

    #[tokio::test]
    async fn serves_a_skill_body_under_a_header() {
        let (_dir, root) = temp_root("body");
        write_skill(&root, "release", "# Release checklist\n1. run cargo test\n");
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "release" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.starts_with("# skill: release\n"));
        assert!(out.content.contains("# Release checklist"));
        assert!(!out.content.contains("cut to budget"));
    }

    #[tokio::test]
    async fn cuts_runaway_bodies_with_the_omission_marker() {
        let (_dir, root) = temp_root("cut");
        let body = "x".repeat(crate::budget::SKILL_BODY_HEAD_BYTES + 10_000);
        write_skill(&root, "big", &body);
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "big" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.contains("cut to budget"));
        assert!(out.content.contains("bytes omitted"));
    }

    #[tokio::test]
    async fn unknown_name_lists_the_menu() {
        let (_dir, root) = temp_root("unknown");
        write_skill(&root, "alpha", "a");
        write_skill(&root, "beta", "b");
        let err = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "gamma" }), &ctx_at(&root))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(message) => {
                assert!(message.contains("no skill named `gamma`"));
                assert!(message.contains("available skills: alpha, beta"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_directory_says_so() {
        let (_dir, root) = temp_root("missing");
        let err = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "any" }), &ctx_at(&root))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(message) => {
                assert!(message.contains("no `.agents/skills/` directory"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_traversal_and_path_shapes() {
        let (_dir, root) = temp_root("traversal");
        write_skill(&root, "real", "body");
        for bad in ["../real", ".agents/skills/real", "a/b", "..", ""] {
            let err = LoadSkillTool::default()
                .call(serde_json::json!({ "name": bad }), &ctx_at(&root))
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "name {bad:?} must be rejected as InvalidInput"
            );
        }
    }

    #[test]
    fn touched_paths_reports_the_skill_file() {
        let touches = LoadSkillTool::default()
            .touched_paths(&serde_json::json!({ "name": "release" }))
            .unwrap();
        assert_eq!(touches.len(), 1);
    }

    #[test]
    fn spec_name_is_load_skill() {
        assert_eq!(LoadSkillTool::default().spec().name, "load_skill");
    }
}
