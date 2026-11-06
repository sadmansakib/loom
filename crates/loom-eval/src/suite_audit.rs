//! Frozen-subset suite audit (roadmap item 1's 2026-11 tail, lit-review
//! shortlist #8): UTBoost (arXiv 2506.09289) found 345 erroneously-passing
//! SWE-bench patches — the official PASS_TO_PASS / FAIL_TO_PASS suites
//! under-discriminate for a nontrivial share of instances, so the frozen
//! subset's verdicts are hypotheses, not ground truth.
//!
//! Two deterministic passes, no model and no network:
//!
//! 1. **Structural audit** (offline, always): per-instance suite shape —
//!    F2P/P2P counts, gold-patch hunk count, whether the test patch adds
//!    test-looking lines — flagging instances whose suite is small or
//!    adds no tests (`suite_small`, `no_new_tests`). These flags are
//!    *hypotheses*, the same signal class UTBoost amplified.
//! 2. **Mutant re-grade** (docker, one-off): for every gold patch with
//!    more than one hunk, [`mutant_patches`] emits one single-hunk-dropped
//!    mutant per hunk. Run through the official scorer, an instance whose
//!    every mutant still resolves has a suite that cannot tell the gold
//!    patch from its own crippled variants — the weak-suite confirmation.
//!
//! The `suite-audit` binary writes the audit report and per-mutant
//! predictions files; the scorer loop itself is documented in the
//! emitted README because it costs docker-hours and belongs to the
//! one-off re-grade, not every sweep.

use crate::swebench::SweInstance;
use serde::Serialize;

/// One instance's suite shape. Counts only — deterministic and
/// reproducible from the dataset alone.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SuiteAuditRow {
    /// The dataset's canonical id (`org__repo-number`).
    pub instance_id: String,
    /// `org/repo` on GitHub.
    pub repo: String,
    /// `FAIL_TO_PASS` count.
    pub f2p: usize,
    /// `PASS_TO_PASS` count.
    pub p2p: usize,
    /// Hunks in the gold patch.
    pub gold_hunks: usize,
    /// Added lines in the test patch that look like tests
    /// (`test`, case-insensitive) — 0 means the suite grew nothing.
    pub added_test_lines: usize,
    /// `f2p + p2p <= 2`: a suite this small discriminates little.
    pub suite_small: bool,
    /// The test patch adds no test-looking lines.
    pub no_new_tests: bool,
    /// The gold patch has one hunk: no mutant exists, the mutant
    /// re-grade cannot speak to this instance either way.
    pub single_hunk: bool,
}

impl SuiteAuditRow {
    /// The weak-suite hypothesis flag: either structural signal fired.
    pub fn weak(&self) -> bool {
        self.suite_small || self.no_new_tests
    }
}

/// Provenance stamped into the audit report: the artifact is a
/// measurement of one dataset file under one method version, and a
/// weak-suite verdict is only citable with both (same rule as
/// [`crate::swebench::SweepProvenance`]).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SuiteAuditProvenance {
    /// Method marker — bump when any flag or the mutant rule changes,
    /// invalidating recorded audit verdicts.
    pub method_version: &'static str,
    /// sha256 of the dataset file as read, hex.
    pub dataset_sha256: String,
    /// Rows in the dataset file (selection happens after parsing).
    pub dataset_rows: usize,
    /// The selection rule that produced the audited subset (e.g.
    /// `--per-repo 20 --repos astropy/astropy,…`), verbatim.
    pub selection: String,
    /// loom-eval's own package version (the auditor's identity).
    pub runner_version: &'static str,
}

/// The whole audit: per-instance rows plus the counts the provenance
/// block and the re-grade README consume.
#[derive(Debug, Serialize)]
pub struct SuiteAudit {
    /// Attributable-measurement block (house rule: no report without
    /// one).
    pub provenance: SuiteAuditProvenance,
    /// One row per instance, dataset order.
    pub rows: Vec<SuiteAuditRow>,
    /// Instances with the weak-suite hypothesis flag.
    pub weak_ids: Vec<String>,
    /// Total single-hunk-dropped mutants the full re-grade must score.
    pub mutant_runs: usize,
    /// Instances the mutant pass cannot test (single-hunk gold patch).
    pub unmutable_ids: Vec<String>,
}

/// Counts the hunks in a unified diff (`@@` headers).
pub fn count_hunks(diff: &str) -> usize {
    diff.lines().filter(|line| line.starts_with("@@")).count()
}

/// Counts added test patch lines that look like tests. Coarse on
/// purpose — the signal is "the suite grew", not exact test counts.
fn added_test_lines(test_patch: &str) -> usize {
    test_patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .filter(|line| line.to_lowercase().contains("test"))
        .count()
}

/// One file section of a diff: its header lines plus hunks.
struct FileSection {
    header: Vec<String>,
    hunks: Vec<Vec<String>>,
}

fn split_sections(diff: &str) -> Vec<FileSection> {
    let mut sections: Vec<FileSection> = Vec::new();
    let mut current: Option<FileSection> = None;
    for line in diff.lines().filter(|line| !line.is_empty()) {
        if line.starts_with("diff --git ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(FileSection {
                header: vec![line.to_owned()],
                hunks: Vec::new(),
            });
        } else if line.starts_with("@@") {
            match current.as_mut() {
                Some(section) => section.hunks.push(vec![line.to_owned()]),
                // A diff with no `diff --git` header: synthesize one
                // section so lone-hunk diffs still parse.
                None => {
                    current = Some(FileSection {
                        header: Vec::new(),
                        hunks: vec![vec![line.to_owned()]],
                    })
                }
            }
        } else if let Some(section) = current.as_mut() {
            match section.hunks.last_mut() {
                Some(hunk) => hunk.push(line.to_owned()),
                None => section.header.push(line.to_owned()),
            }
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    sections
}

/// Rebuilds the diff with global hunk `index` (0-based) dropped, or
/// `None` when the index is out of range. File sections left without
/// hunks are dropped entirely — an empty `diff --git` block is not a
/// patch the scorer would accept.
pub fn drop_hunk(diff: &str, index: usize) -> Option<String> {
    if index >= count_hunks(diff) {
        return None;
    }
    let mut seen = 0usize;
    let mut body = Vec::new();
    for section in split_sections(diff) {
        let kept: Vec<&Vec<String>> = section
            .hunks
            .iter()
            .filter(|_| {
                let drop = seen == index;
                seen += 1;
                !drop
            })
            .collect();
        if kept.is_empty() {
            continue;
        }
        body.extend(section.header.iter().cloned());
        for hunk in kept {
            body.extend(hunk.iter().cloned());
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(body.join("\n") + "\n")
}

/// Every single-hunk-dropped mutant of a gold patch, one per hunk, in
/// hunk order. A one-hunk (or empty) patch yields no mutants — there
/// is nothing to cripple.
pub fn mutant_patches(patch: &str) -> Vec<String> {
    (0..count_hunks(patch))
        .filter_map(|index| drop_hunk(patch, index))
        .collect()
}

/// Method marker for [`SuiteAuditProvenance`]. Bump on any change to
/// the weak flags, the mutant rule, or the row shape.
pub const SUITE_AUDIT_METHOD_VERSION: &str = "1";

/// Builds the audit's provenance block from the dataset bytes and the
/// selection rule (the bin owns both; `audit_dataset` stays pure).
pub fn suite_audit_provenance(
    dataset_bytes: &[u8],
    dataset_rows: usize,
    selection: &str,
) -> SuiteAuditProvenance {
    SuiteAuditProvenance {
        method_version: SUITE_AUDIT_METHOD_VERSION,
        dataset_sha256: crate::swebench::sha256_hex(dataset_bytes),
        dataset_rows,
        selection: selection.to_owned(),
        runner_version: env!("CARGO_PKG_VERSION"),
    }
}

/// Runs the structural audit over the (frozen) dataset.
pub fn audit_dataset(instances: &[&SweInstance], provenance: SuiteAuditProvenance) -> SuiteAudit {
    let rows: Vec<SuiteAuditRow> = instances
        .iter()
        .map(|instance| {
            let added = added_test_lines(&instance.test_patch);
            let f2p = instance.fail_to_pass.len();
            let p2p = instance.pass_to_pass.len();
            let gold_hunks = count_hunks(&instance.patch);
            SuiteAuditRow {
                instance_id: instance.instance_id.clone(),
                repo: instance.repo.clone(),
                f2p,
                p2p,
                gold_hunks,
                added_test_lines: added,
                suite_small: f2p + p2p <= 2,
                no_new_tests: added == 0,
                single_hunk: gold_hunks == 1,
            }
        })
        .collect();
    SuiteAudit {
        provenance,
        weak_ids: rows
            .iter()
            .filter(|row| row.weak())
            .map(|row| row.instance_id.clone())
            .collect(),
        mutant_runs: rows
            .iter()
            .map(|row| row.gold_hunks.saturating_sub(1))
            .sum(),
        unmutable_ids: rows
            .iter()
            .filter(|row| row.single_hunk)
            .map(|row| row.instance_id.clone())
            .collect(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILE_PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
+fn added() {}
 fn b() {}
@@ -10,2 +11,3 @@
 fn c() {}
+fn also_added() {}
diff --git a/src/main.rs b/src/main.rs
index 333..444 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -5,2 +5,3 @@
 fn main() {
+    println!(\"hi\");
";

    #[test]
    fn count_hunks_reads_across_files() {
        assert_eq!(count_hunks(TWO_FILE_PATCH), 3);
        assert_eq!(count_hunks(""), 0);
    }

    #[test]
    fn drop_hunk_removes_one_hunk_and_keeps_the_rest() {
        let first = drop_hunk(TWO_FILE_PATCH, 0).expect("mutant 0");
        assert!(!first.contains("fn added"), "hunk 0's added line is gone");
        assert!(first.contains("also_added"));
        assert!(first.contains("src/main.rs"));
        let last = drop_hunk(TWO_FILE_PATCH, 2).expect("mutant 2");
        assert!(!last.contains("hi"));
        assert!(last.contains("fn a() {}"));
    }

    #[test]
    fn drop_hunk_drops_a_file_left_without_hunks() {
        // Hunk 2 is main.rs's only hunk: the whole section must go.
        let mutant = drop_hunk(TWO_FILE_PATCH, 2).expect("mutant 2");
        assert!(!mutant.contains("main.rs"));
    }

    #[test]
    fn drop_hunk_out_of_range_is_none() {
        assert!(drop_hunk(TWO_FILE_PATCH, 3).is_none());
    }

    #[test]
    fn mutant_patches_emits_one_per_hunk() {
        let mutants = mutant_patches(TWO_FILE_PATCH);
        assert_eq!(mutants.len(), 3);
        assert!(mutants.iter().all(|m| count_hunks(m) == 2));
        assert!(mutant_patches("@@ -1,1 +1,2 @@\n+x\n").is_empty());
    }

    #[test]
    fn audit_dataset_flags_small_and_testless_suites() {
        let instance =
            |id: &str, f2p: usize, p2p: usize, patch: &str, test_patch: &str| SweInstance {
                instance_id: id.to_owned(),
                repo: "org/repo".to_owned(),
                base_commit: "0".repeat(40),
                patch: patch.to_owned(),
                test_patch: test_patch.to_owned(),
                problem_statement: String::new(),
                hints_text: String::new(),
                fail_to_pass: vec!["t".to_owned(); f2p],
                pass_to_pass: vec!["t".to_owned(); p2p],
            };
        let hunk2 = "@@ -1,2 +1,3 @@\n a\n b\n+c\n@@ -9,2 +10,3 @@\n d\n e\n+f\n";
        let provenance =
            suite_audit_provenance(b"dataset-bytes", 500, "--per-repo 20 --repos org/repo");
        let all = [
            // Small suite, no new tests: both flags.
            instance("small", 1, 0, hunk2, ""),
            // Healthy: 3 tests, tests added, multi-hunk.
            instance("ok", 1, 2, hunk2, "+def test_new():\n+    assert True\n"),
            // Single-hunk gold: unmutable, not weak by itself.
            instance("single", 3, 3, "@@ -1,1 +1,2 @@\n+z\n", ""),
        ];
        let rows: Vec<&SweInstance> = all.iter().collect();
        let audit = audit_dataset(&rows, provenance);
        assert_eq!(
            audit.weak_ids,
            vec!["small".to_owned(), "single".to_owned()]
        );
        assert_eq!(audit.mutant_runs, 2);
        assert_eq!(audit.unmutable_ids, vec!["single".to_owned()]);
        let small = &audit.rows[0];
        assert!(small.suite_small && small.no_new_tests);
        assert!(!audit.rows[1].weak());
        assert_eq!(audit.provenance.dataset_rows, 500);
        assert_eq!(audit.provenance.method_version, SUITE_AUDIT_METHOD_VERSION);
        assert_eq!(
            audit.provenance.dataset_sha256,
            crate::swebench::sha256_hex(b"dataset-bytes")
        );
    }
}
