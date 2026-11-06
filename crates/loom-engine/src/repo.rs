//! Stage-1 repository intelligence (Iteration 2 step 2E; ADR-015).
//!
//! [`scan`] produces a bounded, deterministic [`KnownFacts`] snapshot of a
//! workspace: repository structure (languages, build systems, source and test
//! directories, configuration files) plus git state (branch, changed files).
//! It is the **I/O half** of the facts layer — the pure [`KnownFacts`] value
//! and the [`FactsSource`] seam live in `loom-kernel` — so the kernel stays
//! free of filesystem and process dependencies (ADR-001, ADR-021).
//!
//! The scan is deliberately bounded (depth, entry count, rendered list
//! lengths) and sorted, so it is deterministic for a given tree and cheap
//! enough to run once per task. It never fails: an unreadable directory or a
//! missing `git` degrades to fewer facts rather than failing task setup.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;
use loom_kernel::context::{
    FactsSource, GitState, InstructionFile, KnownFacts, LessonIndexEntry, SkillIndexEntry,
};
use loom_kernel::lessons::{self, MinedLesson};
use loom_tools::skills::split_frontmatter;

/// How deep into the tree the walk descends. Enough to reach
/// `crates/<crate>/src` and `crates/<crate>/tests` in a monorepo, bounded so a
/// pathological tree cannot stall the scan.
const MAX_DEPTH: usize = 4;

/// Depth of the rendered file tree (shallower than the walk: the tree is a
/// map, not an inventory). Four levels reach `crates/<crate>/src/<file>` in
/// a monorepo; shallow repos are unaffected.
const TREE_DEPTH: usize = 4;

/// Hard cap on entries visited, a backstop against enormous trees.
const MAX_ENTRIES: usize = 20_000;

/// Hard cap on rendered tree lines; the remainder is summarised by a
/// `(+N more entries)` marker so the block stays bounded.
const TREE_MAX_LINES: usize = 200;

/// Project instruction files, in preference order.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Head of an instruction file kept in the facts block.
const INSTRUCTIONS_MAX_BYTES: usize = 4096;

/// Directory scanned for the on-demand skills index (iteration 5).
const SKILLS_DIR: &[&str] = &[".agents", "skills"];

/// The lessons store mined by `loom lessons` (roadmap item 20.4).
const LESSONS_FILE: &[&str] = &[".loom", "lessons.json"];

/// Cap on lesson entries read from the store; the render caps again at
/// the list level (mirrors the skills index two caps).
const LESSONS_MAX_ENTRIES: usize = 64;

/// Longest lesson statement the store scan will inject — a runaway file
/// costs one bounded line, not the context.
const LESSON_STATEMENT_MAX_CHARS: usize = 512;

/// Cap on indexed skill files; the render caps again at the list level.
const SKILLS_MAX_FILES: usize = 64;

/// Head of a skill file read when deriving its one-line description.
const SKILL_DESCRIPTION_READ_BYTES: usize = 1024;

/// Byte budget for a rendered skill description.
const SKILL_DESCRIPTION_MAX_BYTES: usize = 120;

/// Toolchain probes per detected build system, in deterministic order.
/// Each entry is `(build system, program, args, human prefix)` — the
/// rendered line is the trimmed stdout verbatim (e.g. `cargo 1.89.0`).
const TOOLCHAIN_PROBES: &[(&str, &str, &[&str])] = &[
    ("Cargo", "cargo", &["--version"]),
    ("Cargo", "rustc", &["--version"]),
    ("npm", "node", &["--version"]),
    ("npm", "npm", &["--version"]),
    ("Python", "python3", &["--version"]),
    ("Go", "go", &["version"]),
];

/// Files whose presence identifies a build system, and the system they imply.
const BUILD_SYSTEMS: &[(&str, &str)] = &[
    ("Cargo.toml", "Cargo"),
    ("package.json", "npm"),
    ("pyproject.toml", "Python"),
    ("setup.py", "Python"),
    ("go.mod", "Go"),
    ("Makefile", "Make"),
    ("makefile", "Make"),
    ("CMakeLists.txt", "CMake"),
    ("build.gradle", "Gradle"),
    ("build.gradle.kts", "Gradle"),
    ("pom.xml", "Maven"),
];

/// Configuration files worth naming even when they imply no build system.
const EXTRA_CONFIG_FILES: &[&str] = &[
    "rust-toolchain.toml",
    "rustfmt.toml",
    "clippy.toml",
    "tsconfig.json",
    "justfile",
    "Justfile",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
];

/// Source-language extensions reported in the distribution. Configuration and
/// prose files are intentionally excluded: the fact is "what language is this
/// codebase in", not a file inventory.
const LANGUAGES: &[(&str, &str)] = &[
    ("rs", "Rust"),
    ("ts", "TypeScript"),
    ("tsx", "TypeScript"),
    ("js", "JavaScript"),
    ("jsx", "JavaScript"),
    ("mjs", "JavaScript"),
    ("cjs", "JavaScript"),
    ("py", "Python"),
    ("go", "Go"),
    ("java", "Java"),
    ("kt", "Kotlin"),
    ("rb", "Ruby"),
    ("c", "C"),
    ("h", "C"),
    ("cc", "C++"),
    ("cpp", "C++"),
    ("hpp", "C++"),
    ("cs", "C#"),
    ("swift", "Swift"),
];

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
        facts.mcp_menu = self.mcp_menu.clone();
        facts.lsp_menu = self.lsp_menu.clone();
        facts
    }
}

/// Scans `root` into a deterministic [`KnownFacts`] snapshot.
///
/// Infallible by design: every failure degrades to less information.
#[must_use]
pub fn scan(root: &Path) -> KnownFacts {
    let mut languages: BTreeMap<String, usize> = BTreeMap::new();
    let mut source_dirs: BTreeSet<String> = BTreeSet::new();
    let mut test_dirs: BTreeSet<String> = BTreeSet::new();
    let mut config_files: BTreeSet<String> = BTreeSet::new();
    let mut build_systems: BTreeSet<String> = BTreeSet::new();
    let mut tree = TreeNode::default();

    let walker = WalkBuilder::new(root)
        .max_depth(Some(MAX_DEPTH))
        .hidden(true)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .build();

    for (index, entry) in walker.enumerate() {
        if index >= MAX_ENTRIES {
            break;
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = relative(root, path);
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            match name {
                "src" => {
                    source_dirs.insert(rel.clone());
                }
                "tests" | "test" | "__tests__" => {
                    test_dirs.insert(rel.clone());
                }
                _ => {}
            }
        } else if let Some(system) = BUILD_SYSTEMS
            .iter()
            .find(|(file, _)| *file == name)
            .map(|(_, system)| *system)
        {
            build_systems.insert(system.to_owned());
            config_files.insert(rel.clone());
        } else if EXTRA_CONFIG_FILES.contains(&name) {
            config_files.insert(rel.clone());
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && let Some(language) = LANGUAGES
                .iter()
                .find(|(suffix, _)| *suffix == ext)
                .map(|(_, language)| *language)
        {
            *languages.entry(language.to_owned()).or_default() += 1;
        }
        // The rendered tree is shallower than the walk and skips the root.
        if entry.depth() >= 1 && entry.depth() <= TREE_DEPTH && rel != "." {
            tree.insert(&rel, is_dir);
        }
    }

    let rendered = tree.render();
    let total_entries = rendered.len();
    let mut tree_lines: Vec<String> = rendered.into_iter().take(TREE_MAX_LINES).collect();
    if total_entries > TREE_MAX_LINES {
        // One line per entry: the marker states exactly what is omitted.
        tree_lines.push(format!(
            "(+{} more entries)",
            total_entries - TREE_MAX_LINES
        ));
    }

    KnownFacts::new(
        root.to_path_buf(),
        ranked_languages(&languages),
        build_systems.iter().cloned().collect(),
        source_dirs.into_iter().collect(),
        test_dirs.into_iter().collect(),
        config_files.into_iter().collect(),
        toolchain_versions(&build_systems),
        tree_lines,
        read_instructions(root),
        Vec::new(),
        git_state(root),
        None,
        None,
    )
}

/// Indexes `.agents/skills/*.md` (iteration 5): name = file stem,
/// description = first non-empty line stripped of heading syntax, bounded.
///
/// Infallible like the rest of the scan: a missing or unreadable directory
/// degrades to an empty index, which renders nothing. Two file shapes
/// count: flat `<name>.md` and `<name>/SKILL.md` (Agent Skills
/// convention) — the flat file wins a same-name collision because it is
/// what `load_skill` serves first.
pub(crate) fn scan_skills(root: &Path) -> Vec<SkillIndexEntry> {
    let dir = SKILLS_DIR
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut flat: Vec<SkillIndexEntry> = Vec::new();
    let mut nested: Vec<SkillIndexEntry> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // A stem that cannot be typed as a `load_skill` argument adds
            // noise, not capability.
            if stem.is_empty() {
                continue;
            }
            if let Some(index_entry) = skill_index_entry(&path, stem) {
                flat.push(index_entry);
            }
        } else if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            if let Some(index_entry) = skill_index_entry(&skill_md, name) {
                nested.push(index_entry);
            }
        }
    }
    let flat_names: BTreeSet<String> = flat.iter().map(|e| e.name.clone()).collect();
    let mut skills = flat;
    skills.extend(nested.into_iter().filter(|e| !flat_names.contains(&e.name)));
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.truncate(SKILLS_MAX_FILES);
    skills
}

/// Reads `.loom/lessons.json` (roadmap item 20.4): the evidence-gated
/// lessons `loom lessons` mined from stored transcripts.
///
/// Infallible like the rest of the scan: a missing file, unreadable
/// JSON, or an entry that no longer clears the miner's thresholds
/// degrades to fewer lessons, never to a failed task setup. Entries are
/// re-validated on read (known failure kind, bounded non-empty statement,
/// `is_live()`), name-ordered, deduped by (tool, failure), and capped.
pub(crate) fn scan_lessons(root: &Path) -> Vec<LessonIndexEntry> {
    let path = LESSONS_FILE
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(mined) = serde_json::from_str::<Vec<MinedLesson>>(&body) else {
        return Vec::new();
    };
    let mut valid: Vec<MinedLesson> = mined
        .into_iter()
        .filter(|lesson| {
            !lesson.tool.is_empty()
                && lessons::is_known_failure(&lesson.failure)
                && !lesson.statement.trim().is_empty()
                && lesson.statement.chars().count() <= LESSON_STATEMENT_MAX_CHARS
                && lesson.is_live()
        })
        .collect();
    valid.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.failure.cmp(&b.failure)));
    valid.dedup_by(|a, b| a.tool == b.tool && a.failure == b.failure);
    valid.truncate(LESSONS_MAX_ENTRIES);
    valid
        .into_iter()
        .map(|lesson| {
            let evidence = lesson.evidence();
            LessonIndexEntry::new(lesson.statement, evidence)
        })
        .collect()
}

/// Builds one menu entry from a skill file's bounded head: frontmatter
/// `description` when present, else the first non-empty body line
/// stripped of heading markers. A frontmatter `name` that disagrees
/// with the file stem keeps the skill off the menu — mislabeled
/// metadata must not become a silent alias for a different name.
fn skill_index_entry(path: &Path, stem: &str) -> Option<SkillIndexEntry> {
    let Ok(bytes) = std::fs::read(path) else {
        // Listed but unreadable: an empty description keeps the menu
        // honest about the shape without failing the scan.
        return Some(SkillIndexEntry::new(stem.to_owned(), String::new()));
    };
    let head = truncate_utf8_head(&bytes, SKILL_DESCRIPTION_READ_BYTES);
    let (frontmatter, body) = split_frontmatter(&head);
    if let Some(declared) = &frontmatter.name
        && declared != stem
    {
        return None;
    }
    let description = match &frontmatter.description {
        Some(desc) => desc.clone(),
        None => body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_owned(),
    };
    Some(SkillIndexEntry::new(
        stem.to_owned(),
        truncate_utf8_head(description.as_bytes(), SKILL_DESCRIPTION_MAX_BYTES),
    ))
}

/// Orders detected languages by frequency, ties by name, for a stable fact.
fn ranked_languages(counts: &BTreeMap<String, usize>) -> Vec<String> {
    let mut ranked: Vec<(&String, &usize)> = counts.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    ranked.into_iter().map(|(name, _)| name.clone()).collect()
}

/// A bounded, sorted directory tree accumulated during the walk.
#[derive(Default)]
struct TreeNode {
    dirs: BTreeMap<String, TreeNode>,
    files: BTreeSet<String>,
}

impl TreeNode {
    /// Inserts a workspace-relative path (slash-separated).
    fn insert(&mut self, rel: &str, is_dir: bool) {
        let mut node = self;
        let mut rest = rel;
        loop {
            match rest.split_once('/') {
                Some((head, tail)) => {
                    node = node.dirs.entry(head.to_owned()).or_default();
                    rest = tail;
                }
                None => {
                    if is_dir {
                        node.dirs.entry(rest.to_owned()).or_default();
                    } else {
                        node.files.insert(rest.to_owned());
                    }
                    break;
                }
            }
        }
    }

    /// Renders every entry as an indented line (directories carry `/`),
    /// children sorted by name. The result is bounded by the walk itself
    /// (depth and entry caps); callers cap the line count.
    fn render(&self) -> Vec<String> {
        let mut lines = Vec::new();
        self.render_node(0, &mut lines);
        lines
    }

    fn render_node(&self, depth: usize, lines: &mut Vec<String>) {
        let indent = "  ".repeat(depth);
        let mut names: Vec<(&str, bool)> = self
            .dirs
            .keys()
            .map(|name| (name.as_str(), true))
            .chain(self.files.iter().map(|name| (name.as_str(), false)))
            .collect();
        names.sort();
        for (name, is_dir) in names {
            if is_dir {
                lines.push(format!("{indent}{name}/"));
                if let Some(child) = self.dirs.get(name) {
                    child.render_node(depth + 1, lines);
                }
            } else {
                lines.push(format!("{indent}{name}"));
            }
        }
    }
}

/// Reads the first well-known project instruction file under `root`.
fn read_instructions(root: &Path) -> Option<InstructionFile> {
    for name in INSTRUCTION_FILES {
        let path = root.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let total = bytes.len();
        let text = truncate_utf8_head(&bytes, INSTRUCTIONS_MAX_BYTES);
        return Some(InstructionFile::new(name.to_owned(), total, text));
    }
    None
}

/// Head of `bytes`, cut on a UTF-8 character boundary.
fn truncate_utf8_head(bytes: &[u8], cap: usize) -> String {
    let mut end = bytes.len().min(cap);
    // Only when a cut happens can `bytes[end]` land mid-character.
    while end > 0 && end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Probes toolchain versions for the detected build systems, in
/// [`TOOLCHAIN_PROBES`] order. Each probe degrades to nothing on failure.
fn toolchain_versions(build_systems: &BTreeSet<String>) -> Vec<String> {
    TOOLCHAIN_PROBES
        .iter()
        .filter(|(system, _, _)| build_systems.contains(*system))
        .filter_map(|(_, program, args)| run_version(program, args))
        .collect()
}

/// Runs `<program> <args>` and returns the first stdout line (e.g.
/// `cargo 1.89.0 (...)`), or `None` on any failure.
fn run_version(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()?
        .trim()
        .to_owned();
    (!line.is_empty()).then_some(line)
}

/// Workspace-relative, slash-separated path (the root itself maps to `.`).
fn relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Reads branch and changed-file state from git, degrading to empty on any
/// failure (including `git` not being installed).
fn git_state(root: &Path) -> GitState {
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|out| out.trim().to_owned())
        .filter(|branch| !branch.is_empty());
    let changed_files = run_git(root, &["status", "--porcelain"])
        .map(|out| parse_porcelain(&out))
        .unwrap_or_default();
    GitState::new(branch, changed_files)
}

/// Runs `git -C <root> <args>` and returns stdout on success.
fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        // Absolute paths and no quoting ambiguity in `status --porcelain`.
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Extracts changed paths from `git status --porcelain` output.
///
/// Handles the rename form (`R  old -> new`), taking the destination, and
/// sorts/dedupes for determinism. Paths that git quotes (they contain `"`,
/// `\`, or control bytes — `core.quotepath=false` only stops *non-ASCII*
/// being quoted) are C-unquoted, so quoted and unquoted spellings of the
/// same path land on one entry.
fn parse_porcelain(output: &str) -> Vec<String> {
    let mut paths: Vec<String> = output
        .lines()
        .filter_map(|line| {
            // `XY <path>`: the status is two fixed columns then a space.
            let rest = line.get(3..)?;
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let path = unquote_git_path(path.trim());
            (!path.is_empty()).then_some(path)
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Decodes one git-quoted path: `"…"` with C escapes (`\t`, `\n`, `\r`,
/// `\"`, `\\`, `\NNN` octal naming raw bytes). Unquoted input passes through
/// unchanged. Octal escapes are bytes, not code points, so the result is
/// re-decoded as UTF-8; invalid sequences (from raw non-UTF-8 paths)
/// degrade via lossy conversion rather than a wrong path.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_owned();
    };
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    let mut buf = [0_u8; 4];
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('a') => out.push(0x07),
            Some('b') => out.push(0x08),
            Some('f') => out.push(0x0c),
            Some('v') => out.push(0x0b),
            Some('"') => out.push(b'"'),
            Some('\\') => out.push(b'\\'),
            Some(first @ '0'..='7') => {
                // Exactly three octal digits name one raw byte.
                let mut value = first.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.next().and_then(|d| d.to_digit(8)) {
                        Some(digit) => value = value * 8 + digit,
                        None => break,
                    }
                }
                out.push(value as u8);
            }
            Some(other) => out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes()),
            None => out.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    fn scan_lessons_reads_a_miner_store() {
        let dir = tempdir();
        let root = dir.path();
        let loom = root.join(".loom");
        fs::create_dir_all(&loom).unwrap();
        let store = vec![loom_kernel::lessons::MinedLesson::new(
            "edit_file",
            "invalid_input",
            "For edit_file invalid-input failures: correct the arguments and reissue.",
            3,
            3,
            3,
        )];
        fs::write(
            loom.join("lessons.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();

        let lessons = scan_lessons(root);
        assert_eq!(lessons.len(), 1);
        assert!(lessons[0].statement.contains("edit_file"));
        assert_eq!(lessons[0].evidence, "3 failures, 3 recovered, 3 sessions");
    }

    #[test]
    fn scan_lessons_degrades_cleanly_and_validates() {
        let dir = tempdir();
        let root = dir.path();
        // Missing store: empty.
        assert!(scan_lessons(root).is_empty());

        let loom = root.join(".loom");
        fs::create_dir_all(&loom).unwrap();
        // Corrupt JSON: empty.
        fs::write(loom.join("lessons.json"), "not json").unwrap();
        assert!(scan_lessons(root).is_empty());

        // Dead entry (below thresholds) and unknown failure kind are
        // dropped; the live entry survives.
        let store = vec![
            loom_kernel::lessons::MinedLesson::new("edit_file", "invalid_input", "dead", 1, 0, 1),
            loom_kernel::lessons::MinedLesson::new("edit_file", "bogus", "bogus kind", 9, 9, 9),
            loom_kernel::lessons::MinedLesson::new("read_file", "timeout", "live", 4, 3, 3),
        ];
        fs::write(
            loom.join("lessons.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();
        let lessons = scan_lessons(root);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].statement, "live");
    }

    #[test]
    fn scan_lessons_name_orders_and_caps() {
        let dir = tempdir();
        let root = dir.path();
        let loom = root.join(".loom");
        fs::create_dir_all(&loom).unwrap();
        let mut store = Vec::new();
        for i in (0..(LESSONS_MAX_ENTRIES + 5)).rev() {
            store.push(loom_kernel::lessons::MinedLesson::new(
                format!("tool{i:03}"),
                "invalid_input",
                format!("Rule {i}."),
                3,
                3,
                3,
            ));
        }
        fs::write(
            loom.join("lessons.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();
        let lessons = scan_lessons(root);
        assert_eq!(lessons.len(), LESSONS_MAX_ENTRIES);
        assert_eq!(lessons[0].statement, "Rule 0.");
        assert_eq!(
            lessons.last().map(|l| l.statement.as_str()),
            Some("Rule 63.")
        );
    }

    #[test]
    fn facts_source_injects_lessons_only_when_opted_in() {
        let dir = tempdir();
        let root = dir.path();
        let loom = root.join(".loom");
        fs::create_dir_all(&loom).unwrap();
        let store = vec![loom_kernel::lessons::MinedLesson::new(
            "edit_file",
            "invalid_input",
            "rule.",
            3,
            3,
            3,
        )];
        fs::write(
            loom.join("lessons.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();

        // Default source: the frozen protocol is unchanged — no lessons.
        let default = FsFactsSource::new(root).scan();
        assert!(default.lessons.is_empty());

        // Opted in: the store rides the block.
        let opted = FsFactsSource::new(root).with_lessons(true).scan();
        assert_eq!(opted.lessons.len(), 1);
        assert!(opted.render().contains("lessons (evidence-gated rules"));
    }

    #[test]
    fn scan_skills_reads_flat_and_dir_shapes() {
        let dir = tempdir();
        let root = dir.path();
        let skills = root.join(".agents").join("skills");
        fs::create_dir_all(skills.join("triage")).unwrap();
        fs::write(skills.join("release.md"), "# Release checklist\n").unwrap();
        fs::write(
            skills.join("triage").join("SKILL.md"),
            "---\nname: triage\ndescription: sort incoming issues\n---\nBody.\n",
        )
        .unwrap();

        let entries = scan_skills(root);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "release");
        assert!(entries[0].description.contains("Release checklist"));
        assert_eq!(entries[1].name, "triage");
        // Frontmatter description feeds the menu one-liner.
        assert_eq!(entries[1].description, "sort incoming issues");
    }

    #[test]
    fn scan_skills_rejects_mismatched_frontmatter_names() {
        let dir = tempdir();
        let root = dir.path();
        let skills = root.join(".agents").join("skills");
        fs::create_dir_all(skills.join("triage")).unwrap();
        fs::write(
            skills.join("triage").join("SKILL.md"),
            "---\nname: totally-different\n---\nBody.\n",
        )
        .unwrap();

        assert!(scan_skills(root).is_empty());
    }

    #[test]
    fn scan_skills_flat_wins_over_dir() {
        let dir = tempdir();
        let root = dir.path();
        let skills = root.join(".agents").join("skills");
        fs::create_dir_all(skills.join("both")).unwrap();
        fs::write(skills.join("both.md"), "flat").unwrap();
        fs::write(skills.join("both").join("SKILL.md"), "dir").unwrap();

        let entries = scan_skills(root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "both");
        assert_eq!(entries[0].description, "flat");
    }

    #[test]
    fn scan_detects_structure() {
        let dir = tempdir();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("tests/it.rs"), "#[test]\nfn t() {}\n").unwrap();

        let facts = scan(root);
        assert_eq!(facts.languages, vec!["Rust".to_owned()]);
        assert_eq!(facts.build_systems, vec!["Cargo".to_owned()]);
        assert_eq!(facts.source_dirs, vec!["src".to_owned()]);
        assert_eq!(facts.test_dirs, vec!["tests".to_owned()]);
        assert_eq!(facts.config_files, vec!["Cargo.toml".to_owned()]);
    }

    #[test]
    fn scan_surfaces_instructions_tree_and_toolchains() {
        let dir = tempdir();
        let root = dir.path();
        fs::create_dir_all(root.join("src/nested/deeper/still")).unwrap();
        fs::write(root.join("AGENTS.md"), "Run `cargo test -p x` first.\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("src/nested/mod.rs"), "").unwrap();
        fs::write(root.join("src/nested/deeper/leaf.rs"), "").unwrap();
        // Below TREE_DEPTH: walked for languages, never rendered in the tree.
        fs::write(root.join("src/nested/deeper/still/hidden.rs"), "").unwrap();

        let facts = scan(root);
        let instructions = facts.instructions.expect("AGENTS.md surfaces");
        assert_eq!(instructions.path, "AGENTS.md");
        assert_eq!(instructions.total_bytes, 29);
        assert_eq!(instructions.text, "Run `cargo test -p x` first.\n");
        assert_eq!(
            facts.tree,
            vec![
                "AGENTS.md".to_owned(),
                "Cargo.toml".to_owned(),
                "src/".to_owned(),
                "  lib.rs".to_owned(),
                "  nested/".to_owned(),
                "    deeper/".to_owned(),
                "      leaf.rs".to_owned(),
                "      still/".to_owned(),
                "    mod.rs".to_owned(),
            ]
        );
        if Command::new("cargo").arg("--version").output().is_ok() {
            assert!(
                facts.toolchains.iter().any(|v| v.starts_with("cargo ")),
                "{:?}",
                facts.toolchains
            );
        }
    }

    #[test]
    fn claude_md_is_the_fallback_instructions_file() {
        let dir = tempdir();
        fs::write(dir.path().join("CLAUDE.md"), "claude rules\n").unwrap();
        let facts = scan(dir.path());
        assert_eq!(
            facts.instructions.expect("CLAUDE.md surfaces").path,
            "CLAUDE.md"
        );
    }

    #[test]
    fn instructions_are_head_bounded() {
        let dir = tempdir();
        let body = "x".repeat(10_000);
        fs::write(dir.path().join("AGENTS.md"), &body).unwrap();
        let facts = scan(dir.path());
        let instructions = facts.instructions.expect("instructions");
        assert_eq!(instructions.total_bytes, 10_000);
        assert_eq!(instructions.text.len(), INSTRUCTIONS_MAX_BYTES);
    }

    #[test]
    fn tree_cap_appends_a_marker() {
        let dir = tempdir();
        let root = dir.path();
        for i in 0..(TREE_MAX_LINES + 10) {
            fs::write(root.join(format!("file{i:03}.txt")), "").unwrap();
        }
        let facts = scan(root);
        assert_eq!(facts.tree.len(), TREE_MAX_LINES + 1);
        assert_eq!(
            facts.tree.last().map(String::as_str),
            Some("(+10 more entries)")
        );
    }

    #[test]
    fn utf8_head_cuts_on_a_character_boundary() {
        // `é` is two bytes; cutting between them would be invalid UTF-8.
        let bytes = "é".repeat(100).into_bytes();
        let head = truncate_utf8_head(&bytes, 4097);
        assert!(head.len() <= 4096);
        assert!(head.chars().all(|c| c == 'é'));
    }

    #[test]
    fn scan_is_deterministic() {
        let dir = tempdir();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "").unwrap();
        assert_eq!(scan(root), scan(root));
    }

    #[test]
    fn non_git_workspace_degrades_cleanly() {
        let dir = tempdir();
        fs::write(dir.path().join("main.go"), "package main\n").unwrap();
        let facts = scan(dir.path());
        assert_eq!(facts.languages, vec!["Go".to_owned()]);
        assert!(facts.git.branch.is_none());
        assert!(facts.git.changed_files.is_empty());
    }

    #[test]
    fn git_state_reports_branch_and_changes() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = tempdir();
        let root = dir.path();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("run git");
            assert!(status.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(root.join("tracked.txt"), "hello\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-qm", "init"]);
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();

        let facts = scan(root);
        assert_eq!(facts.git.branch.as_deref(), Some("main"));
        assert_eq!(facts.git.changed_files, vec!["tracked.txt".to_owned()]);
    }

    #[test]
    fn porcelain_parses_renames_and_sorts() {
        let out = " M b.txt\nR  old.txt -> new.txt\n?? a.txt\n";
        assert_eq!(
            parse_porcelain(out),
            vec!["a.txt".to_owned(), "b.txt".to_owned(), "new.txt".to_owned()]
        );
    }

    #[test]
    fn porcelain_unquotes_escaped_paths() {
        // git quotes paths containing `"`, `\`, or control bytes even with
        // core.quotepath=false; octal escapes appear for raw bytes.
        let out = "?? \"a\\tb.txt\"\n M \"quote\\\"d.txt\"\n?? \"caf\\303\\251.txt\"\n";
        assert_eq!(
            parse_porcelain(out),
            vec![
                "a\tb.txt".to_owned(),
                "café.txt".to_owned(),
                "quote\"d.txt".to_owned(),
            ]
        );
    }

    #[test]
    fn porcelain_unquotes_rename_destination() {
        let out = "R  \"old\\tname.txt\" -> \"new\\tname.txt\"\n";
        assert_eq!(parse_porcelain(out), vec!["new\tname.txt".to_owned()]);
    }

    #[test]
    fn rankings_break_ties_by_name() {
        let mut counts = BTreeMap::new();
        counts.insert("Rust".to_owned(), 2);
        counts.insert("Go".to_owned(), 2);
        counts.insert("Python".to_owned(), 5);
        assert_eq!(
            ranked_languages(&counts),
            vec!["Python".to_owned(), "Go".to_owned(), "Rust".to_owned()]
        );
    }
}
