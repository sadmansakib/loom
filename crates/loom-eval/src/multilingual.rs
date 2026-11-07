//! SWE-bench Multilingual — the Rust slice, as a parallel eval path.
//!
//! Roadmap item 35. Where the Verified sweep runs Python repos, this
//! module runs the 43 Rust tasks of `SWE-bench/SWE-bench_Multilingual`
//! (300 tasks, 9 languages, 41 repos; Rust across 7 of them) with the
//! same isolation model: one container per (instance, run), the frozen
//! v5 prompt byte-identical, the patch extracted from the git tree, and
//! grading only through the official scorer
//! (`swebench eval multilingual -p <predictions> --run-id …`).
//!
//! Two facts shape the design, both verified against the live dataset
//! on 2026-11-06:
//!
//! * the dataset has **no language column** — language is a property of
//!   the repo, so the slice is defined by an explicit repo set
//!   ([`RUST_REPOS`]) and gated by integrity checks that abort before
//!   any spend;
//! * every row carries its **own `image` field naming the prebuilt
//!   testbed on Docker Hub** (`swebench/sweb.eval.x86_64.<id>:latest`),
//!   so the runner pulls exactly what the row says — no image-name
//!   derivation, no retag step (in Verified the runner must fold and
//!   dunder-rewrite the id; here the dataset does it for us, and the
//!   shape check exists only to refuse anything unexpected).
//!
//! The container script is the Verified [`crate::swebench::CONTAINER_SCRIPT`]
//! plus the Rust toolchain in the seeded manifest; it is frozen as an
//! explicit constant, never spliced at runtime, and both scripts are
//! pinned by sha256 in the tests below.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::swebench::{self, prompt_text};

/// Dataset id as the scorer's `multilingual` alias resolves it.
pub const MULTILINGUAL_DATASET_ID: &str = "SWE-bench/SWE-bench_Multilingual";

/// Row count of the full dataset. The fetch aborts on drift — a
/// half-loaded corpus would silently shrink the slice.
pub const EXPECTED_ROWS: usize = 300;

/// Expected size of the Rust slice; the integrity gate refuses to run
/// on any other count.
pub const EXPECTED_RUST_ROWS: usize = 43;

/// The seven Rust repositories of the dataset, sorted. Measured
/// 2026-11-06 (matches the official appendix table): tokio 9, bat 8,
/// axum 7, ruff 7, nushell 5, coreutils 5, ripgrep 2.
pub const RUST_REPOS: [&str; 7] = [
    "astral-sh/ruff",
    "burntsushi/ripgrep",
    "nushell/nushell",
    "sharkdp/bat",
    "tokio-rs/axum",
    "tokio-rs/tokio",
    "uutils/coreutils",
];

/// Provenance tag for the selection rule: "the seven Rust repos, first
/// N per repo in dataset order".
pub const SELECTION_RULE: &str = "rust-repos-v1";

/// Every prebuilt image name observed in the dataset has this shape;
/// rows carrying anything else are refused rather than guessed at.
const IMAGE_PREFIX: &str = "swebench/sweb.eval.x86_64.";
const IMAGE_SUFFIX: &str = ":latest";

/// One Multilingual row: the classic SWE-bench fields plus the columns
/// the Multilingual release adds. `created_at`, `eval_type`, and
/// `eval_script` arrive too but are scorer-side only; serde skips them.
/// The test lists use the same string-or-array shapes as Verified, so
/// the deserializer is shared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultilingualInstance {
    /// `org__repo-number`, same convention as Verified.
    pub instance_id: String,
    /// `org/repo` on GitHub.
    pub repo: String,
    /// The commit the testbed image has checked out.
    pub base_commit: String,
    /// The prebuilt testbed image on Docker Hub, straight from the
    /// dataset row. This is already the scorer's lookup name.
    pub image: String,
    /// Scorer-side log parser; every Rust row is `parse_log_cargo`.
    pub log_parser: String,
    /// Language version the testbed pins (provenance only).
    #[serde(default)]
    pub version: String,
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
    /// Tests the gold patch flips from red to green (scorer-side).
    #[serde(
        rename = "FAIL_TO_PASS",
        default,
        deserialize_with = "swebench::string_or_vec"
    )]
    pub fail_to_pass: Vec<String>,
    /// Tests that must stay green (scorer-side).
    #[serde(
        rename = "PASS_TO_PASS",
        default,
        deserialize_with = "swebench::string_or_vec"
    )]
    pub pass_to_pass: Vec<String>,
}

/// Parses Multilingual dataset JSONL. A malformed line panics with its
/// number, same contract as [`swebench::parse_dataset`]: a half-loaded
/// corpus must fail loudly.
pub fn parse_multilingual(body: &str) -> Vec<MultilingualInstance> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("dataset line {}: {error}", index + 1))
        })
        .collect()
}

/// Dataset cache location: `<eval_dir>/swebench/multilingual.jsonl`,
/// beside the Verified cache.
pub fn dataset_cache_path(eval_dir: &Path) -> PathBuf {
    eval_dir.join("swebench").join("multilingual.jsonl")
}

/// Loads the dataset without touching the network: the explicit
/// `LOOM_EVAL_MULTILINGUAL_JSONL` file wins (manual/offline mode), then
/// the cache. `None` means the caller must fetch.
pub fn load_dataset_cached(eval_dir: &Path) -> Option<Vec<MultilingualInstance>> {
    if let Ok(path) = std::env::var("LOOM_EVAL_MULTILINGUAL_JSONL") {
        return Some(parse_multilingual(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("LOOM_EVAL_MULTILINGUAL_JSONL ({path}): {error}")),
        ));
    }
    let cache = dataset_cache_path(eval_dir);
    if cache.exists() {
        let body = std::fs::read_to_string(&cache)
            .unwrap_or_else(|error| panic!("{}: {error}", cache.display()));
        return Some(parse_multilingual(&body));
    }
    None
}

/// Fetches the Multilingual split through the HF datasets-server rows
/// API and returns the canonical JSONL, mirroring the Verified fetch
/// (including its drift checks). Live-only: called by the runner when
/// no cache exists.
pub async fn fetch_dataset_multilingual() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("loom-eval/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("building the datasets-server client: {error}"))?;
    let mut instances = Vec::new();
    let mut total: Option<usize> = None;
    while total.is_none_or(|total| instances.len() < total) {
        let url = format!(
            "https://datasets-server.huggingface.co/rows?dataset=SWE-bench%2FSWE-bench_Multilingual&config=default&split=test&offset={}&length=100",
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
            let instance: MultilingualInstance = serde_json::from_value(row["row"].clone())
                .map_err(|error| format!("dataset row: {error}"))?;
            instances.push(instance);
        }
        total = Some(
            page["num_rows_total"]
                .as_u64()
                .ok_or("datasets-server response missing num_rows_total")? as usize,
        );
    }
    let total = total.unwrap_or(0);
    if total != EXPECTED_ROWS || instances.len() != total {
        return Err(format!(
            "dataset shape drifted: {} rows fetched of {} advertised (expected {EXPECTED_ROWS})",
            instances.len(),
            total
        ));
    }
    Ok(instances
        .iter()
        .map(|instance| {
            serde_json::to_string(instance)
                .map_err(|error| format!("re-serializing a dataset row: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n")
}

/// True when the row's image has the shape every Multilingual prebuilt
/// observed so far shares: `swebench/sweb.eval.x86_64.<slug>:latest`
/// with the slug limited to the characters the harness's own rewrite
/// produces (`a-z`, digits, `.`, `-`, `_`).
pub fn image_is_wellformed(image: &str) -> bool {
    let Some(slug) = image
        .strip_prefix(IMAGE_PREFIX)
        .and_then(|rest| rest.strip_suffix(IMAGE_SUFFIX))
    else {
        return false;
    };
    !slug.is_empty()
        && slug.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

/// Selects the Rust slice: rows whose repo is in [`RUST_REPOS`], in
/// dataset order. Deterministic and independent of row counts, so a
/// cached and a fresh fetch select the same tasks.
pub fn select_rust_rows(rows: &[MultilingualInstance]) -> Vec<MultilingualInstance> {
    rows.iter()
        .filter(|row| RUST_REPOS.contains(&row.repo.as_str()))
        .cloned()
        .collect()
}

/// Integrity gate for the slice; every check that must hold before any
/// spend. Aborts (Err) rather than degrades: a 42-task "Rust slice" of
/// a drifted dataset is a silent headline change, not an inconvenience.
pub fn check_slice_integrity(
    all: &[MultilingualInstance],
    rust: &[MultilingualInstance],
) -> Result<(), String> {
    if all.len() != EXPECTED_ROWS {
        return Err(format!(
            "dataset drifted: {} rows, expected {EXPECTED_ROWS}",
            all.len()
        ));
    }
    if rust.len() != EXPECTED_RUST_ROWS {
        return Err(format!(
            "rust slice drifted: {} tasks, expected {EXPECTED_RUST_ROWS} (repos: {})",
            rust.len(),
            RUST_REPOS.join(", ")
        ));
    }
    for row in rust {
        if row.log_parser != "parse_log_cargo" {
            return Err(format!(
                "{}: log parser {} is not parse_log_cargo — not a Rust task",
                row.instance_id, row.log_parser
            ));
        }
        if !image_is_wellformed(&row.image) {
            return Err(format!(
                "{}: unexpected image name {} — refusing to guess",
                row.instance_id, row.image
            ));
        }
    }
    Ok(())
}

/// The prompt for a Multilingual row: the frozen v5 text, byte-identical
/// to the Verified prompt for the same (repo, issue, hints) — the
/// contract is language-neutral on purpose, and sharing it means a
/// Multilingual score is comparable to a Verified one down to the
/// instruction the model actually saw.
pub fn build_prompt_ml(instance: &MultilingualInstance) -> String {
    prompt_text(
        &instance.repo,
        &instance.problem_statement,
        &instance.hints_text,
    )
}

/// The container script for the Rust slice: the Verified
/// [`swebench::CONTAINER_SCRIPT`] with the Rust toolchain added to the
/// seeded manifest. Written as an explicit frozen constant — never
/// spliced at runtime — so the exact bytes the sweep ran are pin-able
/// in provenance and tests. Everything after the heredoc `EOF` marker
/// is byte-identical to the Python script by construction (pinned by a
/// test): the same loom invocation, the same patch extraction, the
/// same exit-status preservation. Python/pip entries stay — inert
/// where absent, and ruff's testbed uses Python-side tooling.
pub const RUST_CONTAINER_SCRIPT: &str = r#"mkdir -p /testbed/.loom && cat > /testbed/.loom/commands.toml <<'EOF'
[[command]]
program = "bash"
sandbox = false

[[command]]
program = "sh"
sandbox = false

[[command]]
program = "cargo"
sandbox = false

[[command]]
program = "cargo-fmt"
sandbox = false

[[command]]
program = "rustc"
sandbox = false

[[command]]
program = "rustup"
sandbox = false

[[command]]
program = "python"
sandbox = false

[[command]]
program = "python3"
sandbox = false

[[command]]
program = "pip"
sandbox = false

[[command]]
program = "pip3"
sandbox = false

[[command]]
program = "git"
sandbox = false
EOF
if [ -n "${LOOM_BASE_URL:-}" ]; then set -- --base-url "$LOOM_BASE_URL"; else set --; fi; timeout "$LOOM_TIMEOUT_SECS" /loom/loom run --json --no-save --provider "$LOOM_PROVIDER" --model "$LOOM_MODEL" "$@" --max-turns "$LOOM_MAX_TURNS" --workspace /testbed "$(cat /prompt.txt)" > /out/run.json 2> /out/run.err; status=$?; rm -rf /testbed/.loom; cd /testbed && git add -A && git diff --cached > /out/patch.diff 2> /out/patch.err; exit $status"#;

/// The slice's provenance block, appended to the sweep report: what was
/// run, from where, selected how, with which script. Everything an
/// auditor needs to reproduce the task list without the report body.
#[derive(Debug, Clone, Serialize)]
pub struct SliceProvenance {
    /// Dataset id (`SWE-bench/SWE-bench_Multilingual`).
    pub dataset: &'static str,
    /// sha256 of the cached dataset JSONL the slice was read from.
    pub dataset_sha256: String,
    /// Selection rule id + the repo list it is defined by.
    pub selection_rule: &'static str,
    /// The seven Rust repos the rule selects by.
    pub repos: [&'static str; 7],
    /// Rows in the full dataset / rows selected.
    pub rows_total: usize,
    /// Rows in the full dataset / rows selected.
    pub rows_selected: usize,
    /// The frozen script the containers ran.
    pub container_script_sha256: String,
    /// The prompt contract both datasets share.
    pub prompt_version: &'static str,
}

/// Assembles the slice provenance from the canonical JSONL text (the
/// same bytes the cache holds) and the selected rows.
pub fn slice_provenance(dataset_jsonl: &str, rust_rows: usize) -> SliceProvenance {
    SliceProvenance {
        dataset: MULTILINGUAL_DATASET_ID,
        dataset_sha256: swebench::sha256_hex(dataset_jsonl.as_bytes()),
        selection_rule: SELECTION_RULE,
        repos: RUST_REPOS,
        rows_total: EXPECTED_ROWS,
        rows_selected: rust_rows,
        container_script_sha256: swebench::sha256_hex(RUST_CONTAINER_SCRIPT.as_bytes()),
        prompt_version: swebench::PROMPT_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swebench::{self, SweInstance};

    fn row(id: &str, repo: &str, parser: &str, image: &str) -> MultilingualInstance {
        MultilingualInstance {
            instance_id: id.to_owned(),
            repo: repo.to_owned(),
            base_commit: "c0ffee".to_owned(),
            image: image.to_owned(),
            log_parser: parser.to_owned(),
            version: "1.86.0".to_owned(),
            patch: String::new(),
            test_patch: String::new(),
            problem_statement: "It panics on empty input.".to_owned(),
            hints_text: String::new(),
            fail_to_pass: vec!["tests::case_one".to_owned()],
            pass_to_pass: vec!["tests::case_two".to_owned()],
        }
    }

    fn cargo_image(id: &str) -> String {
        format!("swebench/sweb.eval.x86_64.{id}:latest")
    }

    /// A full-size synthetic dataset: 300 rows, exactly 43 across the
    /// seven Rust repos (with the real per-repo counts), the rest
    /// spread over other languages' repos.
    fn synthetic_dataset() -> Vec<MultilingualInstance> {
        let rust_counts = [
            ("tokio-rs/tokio", 9),
            ("sharkdp/bat", 8),
            ("tokio-rs/axum", 7),
            ("astral-sh/ruff", 7),
            ("nushell/nushell", 5),
            ("uutils/coreutils", 5),
            ("burntsushi/ripgrep", 2),
        ];
        let others = [
            "apache/druid",
            "google/go-github",
            "facebook/hhvm",
            "redis/redis",
            "php/php-src",
        ];
        let mut rows = Vec::new();
        let mut other_i = 0;
        for (repo, count) in rust_counts {
            for n in 0..count {
                let id = format!("{}-{}", repo.replace('/', "__"), 100 + n);
                rows.push(row(&id, repo, "parse_log_cargo", &cargo_image(&id)));
            }
        }
        while rows.len() < EXPECTED_ROWS {
            let repo = others[other_i % others.len()];
            let id = format!("{}-{}", repo.replace('/', "__"), 100 + other_i);
            rows.push(row(&id, repo, "parse_log_maven", &cargo_image(&id)));
            other_i += 1;
        }
        rows
    }

    #[test]
    fn selection_picks_exactly_the_rust_repos_in_dataset_order() {
        let dataset = synthetic_dataset();
        let slice = select_rust_rows(&dataset);
        assert_eq!(slice.len(), EXPECTED_RUST_ROWS);
        assert!(
            slice
                .iter()
                .all(|row| RUST_REPOS.contains(&row.repo.as_str()))
        );
        // Dataset order is preserved: the slice's positions in the full
        // set are strictly increasing.
        let positions: Vec<usize> = dataset
            .iter()
            .enumerate()
            .filter(|(_, row)| RUST_REPOS.contains(&row.repo.as_str()))
            .map(|(index, _)| index)
            .collect();
        let slice_ids: Vec<&str> = slice.iter().map(|r| r.instance_id.as_str()).collect();
        let expected: Vec<&str> = positions
            .iter()
            .map(|&i| dataset[i].instance_id.as_str())
            .collect();
        assert_eq!(slice_ids, expected);
        check_slice_integrity(&dataset, &slice).expect("synthetic dataset is well-formed");
    }

    #[test]
    fn integrity_refuses_row_drift() {
        let mut dataset = synthetic_dataset();
        dataset.pop(); // 299 rows
        let slice = select_rust_rows(&dataset);
        let error = check_slice_integrity(&dataset, &slice).unwrap_err();
        assert!(error.contains("299"), "{error}");
    }

    #[test]
    fn integrity_refuses_slice_drift() {
        let dataset = synthetic_dataset();
        // Drop a repo from the selection: 43 -> 41 tasks.
        let slice: Vec<_> = select_rust_rows(&dataset)
            .into_iter()
            .filter(|row| row.repo != "burntsushi/ripgrep")
            .collect();
        let error = check_slice_integrity(&dataset, &slice).unwrap_err();
        assert!(error.contains("41 tasks"), "{error}");
    }

    #[test]
    fn integrity_refuses_non_cargo_parsers_and_bad_images() {
        let mut dataset = synthetic_dataset();
        let rust_at = dataset
            .iter()
            .position(|row| RUST_REPOS.contains(&row.repo.as_str()))
            .expect("a rust row");
        // A "rust" repo row that no longer parses with cargo.
        dataset[rust_at].log_parser = "parse_log_go".to_owned();
        let slice = select_rust_rows(&dataset);
        let error = check_slice_integrity(&dataset, &slice).unwrap_err();
        assert!(error.contains("parse_log_go"), "{error}");

        let mut dataset = synthetic_dataset();
        let rust_at = dataset
            .iter()
            .position(|row| RUST_REPOS.contains(&row.repo.as_str()))
            .expect("a rust row");
        dataset[rust_at].image = "docker.io/weird/image:nightly".to_owned();
        let slice = select_rust_rows(&dataset);
        let error = check_slice_integrity(&dataset, &slice).unwrap_err();
        assert!(error.contains("refusing to guess"), "{error}");
    }

    #[test]
    fn image_shape_matches_all_observed_names() {
        // Every shape observed in the real 300-row fetch.
        for good in [
            "swebench/sweb.eval.x86_64.astral-sh_1776_ruff-15309:latest",
            "swebench/sweb.eval.x86_64.uutils_1776_coreutils-6377:latest",
            "swebench/sweb.eval.x86_64.nushell_1776_nushell-12901:latest",
        ] {
            assert!(image_is_wellformed(good), "{good}");
        }
        for bad in [
            "",
            "swebench/sweb.eval.arm64.ruff-1:latest",
            "swebench/sweb.eval.x86_64.:latest",
            "swebench/sweb.eval.x86_64.Ruff-1:latest",
            "ghcr.io/other/ruff:latest",
            "swebench/sweb.eval.x86_64.ruff-1",
        ] {
            assert!(!image_is_wellformed(bad), "{bad}");
        }
    }

    #[test]
    fn multilingual_rows_parse_both_test_list_encodings() {
        let body = [
            r#"{"instance_id":"tokio-rs__tokio-1","repo":"tokio-rs/tokio","base_commit":"c","image":"swebench/sweb.eval.x86_64.tokio-rs_1776_tokio-1:latest","log_parser":"parse_log_cargo","problem_statement":"p","FAIL_TO_PASS":"[\"a::b\"]","PASS_TO_PASS":[]}"#,
            r#"{"instance_id":"tokio-rs__tokio-2","repo":"tokio-rs/tokio","base_commit":"c","image":"swebench/sweb.eval.x86_64.tokio-rs_1776_tokio-2:latest","log_parser":"parse_log_cargo","problem_statement":"p","FAIL_TO_PASS":["c"],"PASS_TO_PASS":""}"#,
        ]
        .join("\n");
        let rows = parse_multilingual(&body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fail_to_pass, ["a::b"]);
        assert_eq!(rows[1].fail_to_pass, ["c"]);
        assert!(rows[1].pass_to_pass.is_empty());
        assert_eq!(rows[0].version, "");
        assert_eq!(rows[0].hints_text, "");
        // The scorer-side extras arrive and are skipped without error.
        let round_trip = serde_json::to_string(&rows[0]).unwrap();
        assert!(round_trip.contains("parse_log_cargo"));
        assert!(parse_multilingual(&round_trip)[0].image.contains("tokio-1"));
    }

    #[test]
    #[should_panic(expected = "dataset line 2")]
    fn malformed_multilingual_lines_fail_loudly() {
        parse_multilingual(
            "{\"instance_id\":\"ok\",\"repo\":\"tokio-rs/tokio\",\"base_commit\":\"c\",\"image\":\"i\",\"log_parser\":\"parse_log_cargo\",\"problem_statement\":\"p\"}\n{not json}\n",
        );
    }

    #[test]
    fn prompt_is_byte_identical_to_the_verified_prompt() {
        // The whole point of sharing `prompt_text`: same repo, issue,
        // and hints must produce the same instruction text on either
        // dataset type, hints or no hints.
        let ml = row(
            "tokio-rs__tokio-1",
            "tokio-rs/tokio",
            "parse_log_cargo",
            &cargo_image("tokio-rs__tokio-1"),
        );
        let swe = SweInstance {
            instance_id: ml.instance_id.clone(),
            repo: ml.repo.clone(),
            base_commit: ml.base_commit.clone(),
            patch: String::new(),
            test_patch: String::new(),
            problem_statement: ml.problem_statement.clone(),
            hints_text: ml.hints_text.clone(),
            fail_to_pass: Vec::new(),
            pass_to_pass: Vec::new(),
        };
        assert_eq!(build_prompt_ml(&ml), swebench::build_prompt(&swe));

        let mut hinted = ml.clone();
        hinted.hints_text = "see the tracing docs".to_owned();
        let mut hinted_swe = swe;
        hinted_swe.hints_text = hinted.hints_text.clone();
        assert_eq!(
            build_prompt_ml(&hinted),
            swebench::build_prompt(&hinted_swe)
        );
        assert!(build_prompt_ml(&hinted).contains("<hints>\nsee the tracing docs"));
    }

    #[test]
    fn rust_script_pins_by_hash_and_shares_the_python_tail() {
        // Frozen: any edit here is a deliberate, reviewable change.
        assert_eq!(
            swebench::sha256_hex(RUST_CONTAINER_SCRIPT.as_bytes()),
            "7e267a0f913d095c5a104fd381a6d51aede3c4305afa6f01c554a048fb211780",
            "RUST_CONTAINER_SCRIPT changed; update provenance consciously"
        );
        // The manifest carries the Rust toolchain the agent needs...
        for program in ["cargo", "cargo-fmt", "rustc", "rustup"] {
            assert!(
                RUST_CONTAINER_SCRIPT.contains(&format!("program = \"{program}\"")),
                "missing {program}"
            );
        }
        // ...and everything after the heredoc EOF is byte-identical to
        // the Python arm's script: same invocation, same patch
        // extraction, same exit-status contract.
        let rust_tail = RUST_CONTAINER_SCRIPT
            .split("EOF\n")
            .nth(1)
            .expect("script tail after the heredoc");
        let python_tail = swebench::CONTAINER_SCRIPT
            .split("EOF\n")
            .nth(1)
            .expect("script tail after the heredoc");
        assert_eq!(rust_tail, python_tail);
        // The Python script keeps its own frozen bytes untouched.
        assert!(!swebench::CONTAINER_SCRIPT.contains("program = \"cargo\""));
    }

    #[test]
    fn script_override_swaps_the_script_in_docker_argv() {
        use crate::swebench::{AgentKind, ContainerSpec, docker_run_argv};
        let spec = ContainerSpec {
            agent: AgentKind::Loom,
            script_override: Some(RUST_CONTAINER_SCRIPT),
            image: "swebench/sweb.eval.x86_64.tokio-rs_1776_tokio-4384:latest",
            loom_binary: Some(std::path::Path::new("/eval/loom-linux/loom")),
            prompt_file: std::path::Path::new("/eval/multilingual/tokio-4384/prompt.txt"),
            out_dir: std::path::Path::new("/eval/multilingual/tokio-4384/run-1"),
            provider: "deepseek",
            model: "deepseek-flash",
            base_url: None,
            api_key_env: "DEEPSEEK_API_KEY",
            api_key: "sk-test",
            timeout_secs: 3600,
            max_turns: 100,
            bare: false,
            extra_env: &[],
            container_name: "loom-tokio-4384-run1",
        };
        let argv = docker_run_argv(&spec);
        let script_pos = argv
            .iter()
            .position(|arg| arg == "-c")
            .expect("sh -c script element");
        assert_eq!(argv[script_pos + 1], RUST_CONTAINER_SCRIPT);
        // The dataset's own image name rides through verbatim.
        assert!(
            argv.contains(&"swebench/sweb.eval.x86_64.tokio-rs_1776_tokio-4384:latest".to_owned())
        );
    }

    #[test]
    fn provenance_names_the_rule_and_pins_the_artifacts() {
        let dataset = synthetic_dataset();
        let jsonl = dataset
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let provenance = slice_provenance(&jsonl, EXPECTED_RUST_ROWS);
        assert_eq!(provenance.dataset, "SWE-bench/SWE-bench_Multilingual");
        assert_eq!(provenance.selection_rule, "rust-repos-v1");
        assert_eq!(provenance.repos.len(), 7);
        assert_eq!(provenance.rows_total, 300);
        assert_eq!(provenance.rows_selected, 43);
        assert_eq!(
            provenance.dataset_sha256,
            swebench::sha256_hex(jsonl.as_bytes())
        );
        assert_eq!(
            provenance.container_script_sha256,
            swebench::sha256_hex(RUST_CONTAINER_SCRIPT.as_bytes())
        );
        assert_eq!(provenance.prompt_version, swebench::PROMPT_VERSION);
    }
}
