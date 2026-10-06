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
use loom_kernel::context::{FactsSource, GitState, KnownFacts};

/// How deep into the tree the walk descends. Enough to reach
/// `crates/<crate>/src` and `crates/<crate>/tests` in a monorepo, bounded so a
/// pathological tree cannot stall the scan.
const MAX_DEPTH: usize = 4;

/// Hard cap on entries visited, a backstop against enormous trees.
const MAX_ENTRIES: usize = 20_000;

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
}

impl FsFactsSource {
    /// Creates a source that scans `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FactsSource for FsFactsSource {
    fn scan(&self) -> KnownFacts {
        scan(&self.root)
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
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            match name {
                "src" => {
                    source_dirs.insert(rel.clone());
                }
                "tests" | "test" | "__tests__" => {
                    test_dirs.insert(rel.clone());
                }
                _ => {}
            }
            continue;
        }
        if let Some(system) = BUILD_SYSTEMS
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
    }

    KnownFacts::new(
        root.to_path_buf(),
        ranked_languages(&languages),
        build_systems.into_iter().collect(),
        source_dirs.into_iter().collect(),
        test_dirs.into_iter().collect(),
        config_files.into_iter().collect(),
        git_state(root),
    )
}

/// Orders detected languages by frequency, ties by name, for a stable fact.
fn ranked_languages(counts: &BTreeMap<String, usize>) -> Vec<String> {
    let mut ranked: Vec<(&String, &usize)> = counts.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    ranked.into_iter().map(|(name, _)| name.clone()).collect()
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
/// sorts/dedupes for determinism.
fn parse_porcelain(output: &str) -> Vec<String> {
    let mut paths: Vec<String> = output
        .lines()
        .filter_map(|line| {
            // `XY <path>`: the status is two fixed columns then a space.
            let rest = line.get(3..)?;
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let path = path.trim().trim_matches('"');
            (!path.is_empty()).then(|| path.to_owned())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
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
