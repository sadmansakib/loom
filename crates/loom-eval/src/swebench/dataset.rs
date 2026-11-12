//! SWE-bench dataset plumbing: the Verified instance schema, JSONL
//! parsing, cache loading, the HF datasets-server fetch, and sweep
//! instance selection.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The HF dataset the sweep targets (500 human-validated instances).
pub const DATASET_ID: &str = "princeton-nlp/SWE-bench_Verified";

/// Row count the Verified split is expected to carry; the fetch loop
/// cross-checks against the server's `num_rows_total` so a truncated
/// cache can never pass silently for a complete one.
pub const EXPECTED_ROWS: usize = 500;

/// One dataset instance, in the classic SWE-bench JSONL schema. The
/// test lists arrive either as JSON-encoded strings (the original
/// release) or as real arrays (the parquet conversion); both parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweInstance {
    /// The dataset's canonical id (`org__repo-number`).
    pub instance_id: String,
    /// `org/repo` on GitHub.
    pub repo: String,
    /// The commit the repo is checked out at inside the testbed image.
    pub base_commit: String,
    /// Gold patch — provenance only; the agent never sees it.
    #[serde(default)]
    pub patch: String,
    /// Test patch — the scorer applies it, not us.
    #[serde(default)]
    pub test_patch: String,
    /// The issue text the agent is given.
    pub problem_statement: String,
    /// Optional maintainer hints appended to the prompt.
    #[serde(default)]
    pub hints_text: String,
    /// Tests the gold patch flips from red to green (scorer-side only).
    #[serde(rename = "FAIL_TO_PASS", default, deserialize_with = "string_or_vec")]
    pub fail_to_pass: Vec<String>,
    /// Tests that must stay green (scorer-side only).
    #[serde(rename = "PASS_TO_PASS", default, deserialize_with = "string_or_vec")]
    pub pass_to_pass: Vec<String>,
}

/// `FAIL_TO_PASS` / `PASS_TO_PASS` arrive as `"[\"a\",\"b\"]"` or as
/// `["a","b"]`; an empty string is the empty list. Shared with the
/// Multilingual module, whose rows use the same two test-list shapes.
pub(crate) fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Str(String),
        List(Vec<String>),
    }
    match Repr::deserialize(deserializer)? {
        Repr::Str(s) if s.trim().is_empty() => Ok(Vec::new()),
        Repr::Str(s) => serde_json::from_str(&s).map_err(serde::de::Error::custom),
        Repr::List(v) => Ok(v),
    }
}

/// Parses dataset JSONL. A malformed line panics with its number: a
/// half-loaded corpus would silently shrink the sweep.
pub fn parse_dataset(body: &str) -> Vec<SweInstance> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("dataset line {}: {error}", index + 1))
        })
        .collect()
}

/// Dataset cache location: `<eval_dir>/swebench/verified.jsonl`.
pub fn dataset_cache_path(eval_dir: &Path) -> PathBuf {
    eval_dir.join("swebench").join("verified.jsonl")
}

/// Loads the dataset without touching the network: the explicit
/// `LOOM_EVAL_SWEBENCH_JSONL` file wins (manual/offline mode), then the
/// cache. `None` means the caller must fetch.
pub fn load_dataset_cached(eval_dir: &Path) -> Option<Vec<SweInstance>> {
    if let Ok(path) = std::env::var("LOOM_EVAL_SWEBENCH_JSONL") {
        return Some(parse_dataset(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("LOOM_EVAL_SWEBENCH_JSONL ({path}): {error}")),
        ));
    }
    let cache = dataset_cache_path(eval_dir);
    if cache.exists() {
        return Some(parse_dataset(&std::fs::read_to_string(&cache).unwrap()));
    }
    None
}

/// Fetches the Verified split through the HF datasets-server rows API
/// (plain JSON, 100 rows per page) and returns the canonical JSONL.
/// Live-only: called by the runner when no cache exists.
pub async fn fetch_dataset_verified() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("loom-eval/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("building the datasets-server client: {error}"))?;
    let mut instances = Vec::new();
    let mut total: Option<usize> = None;
    while total.is_none_or(|total| instances.len() < total) {
        let url = format!(
            "https://datasets-server.huggingface.co/rows?dataset=princeton-nlp%2FSWE-bench_Verified&config=default&split=test&offset={}&length=100",
            instances.len()
        );
        let response = client
            .get(&url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|error| {
                format!(
                    "datasets-server page at offset {}: {error}",
                    instances.len()
                )
            })?;
        let page: serde_json::Value = response.json().await.map_err(|error| {
            format!(
                "decoding datasets-server page at offset {}: {error}",
                instances.len()
            )
        })?;
        let rows = page["rows"].as_array().ok_or_else(|| {
            format!(
                "datasets-server page at offset {}: no rows array",
                instances.len()
            )
        })?;
        if rows.is_empty() {
            return Err(format!(
                "datasets-server returned an empty page at offset {} before {total:?} rows",
                instances.len()
            ));
        }
        for row in rows {
            // Each entry is {"row_idx": n, "row": {…instance…}}; going
            // through `from_value` keeps the tolerant field decoding in
            // one place (SweInstance's serde impl).
            let instance: SweInstance = serde_json::from_value(row["row"].clone())
                .map_err(|error| format!("dataset row: {error}"))?;
            instances.push(instance);
        }
        total = Some(
            page["num_rows_total"]
                .as_u64()
                .ok_or("datasets-server response missing num_rows_total")? as usize,
        );
    }
    let total = total.unwrap();
    if total != EXPECTED_ROWS || instances.len() != total {
        return Err(format!(
            "dataset shape drifted: {} rows fetched of {} advertised (expected {EXPECTED_ROWS})",
            instances.len(),
            total
        ));
    }
    // Canonical order (as served), one instance per line.
    Ok(instances
        .iter()
        .map(|instance| serde_json::to_string(instance).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

/// Selects the sweep's instances. Explicit ids win (every id must
/// resolve — a typo must fail, not shrink the sweep silently);
/// `Some(per_repo)` takes the first `per_repo` instances of each
/// requested repo, in repo order (stratified sampling); otherwise the
/// repo filter applies and the first `max` instances in dataset order
/// run.
pub fn select_instances<'a>(
    all: &'a [SweInstance],
    ids: &[String],
    repos: &[String],
    max: usize,
    per_repo: Option<usize>,
) -> Vec<&'a SweInstance> {
    if !ids.is_empty() {
        let mut selected = Vec::new();
        for id in ids {
            let instance = all
                .iter()
                .find(|instance| &instance.instance_id == id)
                .unwrap_or_else(|| panic!("unknown SWE-bench instance id: {id}"));
            selected.push(instance);
        }
        return selected;
    }
    if let Some(per_repo) = per_repo {
        assert!(
            !repos.is_empty(),
            "--per-repo needs --repos (which repos to stratify over?)"
        );
        let mut selected = Vec::new();
        for repo in repos {
            let mut taken = 0;
            for instance in all {
                if taken == per_repo {
                    break;
                }
                if &instance.repo == repo {
                    selected.push(instance);
                    taken += 1;
                }
            }
        }
        return selected;
    }
    all.iter()
        .filter(|instance| repos.is_empty() || repos.iter().any(|repo| &instance.repo == repo))
        .take(max)
        .collect()
}

// ------------------------------------------------------------ tests ---

/// A two-instance fixture exercising both test-list encodings.
#[cfg(test)]
pub(crate) fn fixture_jsonl() -> String {
    let one = serde_json::json!({
        "instance_id": "org/one-1",
        "repo": "org/one",
        "base_commit": "aa11",
        "patch": "--- a\n+++ b\n",
        "test_patch": "--- t\n+++ t\n",
        "problem_statement": "It crashes.",
        "hints_text": "",
        // The original release encodes the test lists as JSON strings.
        "FAIL_TO_PASS": "[\"test_one::fails\"]",
        "PASS_TO_PASS": ["test_two::passes"],
    });
    let two = serde_json::json!({
        "instance_id": "org/two-2",
        "repo": "org/two",
        "base_commit": "bb22",
        "problem_statement": "Wrong output.",
        "FAIL_TO_PASS": ["test_a"],
        "PASS_TO_PASS": "",
    });
    format!("{one}\n{two}\n")
}

#[test]
fn dataset_parses_both_test_list_encodings() {
    let instances = parse_dataset(&fixture_jsonl());
    assert_eq!(instances.len(), 2);
    assert_eq!(instances[0].instance_id, "org/one-1");
    assert_eq!(instances[0].fail_to_pass, ["test_one::fails"]);
    assert_eq!(instances[0].pass_to_pass, ["test_two::passes"]);
    assert_eq!(instances[1].fail_to_pass, ["test_a"]);
    assert!(instances[1].pass_to_pass.is_empty());
    // Optional fields default rather than reject.
    assert_eq!(instances[1].hints_text, "");
    assert_eq!(instances[1].patch, "");
    // Round-trip: the canonical cache re-parses to the same instances.
    let canonical = instances
        .iter()
        .map(|i| serde_json::to_string(i).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(parse_dataset(&canonical).len(), 2);
}

#[test]
#[should_panic(expected = "dataset line 2")]
fn dataset_rejects_malformed_lines() {
    parse_dataset(
        "{\"instance_id\":\"ok\",\"repo\":\"o/r\",\"base_commit\":\"c\",\"problem_statement\":\"p\"}\n{not json}\n",
    );
}

#[test]
fn selection_explicit_ids_must_resolve_in_order() {
    let instances = parse_dataset(&fixture_jsonl());
    let ids = [String::from("org/two-2"), String::from("org/one-1")];
    let selected = select_instances(&instances, &ids, &[], 5, None);
    assert_eq!(
        selected
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["org/two-2", "org/one-1"]
    );
}

#[test]
#[should_panic(expected = "unknown SWE-bench instance id")]
fn selection_typo_fails_rather_than_shrinks() {
    let instances = parse_dataset(&fixture_jsonl());
    select_instances(&instances, &[String::from("org/nope-9")], &[], 5, None);
}

#[test]
fn selection_filters_by_repo_then_caps() {
    let instances = parse_dataset(&fixture_jsonl());
    let all = select_instances(&instances, &[], &[], 10, None);
    assert_eq!(all.len(), 2);
    let capped = select_instances(&instances, &[], &[], 1, None);
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].instance_id, "org/one-1");
    let one_repo = [String::from("org/two")];
    let filtered = select_instances(&instances, &[], &one_repo, 10, None);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].instance_id, "org/two-2");
}

#[test]
fn selection_stratified_takes_n_per_repo_in_repo_order() {
    // Three in repo a (dataset order), two in repo b.
    let body = [
        r#"{"instance_id":"a/a-1","repo":"a/a","base_commit":"c","problem_statement":"p"}"#,
        r#"{"instance_id":"a/a-2","repo":"a/a","base_commit":"c","problem_statement":"p"}"#,
        r#"{"instance_id":"b/b-1","repo":"b/b","base_commit":"c","problem_statement":"p"}"#,
        r#"{"instance_id":"a/a-3","repo":"a/a","base_commit":"c","problem_statement":"p"}"#,
        r#"{"instance_id":"b/b-2","repo":"b/b","base_commit":"c","problem_statement":"p"}"#,
    ]
    .join("\n");
    let instances = parse_dataset(&body);
    let repos = [String::from("a/a"), String::from("b/b")];
    let selected = select_instances(&instances, &[], &repos, 99, Some(1));
    assert_eq!(
        selected
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["a/a-1", "b/b-1"]
    );
    // Cap applies per repo: a has three, only two taken.
    let selected = select_instances(&instances, &[], &repos, 99, Some(2));
    assert_eq!(
        selected
            .iter()
            .map(|i| i.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["a/a-1", "a/a-2", "b/b-1", "b/b-2"]
    );
    // Repo order drives output order, not dataset order.
    let reversed = [String::from("b/b"), String::from("a/a")];
    let selected = select_instances(&instances, &[], &reversed, 99, Some(1));
    assert_eq!(selected[0].instance_id, "b/b-1");
    assert_eq!(selected[1].instance_id, "a/a-1");
}

#[test]
#[should_panic(expected = "--per-repo needs --repos")]
fn stratified_selection_requires_repos() {
    let instances = parse_dataset(&fixture_jsonl());
    select_instances(&instances, &[], &[], 5, Some(1));
}
