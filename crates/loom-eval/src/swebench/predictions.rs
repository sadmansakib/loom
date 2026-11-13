//! Predictions emission: the official harness's JSONL row shape,
//! per-run prediction files, and best-of-N patch selection.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// A predictions row in the official harness's JSONL shape.
#[derive(Serialize)]
pub struct Prediction<'a> {
    /// The dataset's canonical id; the scorer keys on it.
    pub instance_id: &'a str,
    /// Model label recorded in the predictions file.
    pub model_name_or_path: &'a str,
    /// The agent's diff (empty string = nothing changed).
    pub model_patch: &'a str,
}

/// Writes one predictions file. One row per instance — the official
/// scorer keys by `instance_id`, so repeated runs write per-run files
/// (`predictions-<n>.jsonl`), never duplicate rows in one file.
///
/// # Panics
///
/// Panics when the file cannot be written: the predictions file is the
/// sweep's deliverable, so a failed write must abort the run.
#[allow(clippy::panic)] // deliverable write failure must abort the sweep
pub fn write_predictions(path: &Path, rows: &[Prediction<'_>]) {
    let body = rows
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, body).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// Writes one prediction file per run and, when there are multiple runs,
/// a best-of-N file selected by normalized-diff majority.
#[must_use]
pub fn write_prediction_sets(
    directory: &Path,
    filename_prefix: &str,
    predictions: &[Vec<(String, String, String)>],
    runs: usize,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for (index, rows) in predictions.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let borrowed: Vec<_> = rows
            .iter()
            .map(|(instance_id, model, patch)| Prediction {
                instance_id,
                model_name_or_path: model,
                model_patch: patch,
            })
            .collect();
        let path = directory.join(format!("{filename_prefix}-{}.jsonl", index + 1));
        write_predictions(&path, &borrowed);
        paths.push(path);
    }

    if runs > 1 && !predictions.is_empty() {
        let selected: Vec<_> = (0..predictions[0].len())
            .map(|index| {
                let patches: Vec<_> = predictions
                    .iter()
                    .map(|rows| rows[index].2.as_str())
                    .collect();
                let (instance_id, model, _) = &predictions[0][index];
                (
                    instance_id.clone(),
                    model.clone(),
                    select_patch(&patches).to_owned(),
                )
            })
            .collect();
        let borrowed: Vec<_> = selected
            .iter()
            .map(|(instance_id, model, patch)| Prediction {
                instance_id,
                model_name_or_path: model,
                model_patch: patch,
            })
            .collect();
        let path = directory.join(format!("{filename_prefix}-selected.jsonl"));
        write_predictions(&path, &borrowed);
        paths.push(path);
    }

    paths
}

/// Filesystem-safe run id: `/` and `:` folded to `-` (the same slug
/// rule the in-house harness used).
#[must_use]
pub fn slug(id: &str) -> String {
    id.replace(['/', ':'], "-")
}

/// Best-of-N selection across an instance's runs (Trae/Agentless
/// majority voting, adapted to visible signals only): among non-empty
/// patches, the most frequent normalized diff wins — independent runs
/// converging on the same change is evidence the fix is real, while a
/// one-off diff is more likely a lucky or broken path. Ties go to the
/// earlier run (cache-friendlier, run-1-compatible); if every patch is
/// empty the first (empty) patch is returned so the scorer still gets
/// its one row per instance.
#[must_use]
pub fn select_patch<'a>(patches: &[&'a str]) -> &'a str {
    let normalize = |patch: &str| {
        patch
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut best: Option<(&'a str, usize)> = None; // (patch, count)
    for patch in patches.iter().filter(|patch| !patch.trim().is_empty()) {
        let normalized = normalize(patch);
        let count = patches
            .iter()
            .filter(|other| normalize(other) == normalized)
            .count();
        // Strictly greater: ties keep the earlier run's patch.
        if best.is_none_or(|(_, best_count)| count > best_count) {
            best = Some((patch, count));
        }
    }
    match best {
        Some((patch, _)) => patch,
        // All runs empty: scorer still needs the row.
        None => patches.first().copied().unwrap_or(""),
    }
}

// ------------------------------------------------------------ tests ---

#[test]
fn predictions_write_the_official_shape() {
    let dir = std::env::temp_dir().join(format!("loom-swe-pred-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("predictions-1.jsonl");
    let rows = [Prediction {
        instance_id: "org/one-1",
        model_name_or_path: "deepseek-flash",
        model_patch: "--- a/x\n+++ b/x\n",
    }];
    write_predictions(&path, &rows);
    let body = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
    assert_eq!(parsed["instance_id"], "org/one-1");
    assert_eq!(parsed["model_name_or_path"], "deepseek-flash");
    assert_eq!(parsed["model_patch"], "--- a/x\n+++ b/x\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prediction_sets_write_per_run_and_selected_files() {
    let dir = std::env::temp_dir().join(format!("loom-swe-sets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let predictions = vec![
        vec![("one".into(), "model".into(), "patch-a".into())],
        vec![("one".into(), "model".into(), "patch-a\n".into())],
    ];

    let paths = write_prediction_sets(&dir, "predictions-model", &predictions, 2);

    assert_eq!(
        paths
            .iter()
            .map(|path| path.file_name().unwrap())
            .collect::<Vec<_>>(),
        [
            std::ffi::OsStr::new("predictions-model-1.jsonl"),
            std::ffi::OsStr::new("predictions-model-2.jsonl"),
            std::ffi::OsStr::new("predictions-model-selected.jsonl"),
        ]
    );
    let selected: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(paths.last().unwrap())
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(selected["model_patch"], "patch-a");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selection_takes_the_majority_patch() {
    let a = "--- a/x\n+++ b/x\n@@\n-old\n+new ";
    let b = "--- a/x\n+++ b/x\n@@\n-old\n+different";
    // Trailing whitespace differences normalize away: a == c.
    let c = "--- a/x\n+++ b/x\n@@\n-old\n+new";
    assert_eq!(select_patch(&[b, a, c]), a, "2-of-3 majority wins");
    assert_eq!(
        select_patch(&[a, b]),
        a,
        "ties go to the earlier run (run-1 compatible)"
    );
    assert_eq!(select_patch(&["", a, ""]), a, "empty patches never win");
    assert_eq!(
        select_patch(&["", ""]).trim(),
        "",
        "all-empty still yields a row"
    );
}

#[test]
fn slug_folds_separators() {
    assert_eq!(slug("sympy__sympy-20590"), "sympy__sympy-20590");
    assert_eq!(slug("org/one-1"), "org-one-1");
    assert_eq!(slug("a:b"), "a-b");
}
