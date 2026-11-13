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

mod consts;
mod git;
mod index;
mod scan;
mod source;

pub use scan::scan;
pub use source::FsFactsSource;
// Preserved crate-internal paths (`crate::repo::scan_skills` /
// `crate::repo::scan_lessons`); nothing outside `repo` uses them today.
#[allow(unused_imports)]
pub(crate) use index::{scan_lessons, scan_skills};

#[cfg(test)]
use consts::{INSTRUCTIONS_MAX_BYTES, LESSONS_MAX_ENTRIES, TREE_MAX_LINES};
#[cfg(test)]
use git::{git_state, parse_porcelain, run_git};
#[cfg(test)]
use loom_kernel::context::FactsSource;
#[cfg(test)]
use scan::{ranked_languages, truncate_utf8_head};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::process::Command;
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
        assert_eq!(scan_lessons(root).len(), 0);

        let loom = root.join(".loom");
        fs::create_dir_all(&loom).unwrap();
        // Corrupt JSON: empty.
        fs::write(loom.join("lessons.json"), "not json").unwrap();
        assert_eq!(scan_lessons(root).len(), 0);

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
        assert_eq!(default.lessons.len(), 0);

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

        assert_eq!(scan_skills(root).len(), 0);
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
        assert_eq!(facts.git.changed_files.len(), 0);
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

    /// F-03 regression: `.git/config` naming a program (`core.fsmonitor`
    /// spawns on every status) must not execute during loom's own facts
    /// scan — the host-git helpers override the knob (see `hostgit`).
    #[test]
    #[cfg(unix)]
    fn git_state_ignores_a_planted_fsmonitor() {
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

        // Plant the fsmonitor: a script whose whole job is to leave a
        // probe behind when git executes it.
        let probe = dir.path().join("fsmonitor-ran");
        let monitor = dir.path().join("fake-fsmonitor.sh");
        fs::write(&monitor, format!("#!/bin/sh\ntouch {}\n", probe.display())).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&monitor, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&[
            "config",
            "core.fsmonitor",
            monitor.to_str().expect("utf8 path"),
        ]);

        let state = git_state(root);
        assert!(state.branch.is_some(), "hardened status still works");
        assert!(
            !probe.exists(),
            "planted core.fsmonitor executed during loom's facts scan"
        );
    }

    /// The PR2 residual: repo config naming `filter.*` used to
    /// execute on content-rewriting operations even with the other
    /// hardening in place. The neutralizer probe turns every
    /// repo-configured filter into an identity pass-through, so a
    /// planted clean filter never runs — and the staged content is
    /// the file as written.
    #[test]
    fn git_add_ignores_a_planted_filter() {
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
        fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git(&["add", "seed.txt"]);
        git(&["commit", "-qm", "init"]);

        // Plant a clean filter whose whole job is to leave a probe
        // behind (and rewrite content if it ever runs).
        let probe = dir.path().join("filter-ran");
        let filter = dir.path().join("fake-filter.sh");
        fs::write(
            &filter,
            format!(
                "#!/bin/sh\ntouch {}\nsed s/payload/PWNED/g\n",
                probe.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&[
            "config",
            "filter.evil.clean",
            filter.to_str().expect("utf8 path"),
        ]);
        git(&["config", "filter.evil.required", "true"]);
        fs::write(root.join(".gitattributes"), "* filter=evil\n").unwrap();
        fs::write(root.join("payload.txt"), "payload line\n").unwrap();

        // The hardened path: run_git neutralizes filter.* before the
        // content-rewriting operation runs.
        run_git(root, &["add", "-A"]).expect("hardened add");
        assert!(
            !probe.exists(),
            "planted filter.evil.clean executed during loom's git add"
        );
        let staged = run_git(root, &["show", ":payload.txt"]).expect("staged blob");
        assert_eq!(staged.trim_end(), "payload line", "content was filtered");
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
