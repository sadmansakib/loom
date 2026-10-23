//! KnownFacts: the deterministic repository-facts layer (Iteration 2, step 2E;
//! `iteration-2.md` §11, ADR-015).
//!
//! Loom answers structural questions — "what kind of project is this?", "what
//! changed?" — from observable repository state, not from a model turn. This
//! module owns the **pure** half of that: the [`KnownFacts`] value, its stable
//! [`render`](KnownFacts::render)ing, and the [`FactsSource`] seam the agent
//! loop scans through. It performs no I/O.
//!
//! The **I/O** half — walking the workspace and reading git state — lives in a
//! [`FactsSource`] implementation outside the kernel (the engine's
//! `repo` module, ADR-021). The loop asks the source for one snapshot at task
//! start and hands it to the context compiler, which projects it as a stable
//! system message. Because the snapshot is taken once per task, the block is
//! byte-identical across every compile of that task and never disturbs the
//! cached prefix (§8.1.1, invariant 1).
//!
//! Facts are derived from observable state only. They never become long-lived
//! semantic memory here — that is Iteration 3 (ADR-010), for which this is the
//! clean seam.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Largest number of entries rendered for one list, before a `(+N more)`
/// marker. Keeps the block bounded regardless of repository size.
const RENDER_LIST_CAP: usize = 16;

/// Largest number of languages rendered (the distribution is a hint, not an
/// inventory).
const RENDER_LANGUAGE_CAP: usize = 8;

/// A project instruction file surfaced at task start (roadmap item 4).
///
/// The scanner picks the first of the well-known instruction files that
/// exists (`AGENTS.md`, falling back to `CLAUDE.md`) and stores a bounded
/// head of its content, so the model starts with the project's own rules
/// instead of rediscovering them by exploration.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InstructionFile {
    /// Workspace-relative path of the file that matched.
    pub path: String,
    /// Full size of the file in bytes (reported when the text is truncated).
    pub total_bytes: usize,
    /// Bounded head of the file content.
    pub text: String,
}

impl InstructionFile {
    /// Builds an instruction-file snapshot.
    #[must_use]
    pub fn new(path: impl Into<String>, total_bytes: usize, text: String) -> Self {
        Self {
            path: path.into(),
            total_bytes,
            text,
        }
    }
}

/// One entry of the on-demand skills index (iteration 5).
///
/// The scanner lists the skill files it found (name plus a one-line
/// description); bodies stay on disk until the model asks for them with
/// the `load_skill` tool, so the facts block never grows with skill
/// content — the index is a menu, not the meal.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SkillIndexEntry {
    /// Skill name (the file stem under `.agents/skills/`), the handle
    /// `load_skill` accepts.
    pub name: String,
    /// One-line description: the file's first non-empty line, stripped of
    /// Markdown heading syntax, bounded. Empty when the file starts with
    /// its body directly.
    pub description: String,
}

impl SkillIndexEntry {
    /// Builds an index entry.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Git state observed when the facts were captured.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitState {
    /// Current branch name (or `HEAD` when detached). `None` when no git
    /// repository was detected.
    pub branch: Option<String>,
    /// Workspace-relative paths git reports as changed, sorted. Empty for a
    /// clean tree.
    pub changed_files: Vec<String>,
}

impl GitState {
    /// Builds a git-state snapshot.
    #[must_use]
    pub fn new(branch: Option<String>, changed_files: Vec<String>) -> Self {
        Self {
            branch,
            changed_files,
        }
    }

    /// Whether nothing about git state was observed at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.branch.is_none() && self.changed_files.is_empty()
    }
}

/// A deterministic snapshot of repository facts (Iteration 2 §11, stage 1).
///
/// Stage 1 covers repository *structure* (root, languages, build systems,
/// source/test directories, config files) and *git state* (branch, changed
/// files). Symbol intelligence, dependency graphs, and effects-derived change
/// linkage are later stages (2H is gated; effects land in 2F).
///
/// The value is pure data: it can be serialized into an event, compared, and
/// rendered without touching the filesystem. Build one with a [`FactsSource`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KnownFacts {
    /// The workspace root the facts describe.
    pub root: PathBuf,
    /// Source languages detected in the walk, most frequent first (ties broken
    /// by name), capped.
    pub languages: Vec<String>,
    /// Build systems inferred from configuration files (e.g. `Cargo`).
    pub build_systems: Vec<String>,
    /// Workspace-relative source directories (e.g. `src`), sorted.
    pub source_dirs: Vec<String>,
    /// Workspace-relative test directories (e.g. `tests`), sorted.
    pub test_dirs: Vec<String>,
    /// Workspace-relative configuration files recognised by the scan, sorted.
    pub config_files: Vec<String>,
    /// Toolchain and package-manager versions detected for the build
    /// systems (e.g. `cargo 1.89.0`), in probe order. Empty when nothing
    /// was probed or every probe failed.
    pub toolchains: Vec<String>,
    /// Depth-limited workspace tree, pre-rendered one line per entry
    /// (directories carry a trailing `/`, children indented two spaces per
    /// level). Bounded by the scanner; empty when nothing was walked.
    pub tree: Vec<String>,
    /// Project instruction file (`AGENTS.md`, falling back to `CLAUDE.md`),
    /// bounded.
    pub instructions: Option<InstructionFile>,
    /// On-demand skills index (iteration 5): name and one-line
    /// description per skill file found under `.agents/skills/`. Bodies
    /// load through the `load_skill` tool, never through the block.
    /// Gated by the engine's skills flag; empty workspaces render
    /// nothing.
    #[serde(default)]
    pub skills: Vec<SkillIndexEntry>,
    /// Git state at capture time.
    pub git: GitState,
}

impl KnownFacts {
    /// Builds a facts snapshot from its parts (the scanner's constructor).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: PathBuf,
        languages: Vec<String>,
        build_systems: Vec<String>,
        source_dirs: Vec<String>,
        test_dirs: Vec<String>,
        config_files: Vec<String>,
        toolchains: Vec<String>,
        tree: Vec<String>,
        instructions: Option<InstructionFile>,
        skills: Vec<SkillIndexEntry>,
        git: GitState,
    ) -> Self {
        Self {
            root,
            languages,
            build_systems,
            source_dirs,
            test_dirs,
            config_files,
            toolchains,
            tree,
            instructions,
            skills,
            git,
        }
    }

    /// Whether there is anything worth rendering.
    ///
    /// An empty value produces no facts block, so a workspace with nothing
    /// observable adds nothing to the model context.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.languages.is_empty()
            && self.build_systems.is_empty()
            && self.source_dirs.is_empty()
            && self.test_dirs.is_empty()
            && self.config_files.is_empty()
            && self.toolchains.is_empty()
            && self.tree.is_empty()
            && self.instructions.is_none()
            && self.skills.is_empty()
            && self.git.is_empty()
    }

    /// Renders the facts as a compact, deterministic text block.
    ///
    /// The block is byte-stable for a given [`KnownFacts`] value, so a task
    /// that captures one snapshot re-emits identical bytes on every compile.
    /// Lists are sorted by the scanner and capped here; the empty value renders
    /// as the empty string (the compiler then omits the block entirely).
    #[must_use]
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Repository facts (structure + git snapshot, taken when the task started):\n",
        );
        if !self.root.as_os_str().is_empty() {
            let _ = writeln!(out, "root: {}", self.root.display());
        }
        render_listed(&mut out, "languages", &self.languages, RENDER_LANGUAGE_CAP);
        render_listed(
            &mut out,
            "build systems",
            &self.build_systems,
            RENDER_LIST_CAP,
        );
        render_listed(&mut out, "source dirs", &self.source_dirs, RENDER_LIST_CAP);
        render_listed(&mut out, "test dirs", &self.test_dirs, RENDER_LIST_CAP);
        render_listed(
            &mut out,
            "config files",
            &self.config_files,
            RENDER_LIST_CAP,
        );
        if !self.toolchains.is_empty() {
            let _ = writeln!(out, "toolchains: {}", self.toolchains.join(", "));
        }
        if let Some(instructions) = &self.instructions {
            render_instructions(&mut out, instructions);
        }
        render_skills(&mut out, &self.skills);
        if !self.tree.is_empty() {
            let _ = writeln!(out, "file tree:");
            for line in &self.tree {
                let _ = writeln!(out, "{line}");
            }
        }
        render_git(&mut out, &self.git);
        // Drop the trailing newline so the block is exactly its lines.
        out.pop();
        out
    }
}

/// Emits one `label: a, b, (+N more)` line for a non-empty capped list.
fn render_listed(out: &mut String, label: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "{label}: {}", render_list(items, cap));
}

/// Emits the project-instructions block: a header noting any truncation,
/// the text verbatim, and an end marker the model can anchor on.
fn render_instructions(out: &mut String, instructions: &InstructionFile) {
    if instructions.total_bytes > instructions.text.len() {
        let _ = writeln!(
            out,
            "project instructions ({}; first {} of {} bytes):",
            instructions.path,
            instructions.text.len(),
            instructions.total_bytes
        );
    } else {
        let _ = writeln!(out, "project instructions ({}):", instructions.path);
    }
    let _ = writeln!(out, "{}", instructions.text);
    let _ = writeln!(out, "(end of project instructions)");
}

/// Emits the skills index (iteration 5): one line per skill, capped,
/// with the `load_skill` hint. Empty index renders nothing.
fn render_skills(out: &mut String, skills: &[SkillIndexEntry]) {
    if skills.is_empty() {
        return;
    }
    let _ = writeln!(out, "skills (call load_skill <name> for the body):");
    for entry in skills.iter().take(RENDER_LIST_CAP) {
        if entry.description.is_empty() {
            let _ = writeln!(out, "- {}", entry.name);
        } else {
            let _ = writeln!(out, "- {}: {}", entry.name, entry.description);
        }
    }
    let extra = skills.len().saturating_sub(RENDER_LIST_CAP);
    if extra > 0 {
        let _ = writeln!(out, "- (+{extra} more)");
    }
}

/// Emits the git footer: branch when observed, then the changed-file
/// state — nothing at all when no repository was detected.
fn render_git(out: &mut String, git: &GitState) {
    match (&git.branch, git.changed_files.is_empty()) {
        // No branch and no changes: no repository was observed.
        (None, true) => {
            let _ = writeln!(out, "git: no repository detected");
        }
        (branch, _) => {
            if let Some(branch) = branch {
                let _ = writeln!(out, "git branch: {branch}");
            }
            if git.changed_files.is_empty() {
                let _ = writeln!(out, "changed files: none");
            } else {
                let _ = writeln!(
                    out,
                    "changed files: {}",
                    render_list(&git.changed_files, RENDER_LIST_CAP)
                );
            }
        }
    }
}

/// Joins `items` with `, `, showing at most `cap` and appending `(+N more)`.
fn render_list(items: &[String], cap: usize) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    let shown = items
        .iter()
        .take(cap)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let extra = items.len().saturating_sub(cap);
    if extra > 0 {
        format!("{shown} (+{extra} more)")
    } else {
        shown
    }
}

/// The seam the agent loop scans repository facts through.
///
/// The kernel stays free of I/O: an implementation (the engine's
/// filesystem/git scanner) does the walking and git invocation; the kernel
/// only consumes the resulting [`KnownFacts`]. `scan` is called **once per
/// task**, at task start, so the snapshot is stable for the whole task and the
/// cached prefix is never churned mid-task.
///
/// Implementations may block (filesystem and process work); the loop runs
/// the scan on the blocking pool at task start, so a slow workspace cannot
/// stall the async runtime.
pub trait FactsSource: Send + Sync {
    /// Returns one deterministic snapshot of the repository state.
    fn scan(&self) -> KnownFacts;
}

/// A [`FactsSource`] that observes nothing.
///
/// The default seam for an agent with no workspace intelligence: the block is
/// empty and the projection is exactly the transcript, as before 2E.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFacts;

impl FactsSource for NoFacts {
    fn scan(&self) -> KnownFacts {
        KnownFacts::default()
    }
}

/// A [`FactsSource`] that always returns a fixed snapshot.
///
/// For tests and for callers that have already computed the facts (e.g. a
/// replayed run or a fixture).
#[derive(Debug, Clone)]
pub struct StaticFacts(KnownFacts);

impl StaticFacts {
    /// Wraps a pre-computed snapshot.
    #[must_use]
    pub fn new(facts: KnownFacts) -> Self {
        Self(facts)
    }
}

impl FactsSource for StaticFacts {
    fn scan(&self) -> KnownFacts {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> KnownFacts {
        KnownFacts {
            root: PathBuf::from("/ws"),
            languages: vec!["Rust".into(), "Python".into()],
            build_systems: vec!["Cargo".into()],
            source_dirs: vec!["crates/loom-kernel/src".into(), "src".into()],
            test_dirs: vec!["tests".into()],
            config_files: vec!["Cargo.toml".into()],
            toolchains: vec!["cargo 1.89.0".into(), "rustc 1.89.0".into()],
            tree: vec!["Cargo.toml".into(), "src/".into(), "  lib.rs".into()],
            instructions: Some(InstructionFile::new(
                "AGENTS.md",
                21,
                "Run cargo test first.".into(),
            )),
            skills: Vec::new(),
            git: GitState {
                branch: Some("main".into()),
                changed_files: vec!["src/lib.rs".into()],
            },
        }
    }

    #[test]
    fn empty_facts_render_nothing() {
        assert!(KnownFacts::default().is_empty());
        assert_eq!(KnownFacts::default().render(), "");
    }

    #[test]
    fn render_is_deterministic() {
        let facts = facts();
        assert_eq!(facts.render(), facts.render());
    }

    #[test]
    fn skills_index_renders_as_a_menu_with_load_hint() {
        let mut facts = facts();
        facts.skills = vec![
            SkillIndexEntry::new("commit-review", "conventional commits for this repo"),
            SkillIndexEntry::new("deploy", ""),
        ];
        let rendered = facts.render();
        assert!(rendered.contains("skills (call load_skill <name> for the body):"));
        assert!(rendered.contains("- commit-review: conventional commits for this repo"));
        assert!(rendered.contains("- deploy\n"));
        // Determinism holds with skills present.
        assert_eq!(facts.render(), rendered);
    }

    #[test]
    fn skills_index_is_capped_like_other_lists() {
        let mut facts = facts();
        facts.skills = (0..20)
            .map(|i| SkillIndexEntry::new(format!("s{i}"), "d"))
            .collect();
        let rendered = facts.render();
        assert!(rendered.contains("- s15: d"));
        assert!(!rendered.contains("- s16: d"));
        assert!(rendered.contains("- (+4 more)"));
    }

    #[test]
    fn facts_without_skills_field_deserialize_from_old_json() {
        // Schema evolution: events written before iteration 5 carry no
        // `skills` key; `#[serde(default)]` must accept them.
        let old = r#"{
            "root": "/ws",
            "languages": [],
            "build_systems": [],
            "source_dirs": [],
            "test_dirs": [],
            "config_files": [],
            "toolchains": [],
            "tree": [],
            "instructions": null,
            "git": {"branch": null, "changed_files": []}
        }"#;
        let facts: KnownFacts = serde_json::from_str(old).expect("old facts must deserialize");
        assert!(facts.skills.is_empty());
        assert!(facts.is_empty());
    }

    #[test]
    fn render_includes_structure_and_git_state() {
        let rendered = facts().render();
        assert!(rendered.starts_with("Repository facts"));
        assert!(rendered.contains("root: /ws"));
        assert!(rendered.contains("languages: Rust, Python"));
        assert!(rendered.contains("build systems: Cargo"));
        assert!(rendered.contains("source dirs: crates/loom-kernel/src, src"));
        assert!(rendered.contains("test dirs: tests"));
        assert!(rendered.contains("config files: Cargo.toml"));
        assert!(rendered.contains("toolchains: cargo 1.89.0, rustc 1.89.0"));
        assert!(rendered.contains("project instructions (AGENTS.md):"));
        assert!(rendered.contains("Run cargo test first."));
        assert!(rendered.contains("(end of project instructions)"));
        assert!(rendered.contains("file tree:"));
        assert!(rendered.contains("src/"));
        assert!(rendered.contains("  lib.rs"));
        assert!(rendered.contains("git branch: main"));
        assert!(rendered.contains("changed files: src/lib.rs"));
        assert!(!rendered.ends_with('\n'), "no trailing newline");
    }

    #[test]
    fn clean_tree_reports_no_changes() {
        let mut f = facts();
        f.git.changed_files.clear();
        let rendered = f.render();
        assert!(rendered.contains("changed files: none"));
    }

    #[test]
    fn missing_repository_is_stated() {
        let mut f = facts();
        f.git = GitState::default();
        let rendered = f.render();
        assert!(rendered.contains("git: no repository detected"));
        assert!(!rendered.contains("changed files"));
    }

    #[test]
    fn long_lists_are_capped_with_a_marker() {
        let mut f = facts();
        f.git.changed_files = (0..20).map(|i| format!("file{i}.rs")).collect();
        let rendered = f.render();
        assert!(rendered.contains("(+4 more)"), "{rendered}");
        // The first sixteen are shown, none beyond.
        assert!(rendered.contains("file15.rs"));
        assert!(!rendered.contains("file16.rs"));
    }

    #[test]
    fn static_source_returns_its_snapshot() {
        let source = StaticFacts::new(facts());
        assert_eq!(source.scan(), facts());
    }

    #[test]
    fn default_source_observes_nothing() {
        assert!(NoFacts.scan().is_empty());
    }

    #[test]
    fn truncated_instructions_report_their_size() {
        let mut f = facts();
        f.instructions = Some(InstructionFile::new(
            "AGENTS.md",
            10_000,
            "first 4096 bytes".into(),
        ));
        let rendered = f.render();
        assert!(
            rendered.contains("project instructions (AGENTS.md; first 16 of 10000 bytes):"),
            "{rendered}"
        );
    }

    #[test]
    fn tree_only_facts_still_render() {
        let f = KnownFacts {
            tree: vec!["src/".into(), "  main.rs".into()],
            ..KnownFacts::default()
        };
        assert!(!f.is_empty());
        let rendered = f.render();
        assert!(rendered.starts_with("Repository facts"));
        assert!(rendered.contains("file tree:"));
        assert!(!rendered.contains("languages:"));
    }
}
