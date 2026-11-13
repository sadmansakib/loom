//! The engine-facing [`FactsSource`] adapter over the scan.

use std::path::PathBuf;

use loom_kernel::context::{FactsSource, KnownFacts};

use super::index::{scan_lessons, scan_skills};
use super::scan::scan;

/// A [`FactsSource`] that scans the filesystem and git.
///
/// The engine constructs one with the canonical workspace root; the agent loop
/// calls it once per task.
#[derive(Debug, Clone)]
pub struct FsFactsSource {
    root: PathBuf,
    skills: bool,
    lessons: bool,
    mcp_menu: Option<String>,
    lsp_menu: Option<String>,
}

impl FsFactsSource {
    /// Creates a source that scans `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            skills: false,
            lessons: false,
            mcp_menu: None,
            lsp_menu: None,
        }
    }

    /// Also indexes `.agents/skills/*.md` into the facts block (iteration 5).
    ///
    /// Off by default: the skills index and the `load_skill` tool are one
    /// opt-in capability, so a default run's facts block is unchanged.
    #[must_use]
    pub fn with_skills(mut self, enabled: bool) -> Self {
        self.skills = enabled;
        self
    }

    /// Also reads `.loom/lessons.json` into the facts block (roadmap
    /// item 20.4).
    ///
    /// Off by default: `loom run --lessons` opts in, so a default run's
    /// facts block is unchanged. A missing or unreadable store degrades
    /// to no lessons, like the rest of the scan.
    #[must_use]
    pub fn with_lessons(mut self, enabled: bool) -> Self {
        self.lessons = enabled;
        self
    }

    /// Bakes the deferred MCP catalog menu into the facts block
    /// (slice D2). The menu is static per task — captured here, rendered
    /// byte-stable by [`KnownFacts::render`] — because the catalog is
    /// fixed at attach time; loads and evictions change the tool
    /// surface, never the menu.
    ///
    /// [`KnownFacts::render`]: loom_kernel::context::KnownFacts::render
    #[must_use]
    pub fn with_mcp_menu(mut self, menu: Option<String>) -> Self {
        self.mcp_menu = menu;
        self
    }

    /// Bakes the attached LSP servers menu into the facts block
    /// (slice E1). Static per task, like the MCP menu: the server set
    /// is fixed at attach time.
    #[must_use]
    pub fn with_lsp_menu(mut self, menu: Option<String>) -> Self {
        self.lsp_menu = menu;
        self
    }
}

impl FactsSource for FsFactsSource {
    fn scan(&self) -> KnownFacts {
        let mut facts = scan(&self.root);
        if self.skills {
            facts.skills = scan_skills(&self.root);
        }
        if self.lessons {
            facts.lessons = scan_lessons(&self.root);
        }
        facts.mcp_menu.clone_from(&self.mcp_menu);
        facts.lsp_menu.clone_from(&self.lsp_menu);
        facts
    }
}
