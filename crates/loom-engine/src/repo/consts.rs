//! The scan's bounded knobs and lookup tables: depth and entry caps,
//! rendered-list budgets, and the file-name tables that classify what
//! the walk finds (ADR-015).

/// How deep into the tree the walk descends. Enough to reach
/// `crates/<crate>/src` and `crates/<crate>/tests` in a monorepo, bounded so a
/// pathological tree cannot stall the scan.
pub(super) const MAX_DEPTH: usize = 4;

/// Depth of the rendered file tree (shallower than the walk: the tree is a
/// map, not an inventory). Four levels reach `crates/<crate>/src/<file>` in
/// a monorepo; shallow repos are unaffected.
pub(super) const TREE_DEPTH: usize = 4;

/// Hard cap on entries visited, a backstop against enormous trees.
pub(super) const MAX_ENTRIES: usize = 20_000;

/// Hard cap on rendered tree lines; the remainder is summarised by a
/// `(+N more entries)` marker so the block stays bounded.
pub(super) const TREE_MAX_LINES: usize = 200;

/// Project instruction files, in preference order.
pub(super) const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Head of an instruction file kept in the facts block.
pub(super) const INSTRUCTIONS_MAX_BYTES: usize = 4096;

/// Directory scanned for the on-demand skills index (iteration 5).
pub(super) const SKILLS_DIR: &[&str] = &[".agents", "skills"];

/// The lessons store mined by `loom lessons` (roadmap item 20.4).
pub(super) const LESSONS_FILE: &[&str] = &[".loom", "lessons.json"];

/// Cap on lesson entries read from the store; the render caps again at
/// the list level (mirrors the skills index two caps).
pub(super) const LESSONS_MAX_ENTRIES: usize = 64;

/// Longest lesson statement the store scan will inject — a runaway file
/// costs one bounded line, not the context.
pub(super) const LESSON_STATEMENT_MAX_CHARS: usize = 512;

/// Cap on indexed skill files; the render caps again at the list level.
pub(super) const SKILLS_MAX_FILES: usize = 64;

/// Head of a skill file read when deriving its one-line description.
pub(super) const SKILL_DESCRIPTION_READ_BYTES: usize = 1024;

/// Byte budget for a rendered skill description.
pub(super) const SKILL_DESCRIPTION_MAX_BYTES: usize = 120;

/// Toolchain probes per detected build system, in deterministic order.
/// Each entry is `(build system, program, args, human prefix)` — the
/// rendered line is the trimmed stdout verbatim (e.g. `cargo 1.89.0`).
pub(super) const TOOLCHAIN_PROBES: &[(&str, &str, &[&str])] = &[
    ("Cargo", "cargo", &["--version"]),
    ("Cargo", "rustc", &["--version"]),
    ("npm", "node", &["--version"]),
    ("npm", "npm", &["--version"]),
    ("Python", "python3", &["--version"]),
    ("Go", "go", &["version"]),
];

/// Files whose presence identifies a build system, and the system they imply.
pub(super) const BUILD_SYSTEMS: &[(&str, &str)] = &[
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
pub(super) const EXTRA_CONFIG_FILES: &[&str] = &[
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
pub(super) const LANGUAGES: &[(&str, &str)] = &[
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
