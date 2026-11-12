//! The workspace walk: one bounded, deterministic pass over the tree
//! that classifies entries and assembles the [`KnownFacts`] snapshot
//! (ADR-015), plus the small pure helpers it shares.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use ignore::WalkBuilder;
use loom_kernel::context::{InstructionFile, KnownFacts};

use super::consts::{
    BUILD_SYSTEMS, EXTRA_CONFIG_FILES, INSTRUCTION_FILES, INSTRUCTIONS_MAX_BYTES, LANGUAGES,
    MAX_DEPTH, MAX_ENTRIES, TOOLCHAIN_PROBES, TREE_DEPTH, TREE_MAX_LINES,
};
use super::git::git_state;

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

/// Orders detected languages by frequency, ties by name, for a stable fact.
pub(super) fn ranked_languages(counts: &BTreeMap<String, usize>) -> Vec<String> {
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
pub(super) fn truncate_utf8_head(bytes: &[u8], cap: usize) -> String {
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
