//! The `load_skill` tool (iteration 5): serves the body of one skill
//! from `.agents/skills/` on demand.
//!
//! Two file shapes resolve to the same menu name: the original flat
//! `.agents/skills/<name>.md`, and the spec-shaped
//! `.agents/skills/<name>/SKILL.md` (Agent Skills convention). The flat
//! shape wins when both exist.
//!
//! Both shapes may open with `---`-delimited frontmatter (a tiny
//! single-line `key: value` subset: `name`, `description`). The block
//! is stripped before the body ships, the description feeds the facts
//! menu one-liner, and a frontmatter `name` that disagrees with the
//! file stem keeps the skill off the menu (mislabeled data, not a
//! silent alias).
//!
//! The skills seam is two halves: `FsFactsSource::scan_skills` lists
//! skill **names** (plus one-line descriptions) in the facts block —
//! the model sees the menu for free at task start — and this tool turns
//! a menu pick into the full body. Bodies are capped at
//! [`SKILL_BODY_HEAD_BYTES`] with the standard omission marker, so a
//! runaway file costs one bounded turn, not the context.
//!
//! The tool is read-only and confined: a skill name resolves to exactly
//! one of the two shapes above inside the workspace (the same
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

/// Input shape: the skill name exactly as the facts-block menu shows it,
/// plus an optional bundled resource (L3) relative to the skill
/// directory.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSkillInput {
    /// Skill name — the `.md` file stem or `SKILL.md` directory name
    /// under `.agents/skills/`, as listed in the facts block's skills
    /// menu.
    name: String,
    /// Optional bundled file to read instead of the body: a path
    /// relative to the skill's directory (`references/api.md`), as
    /// shown in the `bundled files` listing a body ends with. Scripts
    /// are never executed — only read.
    resource: Option<String>,
}

impl Tool for LoadSkillTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "load_skill".to_owned(),
            description: "Loads the full body of one skill from the \
                          workspace's `.agents/skills/` directory (flat \
                          `<name>.md` or `<name>/SKILL.md`), or — with \
                          `resource` — one bundled file from the skill's \
                          directory (see the `bundled files` listing at \
                          the end of a body). Call it with a name exactly \
                          as the skills menu in the context shows; one \
                          skill per call. Bundled scripts are data: they \
                          are read, never executed."
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
                Ok(parsed) if is_plain_name(&parsed.name) => match parsed.resource {
                    // L3 read: only the bundled file is touched.
                    Some(resource) if is_plain_resource(&resource) => vec![PathTouch::read(
                        format!(".agents/skills/{}/{}", parsed.name, resource),
                    )],
                    Some(_) => Vec::new(),
                    // Body read: both candidate shapes are declared —
                    // which exists is known only at read time.
                    None => vec![
                        PathTouch::read(format!(".agents/skills/{}.md", parsed.name)),
                        PathTouch::read(format!(".agents/skills/{}/SKILL.md", parsed.name)),
                    ],
                },
                _ => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "load_skill takes a skill name from the skills menu in the \
                 context (a `.md` file stem or `SKILL.md` directory name \
                 under `.agents/skills/`); one name per call, no paths or \
                 extensions."
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
            let head_bytes = self.budgets.skill_body_head_bytes;
            // L3: a `resource` request reads one bundled file instead of
            // the body. Confined to the skill directory; scripts are
            // data, never executed.
            if let Some(resource) = input.resource {
                if !is_plain_resource(&resource) {
                    return Err(ToolError::InvalidInput(
                        "skill resources are paths relative to the skill \
                         directory: no `..`, no backslashes, no absolute \
                         paths"
                            .to_owned(),
                    ));
                }
                let (content, _) = tokio::task::spawn_blocking(move || {
                    read_resource(&root, &name, &resource, head_bytes)
                })
                .await
                .map_err(|err| ToolError::Execution(format!("load_skill task failed: {err}")))??;
                return Ok(ToolOutput::new(content));
            }
            let relative_flat = format!(".agents/skills/{name}.md");
            let relative_dir = format!(".agents/skills/{name}/SKILL.md");
            let candidates = [
                resolve_within(&root, &relative_flat)?,
                resolve_within(&root, &relative_dir)?,
            ];
            let (content, listed) = tokio::task::spawn_blocking(move || {
                read_skill(&root, &candidates, &name, head_bytes)
            })
            .await
            .map_err(|err| ToolError::Execution(format!("load_skill task failed: {err}")))??;
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

/// The subset of `---`-delimited frontmatter loom understands: flat
/// single-line `key: value` pairs for `name` and `description`.
#[derive(Debug, Default, Clone)]
pub struct SkillFrontmatter {
    /// `name:` from the block, when present and non-empty.
    pub name: Option<String>,
    /// `description:` from the block, when present and non-empty.
    pub description: Option<String>,
}

/// Splits optional `---`-delimited frontmatter off a skill file.
///
/// Recognizes exactly a leading `---` line, `key: value` lines, and a
/// closing `---` line; anything else inside the block is ignored. A
/// missing opening fence or an unterminated block returns no
/// frontmatter and the full text as the body (fail-open to the
/// pre-frontmatter behavior — malformed metadata never hides a body).
pub fn split_frontmatter(text: &str) -> (SkillFrontmatter, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (SkillFrontmatter::default(), text);
    };
    let mut frontmatter = SkillFrontmatter::default();
    let mut rest = rest;
    loop {
        let Some((line, tail)) = rest.split_once('\n') else {
            // No closing fence: treat the whole file as body.
            return (SkillFrontmatter::default(), text);
        };
        if line.trim_end_matches('\r') == "---" {
            return (frontmatter, tail);
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "name" if frontmatter.name.is_none() && !value.is_empty() => {
                    frontmatter.name = Some(value.to_owned());
                }
                "description" if frontmatter.description.is_none() && !value.is_empty() => {
                    frontmatter.description = Some(value.to_owned());
                }
                _ => {}
            }
        }
        rest = tail;
    }
}

/// Reads one skill body and shapes it. Unknown names list the menu so
/// the model corrects in one turn; missing directory says so plainly
/// (the facts block then simply showed no menu). `candidates` carries
/// the resolution order (flat file first, `SKILL.md` second); the first
/// readable candidate wins. Frontmatter is stripped before the head
/// budget is applied.
fn read_skill(
    root: &Path,
    candidates: &[PathBuf],
    name: &str,
    head_bytes: usize,
) -> Result<(String, bool), ToolError> {
    for path in candidates {
        match std::fs::read(path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let (_frontmatter, body) = split_frontmatter(&text);
                let (shaped, cut) = head_only(body, head_bytes);
                let header = if cut {
                    format!("# skill: {name} (body cut to budget)\n")
                } else {
                    format!("# skill: {name}\n")
                };
                let mut content = format!("{header}{shaped}");
                // L3 progressive disclosure: a dir-shaped skill ends
                // with a bounded listing of its bundled files so the
                // model can request them by name (`resource` input).
                if path.file_name().is_some_and(|f| f == "SKILL.md")
                    && let Some(dir) = path.parent()
                {
                    let bundled = list_bundled(dir);
                    if !bundled.is_empty() {
                        let shown: Vec<&str> = bundled
                            .iter()
                            .take(BUNDLED_LIST_MAX)
                            .map(String::as_str)
                            .collect();
                        let remaining = bundled.len() - shown.len();
                        let mut listing = format!(
                            "\n\nbundled files (load_skill with `name` + `resource`; scripts are read, never run):\n  {}",
                            shown.join("\n  ")
                        );
                        if remaining > 0 {
                            listing.push_str(&format!("\n  +{remaining} more"));
                        }
                        content.push_str(&listing);
                    }
                }
                return Ok((content, true));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ToolError::Execution(format!(
                    "reading skill `{name}` failed: {error}"
                )));
            }
        }
    }
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

/// Bundled-file listing cap (entries; the rest collapses to `+N more`).
const BUNDLED_LIST_MAX: usize = 16;
/// Depth cap for the bundled-file walk (SKILL.md's directory = 0).
const BUNDLED_WALK_DEPTH: usize = 4;

/// A bundled-resource path must stay a relative slash path with no
/// `..` components (the confinement check enforces the same; this
/// gives the sharper error).
fn is_plain_resource(resource: &str) -> bool {
    !resource.is_empty()
        && !resource.contains('\\')
        && !resource.starts_with('/')
        && resource
            .split('/')
            .all(|component| !component.is_empty() && component != ".." && component != ".")
}

/// Relative paths of the files bundled in a skill directory, sorted,
/// `SKILL.md` excluded. Bounded: depth ≤ [`BUNDLED_WALK_DEPTH`],
/// listing capped by the caller.
fn list_bundled(dir: &Path) -> Vec<String> {
    fn walk(out: &mut Vec<String>, dir: &Path, prefix: &str, depth: usize) {
        if depth > BUNDLED_WALK_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = if prefix.is_empty() {
                file_name.clone()
            } else {
                format!("{prefix}/{file_name}")
            };
            if file_type.is_dir() {
                walk(out, &entry.path(), &relative, depth + 1);
            } else if file_type.is_file() && file_name != "SKILL.md" {
                out.push(relative);
            }
        }
    }
    let mut out = Vec::new();
    walk(&mut out, dir, "", 0);
    out.sort();
    out
}

/// Reads one bundled resource from a skill directory, head-capped with
/// the standard header. Scripts are served as data exactly like any
/// other file — execution flows through `run_command`'s policy, never
/// here.
fn read_resource(
    root: &Path,
    name: &str,
    resource: &str,
    head_bytes: usize,
) -> Result<(String, bool), ToolError> {
    let relative = format!(".agents/skills/{name}/{resource}");
    let path = resolve_within(root, &relative)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let (shaped, cut) = head_only(&text, head_bytes);
            let header = if cut {
                format!("# skill resource: {name}/{resource} (cut to budget)\n")
            } else {
                format!("# skill resource: {name}/{resource}\n")
            };
            Ok((format!("{header}{shaped}"), true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolError::InvalidInput(format!(
                "skill `{name}` has no bundled resource `{resource}` — \
                 load the skill body (`name` alone) to see its bundled \
                 files listing"
            )))
        }
        Err(error) => Err(ToolError::Execution(format!(
            "reading skill resource `{name}/{resource}` failed: {error}"
        ))),
    }
}

/// Menu names of the skills on disk, for unknown-name errors: flat
/// `.md` stems and `SKILL.md` directory names.
fn available_skills(root: &Path) -> Vec<String> {
    let dir = root.join(".agents").join("skills");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let stem = path.file_stem()?.to_str()?.to_owned();
                (path.extension().and_then(|ext| ext.to_str()) == Some("md")).then_some(stem)
            } else if entry.file_type().is_ok_and(|t| t.is_dir()) && path.join("SKILL.md").is_file()
            {
                path.file_name()?.to_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
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

    fn write_dir_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(".agents").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), body).unwrap();
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
    async fn serves_a_dir_shaped_skill_md() {
        let (_dir, root) = temp_root("dirshape");
        write_dir_skill(
            &root,
            "triage",
            "---\nname: triage\ndescription: sort incoming issues\n---\nSort issues by severity.\n",
        );
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "triage" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.starts_with("# skill: triage\n"));
        // Frontmatter is metadata for the menu, not body for the model.
        assert!(!out.content.contains("description: sort incoming issues"));
        assert!(out.content.contains("Sort issues by severity."));
    }

    #[tokio::test]
    async fn flat_shape_wins_over_dir_shape() {
        let (_dir, root) = temp_root("both");
        write_skill(&root, "both", "flat body");
        write_dir_skill(&root, "both", "dir body");
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "both" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.contains("flat body"));
        assert!(!out.content.contains("dir body"));
    }

    #[tokio::test]
    async fn frontmatter_from_a_flat_file_is_stripped() {
        let (_dir, root) = temp_root("fmflat");
        write_skill(
            &root,
            "fm",
            "---\nname: fm\ndescription: does things\n---\n# Real body\n",
        );
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "fm" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.contains("# Real body"));
        assert!(!out.content.contains("does things"));
    }

    #[test]
    fn split_frontmatter_parses_and_fails_open() {
        let (fm, body) = split_frontmatter("---\nname: a\ndescription: b\n---\nrest");
        assert_eq!(fm.name.as_deref(), Some("a"));
        assert_eq!(fm.description.as_deref(), Some("b"));
        assert_eq!(body, "rest");

        // Unterminated block: full text is the body.
        let (fm, body) = split_frontmatter("---\nname: a\nno close");
        assert!(fm.name.is_none());
        assert_eq!(body, "---\nname: a\nno close");

        // No fence: untouched.
        let (fm, body) = split_frontmatter("plain");
        assert!(fm.name.is_none());
        assert_eq!(body, "plain");
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
        // Both candidate shapes are declared: which one exists on disk
        // is only known at read time.
        assert_eq!(touches.len(), 2);
    }

    #[test]
    fn spec_name_is_load_skill() {
        assert_eq!(LoadSkillTool::default().spec().name, "load_skill");
    }

    #[tokio::test]
    async fn dir_body_lists_bundled_files() {
        let (_dir, root) = temp_root("bundled");
        let skill_dir = root.join(".agents").join("skills").join("deploy");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "Deploy steps.\n").unwrap();
        std::fs::write(skill_dir.join("notes.md"), "extra").unwrap();
        std::fs::write(skill_dir.join("scripts").join("run.sh"), "#!/bin/sh").unwrap();

        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "deploy" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(out.content.contains("bundled files"), "{}", out.content);
        assert!(out.content.contains("notes.md"));
        assert!(out.content.contains("scripts/run.sh"));
        // The body file itself never appears as a bundled entry.
        let listing = out.content.split("bundled files").nth(1).unwrap();
        assert!(!listing.contains("SKILL.md"), "{}", listing);
    }

    #[tokio::test]
    async fn resource_load_returns_bundled_content() {
        let (_dir, root) = temp_root("resload");
        let skill_dir = root.join(".agents").join("skills").join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "Deploy steps.\n").unwrap();
        std::fs::write(skill_dir.join("notes.md"), "port 8080").unwrap();

        let out = LoadSkillTool::default()
            .call(
                serde_json::json!({ "name": "deploy", "resource": "notes.md" }),
                &ctx_at(&root),
            )
            .await
            .unwrap();
        assert!(
            out.content
                .starts_with("# skill resource: deploy/notes.md\n"),
            "{}",
            out.content
        );
        assert!(out.content.contains("port 8080"));
    }

    #[tokio::test]
    async fn resource_traversal_is_rejected() {
        let (_dir, root) = temp_root("restrav");
        write_skill(&root, "real", "body");
        for bad in ["../real.md", "a/../../evil", "/abs", "back\\slash", ""] {
            let err = LoadSkillTool::default()
                .call(
                    serde_json::json!({ "name": "real", "resource": bad }),
                    &ctx_at(&root),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidInput(_)),
                "resource {bad:?} must be rejected as InvalidInput"
            );
        }
    }

    #[tokio::test]
    async fn flat_shape_gets_no_bundled_listing() {
        let (_dir, root) = temp_root("flatlist");
        write_skill(&root, "solo", "just a body");
        let out = LoadSkillTool::default()
            .call(serde_json::json!({ "name": "solo" }), &ctx_at(&root))
            .await
            .unwrap();
        assert!(!out.content.contains("bundled files"));
    }

    #[test]
    fn touched_paths_reports_the_resource_when_given() {
        let touches = LoadSkillTool::default()
            .touched_paths(&serde_json::json!({
                "name": "deploy",
                "resource": "notes.md"
            }))
            .unwrap();
        assert_eq!(touches.len(), 1);
    }
}
