//! `suite-audit`: UTBoost-style weak-suite audit of the frozen
//! SWE-bench Verified subset (roadmap item: frozen-subset suite audit).
//! Two deterministic passes, no model and no network:
//!
//! 1. Structural audit (offline): flags instances whose suite shape is
//!    weak — tiny suites, or suites the test patch does not extend.
//! 2. Mutant re-grade (docker, one-off): every single-hunk-dropped
//!    gold-patch mutant is written as its own predictions file for the
//!    official scorer. An instance whose every mutant still resolves
//!    has a suite that cannot catch a partial fix — confirmed weak.
//!
//! Usage:
//!   suite-audit --dataset <verified.jsonl> --out <dir>
//!
//! The binary never runs docker itself. It writes, under `<out>`:
//!   suite_audit.json               — structural rows + flags
//!   mutants/mutant-<k>/predictions.jsonl — hunk-<k>-dropped gold patches
//!   README.md                      — the scorer loop + weak-confirm rule
//!
//! shortcut: the mutant pass writes every mutant of every instance into
//! shared per-hunk-index files, so one docker run per file covers all
//! instances; upgrade to per-instance fan-out only if scorer cost bites.

// A harness binary: panicking on a setup failure is the intended
// behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use loom_eval::suite_audit::{audit_dataset, count_hunks, mutant_patches, suite_audit_provenance};
use loom_eval::swebench::{
    Prediction, SweInstance, parse_dataset, select_instances, write_predictions,
};

struct Args {
    dataset: PathBuf,
    out: PathBuf,
    /// `--repos` (comma-separated) + `--per-repo`, mirroring
    /// swebench-run so the audit can reproduce the frozen subset from
    /// the same cached dataset file.
    repos: Vec<String>,
    per_repo: Option<usize>,
}

fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|field| field.trim().to_owned())
        .filter(|field| !field.is_empty())
        .collect()
}

fn parse_args() -> Args {
    let mut dataset = None;
    let mut out = None;
    let mut repos = Vec::new();
    let mut per_repo = None;
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .unwrap_or_else(|| panic!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--dataset" => dataset = Some(PathBuf::from(value())),
            "--out" => out = Some(PathBuf::from(value())),
            "--repos" => repos = csv(&value()),
            "--per-repo" => {
                per_repo = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| panic!("--per-repo needs a number")),
                )
            }
            other => panic!("unknown flag: {other}"),
        }
    }
    if per_repo.is_some() && repos.is_empty() {
        panic!("--per-repo needs --repos (which repos to stratify over?)");
    }
    Args {
        dataset: dataset.expect("--dataset <verified.jsonl> is required"),
        out: out.expect("--out <dir> is required"),
        repos,
        per_repo,
    }
}

/// The selection rule, verbatim, for the provenance block.
fn selection_rule(args: &Args) -> String {
    match args.per_repo {
        Some(per_repo) => format!("--per-repo {per_repo} --repos {}", args.repos.join(",")),
        None => "all rows".to_owned(),
    }
}

/// Per-hunk-index mutant files: `mutants/mutant-<k>/predictions.jsonl`
/// holds one row per instance whose gold patch has at least `k + 1`
/// hunks, the patch being its hunk-`k`-dropped mutant. Scoring every
/// file and intersecting the resolved sets per instance answers, for
/// each instance, whether any dropped hunk was ever caught.
fn write_mutant_files(out: &std::path::Path, instances: &[&SweInstance]) -> usize {
    let mutants_dir = out.join("mutants");
    std::fs::create_dir_all(&mutants_dir).expect("create mutants dir");
    let mut files = 0usize;
    let mut index = 0usize;
    loop {
        // Owned first: `Prediction` borrows `model_patch`, and the
        // mutant strings are locals of the loop below.
        let mut patches = Vec::new();
        for instance in instances {
            if let Some(patch) = mutant_patches(&instance.patch).get(index) {
                patches.push((instance.instance_id.clone(), patch.clone()));
            }
        }
        let rows: Vec<Prediction<'_>> = patches
            .iter()
            .map(|(id, patch)| Prediction {
                instance_id: id,
                model_name_or_path: "gold-mutant",
                model_patch: patch,
            })
            .collect();
        if rows.is_empty() {
            break;
        }
        if rows.is_empty() {
            break;
        }
        let dir = mutants_dir.join(format!("mutant-{index}"));
        std::fs::create_dir_all(&dir).expect("create mutant dir");
        write_predictions(&dir.join("predictions.jsonl"), &rows);
        files += 1;
        index += 1;
    }
    files
}

fn write_readme(out: &std::path::Path, instances: &[&SweInstance], files: usize) {
    let max_hunks = instances
        .iter()
        .map(|i| count_hunks(&i.patch))
        .max()
        .unwrap_or(0);
    let body = format!(
        "# Frozen-subset suite audit (UTBoost-style)\n\n\
         Structural pass: `suite_audit.json` (rows, weak flags, mutant\n\
         counts). `weak_ids` lists instances flagged by suite shape.\n\n\
         Mutant pass: {files} predictions files under `mutants/`, one per\n\
         dropped-hunk index (max gold hunks: {max_hunks}). Score each with\n\
         the official harness, e.g.:\n\n\
         swebench.harness.run_evaluation --predictions_path mutants/mutant-0/predictions.jsonl\n\
         --dataset_name princeton-nlp/SWE-bench_Verified --run_id mutant-0 --max_workers 8\n\n\
         Weak-confirm rule: an instance is weak when every one of its\n\
         mutants still resolves (no dropped hunk ever failed the suite).\n\
         Intersect the resolved sets from all mutant runs per instance to\n\
         apply it; `suite_audit.json` rows carry each instance's hunk\n\
         count to drive the intersection.\n"
    );
    std::fs::write(out.join("README.md"), body).expect("write README");
}

fn main() {
    let args = parse_args();
    let bytes = std::fs::read(&args.dataset)
        .unwrap_or_else(|error| panic!("{}: {error}", args.dataset.display()));
    let text = String::from_utf8(bytes.clone())
        .unwrap_or_else(|error| panic!("{} is not UTF-8: {error}", args.dataset.display()));
    let dataset = parse_dataset(&text);
    // `max = dataset.len()`: with no --per-repo the audit covers every
    // row the (optional) --repos filter leaves.
    let instances = select_instances(&dataset, &[], &args.repos, dataset.len(), args.per_repo);
    let provenance = suite_audit_provenance(&bytes, dataset.len(), &selection_rule(&args));
    let audit = audit_dataset(&instances, provenance);
    std::fs::create_dir_all(&args.out).expect("create out dir");
    let files = write_mutant_files(&args.out, &instances);
    write_readme(&args.out, &instances, files);
    std::fs::write(
        args.out.join("suite_audit.json"),
        serde_json::to_string_pretty(&audit).expect("serialize audit"),
    )
    .expect("write suite_audit.json");
    println!(
        "{} of {} rows selected; {} structurally weak, {} unmutable (single-hunk), {} mutant files",
        instances.len(),
        dataset.len(),
        audit.weak_ids.len(),
        audit.unmutable_ids.len(),
        files,
    );
}
