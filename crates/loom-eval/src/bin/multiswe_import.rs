//! multiswe-import — bridges Multi-SWE-bench instances into loom's
//! eval-task shape (efficiency-eval phase 2, recommendation 5: the
//! external anchor).
//!
//! The mined corpus (B-tasks, NAV/BIG/MULTI/LONG) measures loom against
//! its own history; an external, independently-graded corpus is what
//! makes loom's numbers comparable to other harnesses. Multi-SWE-bench
//! (arXiv 2504.02605) ships 1 680+ Rust/Java/Go/C++ instances across 25
//! real repositories (6.7k–698.6k LOC) with fail_to_pass/pass_to_pass
//! lists — the same gate shape loom already grades with. The Rust slice
//! is the natural first import: same toolchain, same `cargo test`
//! grading, and the cross-file patches (>1 file is where resolve rates
//! collapse in the field) exercise exactly the multi-file axis the
//! mined corpus lacks.
//!
//! Usage (network required; nothing here runs in CI):
//!
//! ```text
//! cargo run -p loom-eval --bin multiswe-import -- \
//!     --jsonl multilingual-swe-bench_rust-verified.jsonl \
//!     --out crates/loom-eval/tests/eval-tasks \
//!     [--instance ID ... | --max N] [--repo-allowlist org/repo,...]
//! ```
//!
//! For every selected instance the importer:
//!
//! 1. materializes `https://github.com/<repo>` at `base_commit` into
//!    `<out>/EXT-<id>/workspace/` (depth-1 fetch of the bare sha —
//!    no later history exists for an agent to mine, no submodules),
//! 2. applies `test_patch` (the tests that grade the fix),
//! 3. writes `solution.patch` (the gold patch — provenance only, the
//!    gate never reads it),
//! 4. writes `task.json`: a `bugfix` gate with the instance's
//!    fail_to_pass/pass_to_pass lists, the touched test files as
//!    `protected`, `heavy: true` (real repos are long-horizon work and
//!    run only when named), and a prompt naming the issue text.
//!
//! Offline validation still applies: `bugfix_seeds_fail_their_gates`
//! runs the gate on every imported seed — if an instance's F2P tests
//! already pass at base (a known flaw class in older benchmarks;
//! SWE-Bench ProMax audited ~60 % of such cases as flawed tests), the
//! import is wrong, not the harness.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One instance synthesized into eval-task files, before any I/O.
struct Plan {
    instance_id: String,
    repo: String,
    base_commit: String,
    title: String,
    body: String,
    test_patch: String,
    gold_patch: String,
    fail_to_pass: Vec<String>,
    pass_to_pass: Vec<String>,
}

impl Plan {
    /// Parses one JSONL record. Two schemas occur in the wild: the
    /// classic SWE-bench shape (`repo` = `org/name`, `base_commit`,
    /// `patch`, `FAIL_TO_PASS`/`PASS_TO_PASS` arrays or JSON-encoded
    /// strings) and the HF `ByteDance-Seed/Multi-SWE-bench` release
    /// (`org` + bare `repo`, `base.sha`, `fix_patch`, `f2p_tests`/
    /// `p2p_tests` maps of test name → outcome, `title`/`body` issue
    /// text). Both parse; fields prefer the HF names.
    fn from_record(record: &Value) -> Option<Self> {
        // Classic test lists arrive either as arrays or as JSON-encoded
        // strings; accept both shapes.
        let list = |key: &str| -> Vec<String> {
            let Some(raw) = record.get(key) else {
                return Vec::new();
            };
            if let Some(array) = raw.as_array() {
                array
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            } else {
                raw.as_str()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or_default()
            }
        };
        // HF maps test name -> {"run","test","fix"} outcome; the key
        // set is the list.
        let dict_keys = |key: &str| -> Vec<String> {
            record
                .get(key)
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default()
        };
        let repo = match (
            record.get("org").and_then(Value::as_str),
            record.get("repo").and_then(Value::as_str),
        ) {
            (Some(org), Some(name)) if !name.contains('/') => format!("{org}/{name}"),
            (_, Some(slug)) => slug.to_owned(),
            (Some(_), None) | (None, None) => return None,
        };
        let fail_to_pass = dict_keys("f2p_tests");
        let pass_to_pass = dict_keys("p2p_tests");
        Some(Self {
            instance_id: record.get("instance_id")?.as_str()?.to_owned(),
            repo,
            base_commit: record
                .pointer("/base/sha")
                .and_then(Value::as_str)
                .or_else(|| record.get("base_commit").and_then(Value::as_str))?
                .to_owned(),
            title: record
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            body: record
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            test_patch: record
                .get("test_patch")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            gold_patch: record
                .get("fix_patch")
                .or_else(|| record.get("patch"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            fail_to_pass: if fail_to_pass.is_empty() {
                list("FAIL_TO_PASS")
            } else {
                fail_to_pass
            },
            pass_to_pass: if pass_to_pass.is_empty() {
                list("PASS_TO_PASS")
            } else {
                pass_to_pass
            },
        })
    }

    /// The task.json body for this instance.
    fn task_json(&self, language: &str) -> Value {
        // Protected files: every path the test patch touches, so the
        // grader can verify the model did not rewrite the tests. Parsed
        // from the unified diff headers, not trusted from metadata.
        let protected = diff_paths(&self.test_patch);
        let prompt = if self.title.is_empty() && self.body.is_empty() {
            format!(
                "The {language} repository in this workspace has a bug described in issue \
                 {instance_id}. Investigate the repository, find the fault the issue describes, \
                 and fix it. The fail_to_pass tests in the gate name the exact failing behavior; \
                 keep every other test green. Make the smallest correct change.",
                instance_id = self.instance_id,
            )
        } else {
            format!(
                "The {language} repository in this workspace has a bug reported upstream as \
                 \"{title}\".\n\n{body}\n\nInvestigate the repository, find the fault the issue \
                 describes, and fix it. The fail_to_pass tests in the gate name the exact \
                 failing behavior; keep every other test green. Make the smallest correct change.",
                title = self.title,
                body = clip(&self.body, 1_500),
            )
        };
        serde_json::json!({
            "id": format!("EXT-{}", self.instance_id),
            "prompt": prompt,
            "tools": "coding",
            "heavy": true,
            "tier": "l",
            "workspace": "copy",
            "gate": {
                "kind": "bugfix",
                "protected": protected,
                "fail_to_pass": self.fail_to_pass,
                "pass_to_pass": self.pass_to_pass,
            },
            "notes": format!(
                "imported from Multi-SWE-bench: repo {} @ {} (gold patch kept as solution.patch; grading = FAIL_TO_PASS/PASS_TO_PASS over cargo/pytest at seed)",
                self.repo, self.base_commit
            ),
        })
    }
}

/// File paths from a unified diff's `+++ b/...` headers (test-patch
/// scope). Falls back to `diff --git` lines when no hunks are present.
fn diff_paths(patch: &str) -> Vec<String> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            let path = rest.split_whitespace().next().unwrap_or(rest);
            if path != "/dev/null" {
                paths.insert(path.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/x b/x`
            if let Some((_, b)) = rest.split_once(' ')
                && let Some(path) = b.strip_prefix("b/")
            {
                paths.insert(path.to_owned());
            }
        }
    }
    paths.into_iter().collect()
}

/// Truncates `text` to at most `max` bytes on a UTF-8 char boundary.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Appends an empty `[workspace]` table to a root manifest that lacks
/// one, detaching the clone from any ancestor cargo workspace.
fn detach_workspace(manifest: &str) -> String {
    if manifest.lines().any(|line| line.trim() == "[workspace]") {
        manifest.to_owned()
    } else {
        format!(
            "{manifest}\n# detached from any parent cargo workspace by multiswe-import\n[workspace]\n"
        )
    }
}

fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{}: {error}", command.get_program().to_string_lossy()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{} failed: {}{}",
            command.get_program().to_string_lossy(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }
}

fn import(plan: &Plan, out: &Path) -> Result<PathBuf, String> {
    let task_dir = out.join(format!("EXT-{}", plan.instance_id));
    let workspace = task_dir.join("workspace");
    if workspace.exists() {
        return Err(format!(
            "{} already exists — remove it to re-import",
            workspace.display()
        ));
    }
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;

    let url = format!("https://github.com/{}", plan.repo);
    // Benchmark hygiene: fetch EXACTLY the base commit, nothing else.
    // A plain `git clone` would carry the full history — including the
    // commits after base that contain the actual fix — and an agent
    // with `run_command` could `git log --all` its way to the answer.
    // A depth-1 fetch of the bare sha leaves one commit and no refs
    // pointing at anything beyond it.
    run(Command::new("git").arg("init").arg(&workspace)).map_err(|e| format!("init {url}: {e}"))?;
    run(Command::new("git")
        .current_dir(&workspace)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(&url))
    .map_err(|e| format!("remote {url}: {e}"))?;
    run(Command::new("git")
        .current_dir(&workspace)
        .arg("fetch")
        .arg("--quiet")
        .arg("--depth=1")
        .arg("origin")
        .arg(&plan.base_commit))
    .map_err(|e| format!("fetch {url}@{}: {e}", plan.base_commit))?;
    run(Command::new("git")
        .current_dir(&workspace)
        .arg("checkout")
        .arg("--detach")
        .arg("FETCH_HEAD"))
    .map_err(|e| format!("checkout {}: {e}", plan.base_commit))?;
    if !plan.test_patch.is_empty() {
        // Apply the grading tests onto the worktree.
        let mut child = Command::new("git")
            .current_dir(&workspace)
            .arg("apply")
            .arg("--whitespace=nowarn")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("git apply: {e}"))?;
        use std::io::Write as _;
        // `Stdio::piped` above guarantees the handle; the failure this
        // guards is a programming error, not a runtime condition.
        #[allow(clippy::expect_used)]
        let stdin = child.stdin.as_mut().expect("piped stdin");
        stdin
            .write_all(plan.test_patch.as_bytes())
            .map_err(|e| format!("pipe test_patch: {e}"))?;
        let output = child
            .wait_with_output()
            .map_err(|e| format!("git apply: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "test_patch did not apply at {}: {}",
                plan.base_commit,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    // A root manifest without a `[workspace]` table makes cargo walk up
    // and adopt an ancestor workspace (loom's own), which refuses the
    // foreign package ("believes it's in a workspace when it's not").
    // Detach: an empty `[workspace]` table roots the clone.
    let manifest = workspace.join("Cargo.toml");
    if manifest.exists() {
        let content = std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?;
        let detached = detach_workspace(&content);
        if detached != content {
            std::fs::write(&manifest, detached).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(task_dir.join("solution.patch"), &plan.gold_patch).map_err(|e| e.to_string())?;
    let task = plan.task_json("upstream");
    std::fs::write(
        task_dir.join("task.json"),
        serde_json::to_string_pretty(&task).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(task_dir)
}

fn main() {
    let args = std::env::args().skip(1);
    let mut jsonl: Option<PathBuf> = None;
    let mut out = PathBuf::from("crates/loom-eval/tests/eval-tasks");
    let mut instances: Vec<String> = Vec::new();
    let mut max: usize = 0;
    let mut language = String::from("rust");
    let mut allowlist: Vec<String> = Vec::new();
    let rest: Vec<String> = args.collect();
    let mut cursor = 0usize;
    while let Some(arg) = rest.get(cursor) {
        let arg = arg.as_str();
        let mut value = || -> String {
            cursor += 1;
            rest.get(cursor).cloned().unwrap_or_default()
        };
        match arg {
            "--jsonl" => jsonl = Some(PathBuf::from(value())),
            "--out" => out = PathBuf::from(value()),
            "--instance" => instances.push(value()),
            "--max" => max = value().parse().unwrap_or(0),
            "--language" => language = value(),
            "--repo-allowlist" => {
                allowlist = value().split(',').map(|s| s.trim().to_owned()).collect()
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        cursor += 1;
    }
    let Some(jsonl) = jsonl else {
        eprintln!(
            "usage: multiswe-import --jsonl <file> [--out DIR] [--instance ID]... [--max N] [--language LANG] [--repo-allowlist org/repo,org/repo]"
        );
        std::process::exit(2);
    };

    let text = std::fs::read_to_string(&jsonl).unwrap_or_else(|e| {
        eprintln!("cannot read {}: {e}", jsonl.display());
        std::process::exit(1);
    });
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if max > 0 && imported >= max {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            skipped += 1;
            continue;
        };
        let Some(plan) = Plan::from_record(&record) else {
            skipped += 1;
            eprintln!("skip: record is not a Multi-SWE-bench instance");
            continue;
        };
        // A bugfix gate needs at least one failing test at seed;
        // records with no fail_to_pass cannot be graded and would
        // fail `bugfix_seeds_fail_their_gates` by construction.
        if plan.fail_to_pass.is_empty() {
            skipped += 1;
            eprintln!(
                "skip {}: no fail_to_pass tests — not gradeable as a bugfix",
                plan.instance_id
            );
            continue;
        }
        if !instances.is_empty() && !instances.contains(&plan.instance_id) {
            continue;
        }
        if !allowlist.is_empty() && !allowlist.contains(&plan.repo) {
            continue;
        }
        // Multi-SWE-bench tags languages per record; keep the requested
        // one unless the record carries no language field.
        if let Some(tag) = record.get("language").and_then(Value::as_str)
            && tag != language
        {
            continue;
        }
        match import(&plan, &out) {
            Ok(dir) => {
                imported += 1;
                println!("imported {} -> {}", plan.instance_id, dir.display());
            }
            Err(error) => {
                skipped += 1;
                eprintln!("skip {}: {error}", plan.instance_id);
            }
        }
    }
    println!("done: {imported} imported, {skipped} skipped");
    if imported == 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesizes_a_gated_task_from_a_hf_multiswe_record() {
        // The HF ByteDance-Seed/Multi-SWE-bench release shape: org +
        // bare repo, base.sha, fix_patch, f2p/p2p outcome maps, and
        // issue title/body for the prompt.
        let record = serde_json::json!({
            "instance_id": "demo__repo-1234",
            "org": "demo",
            "repo": "repo",
            "base": {"label": "demo:main", "ref": "main", "sha": "deadbeef"},
            "title": "widget drains twice",
            "body": "Draining an empty range yields stale items.",
            "fix_patch": "diff --git a/src/lib.rs b/src/lib.rs",
            "test_patch": "diff --git a/tests/it.rs b/tests/it.rs\n+++ b/tests/it.rs\n@@\n",
            "f2p_tests": {"drain_vec_empty_range_yielded": {"run": "NONE", "test": "FAIL", "fix": "PASS"}},
            "p2p_tests": {"smoke_stays_green": {"run": "PASS", "test": "PASS", "fix": "PASS"}},
        });
        let plan = Plan::from_record(&record).expect("record parses");
        assert_eq!(plan.repo, "demo/repo");
        assert_eq!(plan.base_commit, "deadbeef");
        assert_eq!(
            plan.fail_to_pass,
            vec!["drain_vec_empty_range_yielded".to_owned()]
        );
        assert_eq!(plan.pass_to_pass, vec!["smoke_stays_green".to_owned()]);

        let task = plan.task_json("rust");
        assert_eq!(task["id"], serde_json::json!("EXT-demo__repo-1234"));
        assert_eq!(task["heavy"], serde_json::json!(true));
        assert_eq!(task["tier"], serde_json::json!("l"));
        assert_eq!(task["gate"]["kind"], serde_json::json!("bugfix"));
        assert_eq!(
            task["gate"]["fail_to_pass"],
            serde_json::json!(["drain_vec_empty_range_yielded"])
        );
        assert_eq!(
            task["gate"]["protected"],
            serde_json::json!(["tests/it.rs"])
        );
        let prompt = task["prompt"].as_str().expect("prompt");
        assert!(
            prompt.contains("widget drains twice"),
            "prompt names the issue title"
        );
        assert!(
            task["notes"]
                .as_str()
                .expect("notes")
                .contains("demo/repo @ deadbeef")
        );
    }

    #[test]
    fn classic_swebench_records_still_parse() {
        // The classic SWE-bench JSONL shape stays importable.
        let record = serde_json::json!({
            "instance_id": "demo__repo-1234",
            "repo": "demo/repo",
            "base_commit": "deadbeef",
            "patch": "diff --git a/src/lib.rs b/src/lib.rs",
            "test_patch": "diff --git a/tests/it.rs b/tests/it.rs\n+++ b/tests/it.rs\n@@\n",
            "FAIL_TO_PASS": "[\"regression_fires\"]",
            "PASS_TO_PASS": "[\"smoke_stays_green\"]",
        });
        let plan = Plan::from_record(&record).expect("record parses");
        assert_eq!(plan.repo, "demo/repo");
        assert_eq!(plan.base_commit, "deadbeef");
        assert_eq!(plan.fail_to_pass, vec!["regression_fires".to_owned()]);
        assert_eq!(plan.pass_to_pass, vec!["smoke_stays_green".to_owned()]);
        // No title/body: the prompt falls back to the instance id.
        let task = plan.task_json("rust");
        assert!(
            task["prompt"]
                .as_str()
                .expect("prompt")
                .contains("demo__repo-1234")
        );
    }

    #[test]
    fn clip_cuts_on_a_char_boundary() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("héllo", 2), "h");
        assert_eq!(clip("", 4), "");
    }

    #[test]
    fn detach_workspace_appends_only_when_missing() {
        let plain = "[package]\nname = \"bytes\"\n";
        let detached = detach_workspace(plain);
        assert!(detached.contains("[workspace]"));
        assert!(detached.starts_with("[package]\nname = \"bytes\"\n"));
        // Already-rooted manifests are untouched.
        let rooted = "[workspace]\nmembers = [\"x\"]\n";
        assert_eq!(detach_workspace(rooted), rooted);
        // A commented-out table does not count.
        assert!(detach_workspace("# [workspace]\n").contains("\n[workspace]\n"));
    }

    #[test]
    fn diff_paths_reads_both_diff_headers() {
        let patch = "diff --git a/tests/a.rs b/tests/a.rs\n--- a/tests/a.rs\n+++ b/tests/a.rs\n@@ -1 +1 @@\n";
        assert_eq!(diff_paths(patch), vec!["tests/a.rs".to_owned()]);
        assert!(diff_paths("").is_empty());
    }
}
