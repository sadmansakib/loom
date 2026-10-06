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
        git: GitState,
    ) -> Self {
        Self {
            root,
            languages,
            build_systems,
            source_dirs,
            test_dirs,
            config_files,
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
        if !self.languages.is_empty() {
            let _ = writeln!(
                out,
                "languages: {}",
                render_list(&self.languages, RENDER_LANGUAGE_CAP)
            );
        }
        if !self.build_systems.is_empty() {
            let _ = writeln!(
                out,
                "build systems: {}",
                render_list(&self.build_systems, RENDER_LIST_CAP)
            );
        }
        if !self.source_dirs.is_empty() {
            let _ = writeln!(
                out,
                "source dirs: {}",
                render_list(&self.source_dirs, RENDER_LIST_CAP)
            );
        }
        if !self.test_dirs.is_empty() {
            let _ = writeln!(
                out,
                "test dirs: {}",
                render_list(&self.test_dirs, RENDER_LIST_CAP)
            );
        }
        if !self.config_files.is_empty() {
            let _ = writeln!(
                out,
                "config files: {}",
                render_list(&self.config_files, RENDER_LIST_CAP)
            );
        }
        match (&self.git.branch, self.git.changed_files.is_empty()) {
            // No branch and no changes: no repository was observed.
            (None, true) => {
                let _ = writeln!(out, "git: no repository detected");
            }
            (branch, _) => {
                if let Some(branch) = branch {
                    let _ = writeln!(out, "git branch: {branch}");
                }
                if self.git.changed_files.is_empty() {
                    let _ = writeln!(out, "changed files: none");
                } else {
                    let _ = writeln!(
                        out,
                        "changed files: {}",
                        render_list(&self.git.changed_files, RENDER_LIST_CAP)
                    );
                }
            }
        }
        // Drop the trailing newline so the block is exactly its lines.
        out.pop();
        out
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
/// Implementations may block (filesystem and process work); the loop calls
/// them from the task setup path, alongside the synchronous store seam.
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
    fn render_includes_structure_and_git_state() {
        let rendered = facts().render();
        assert!(rendered.starts_with("Repository facts"));
        assert!(rendered.contains("root: /ws"));
        assert!(rendered.contains("languages: Rust, Python"));
        assert!(rendered.contains("build systems: Cargo"));
        assert!(rendered.contains("source dirs: crates/loom-kernel/src, src"));
        assert!(rendered.contains("test dirs: tests"));
        assert!(rendered.contains("config files: Cargo.toml"));
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
}
