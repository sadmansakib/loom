//! SWE-bench Verified plumbing: dataset loading, instance selection,
//! prompt construction, container-run assembly, and predictions
//! emission for the official docker scorer.
//!
//! The measurement contract (roadmap "Measurement"): the agent runs in
//! complete isolation inside the official per-instance testbed image —
//! repo checked out at `base_commit`, correct interpreter and
//! dependencies, no access to the host working tree. Grading belongs
//! to the official `swebench` harness, which applies its own
//! `test_patch` and decides `resolved`. Loom's job ends at a
//! predictions JSONL plus per-run artifacts.
//!
//! Everything here is offline-testable except the two network edges
//! (dataset fetch, docker), which the `swebench-run` binary owns.

// A test harness: panicking on a setup failure is the intended behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
/// `["a","b"]`; an empty string is the empty list.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
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

/// The standard SWE-bench task prompt: the problem statement verbatim,
/// the repository context, and the working-tree contract the patch
/// extraction relies on. Frozen like every other prompt — changing it
/// invalidates every baseline recorded against it. (v2, 2026-10-21:
/// added the iterate-early, run-existing-tests, and scratch-cleanup
/// directives after the first 5-instance sweep; v1 baseline = 3/5
/// resolved on astropy. v3, 2026-10-22: added the reproduce-and-verify
/// and scope-minimality directives after the 25-instance deepseek-flash
/// sweep diagnosed two process misses; v2 baseline = 23/25. Both v3
/// rules mirror the field-standard prompts — SWE-agent's
/// always-replicate-first tip and the standard harness's
/// minimal-changes-to-non-tests step.)
pub fn build_prompt(instance: &SweInstance) -> String {
    let mut prompt = format!(
        "You are an autonomous software engineer fixing a real issue in a GitHub repository.\n\n\
         Repository: {repo}\n\
         The repository is checked out at the issue's base commit in your working \
         directory. You have file tools and a shell: explore the code and edit the \
         source to fix it.\n\n\
         Rules:\n\
         - Fix the root cause in the source; do not paper over the symptom.\n\
         - Fix exactly the reported defect: no new restrictions, errors on \
         previously working inputs, API changes, or renames unless the issue \
         explicitly asks for them. Behavior the issue does not mention must \
         keep working.\n\
         - Once you have a workable hypothesis, start editing and iterate from \
         there; do not keep exploring toward certainty — you cannot verify the \
         fix by reading alone.\n\
         - Reproduce the reported behavior with a minimal script — the issue's \
         own example when it has one — that fails (nonzero exit) while the bug \
         is present; after the fix, run it again and confirm it passes and that \
         the behavior matches what the issue says is correct, including any \
         exact expected values it states.\n\
         - Before finishing, run the repository's existing tests that cover the \
         files you changed, and make sure they still pass.\n\
         - Delete any scratch or reproduction scripts you created along the way; \
         only the fix itself should remain in the tree.\n\
         - Do not commit, branch, or tag; leave your changes in the working tree.\n\
         - Do not modify or delete the .git directory.\n\n\
         The issue follows.\n\n\
         <problem_statement>\n{problem}\n</problem_statement>\n",
        repo = instance.repo,
        problem = instance.problem_statement,
    );
    if !instance.hints_text.trim().is_empty() {
        prompt.push_str(&format!(
            "\n<hints>\n{}\n</hints>\n",
            instance.hints_text.trim()
        ));
    }
    prompt.push_str(
        "\nWhen the fix is complete, reply with a short summary of what you changed and why, and finish — do not wait for further input.\n",
    );
    prompt
}

// ------------------------------------------------- container assembly ---

/// The official per-instance testbed image reference. The swebench
/// harness names evaluation images `sweb.eval.<arch>.<instance_id>`
/// with the id lowercased and `/`/`:` folded to `_`; on arm64 the
/// local build (`swebench images build SWE-bench/swe-bench-tasks
/// --dataset verified -i <id>`) produces exactly this tag.
pub fn testbed_image(instance_id: &str) -> String {
    format!(
        "sweb.eval.x86_64.{}:latest",
        instance_id.to_lowercase().replace(['/', ':'], "_")
    )
}

/// The name the official scorer looks the testbed image up under:
/// Docker Hub rejects dunders in image names, so the harness rewrites
/// every `__` in the key to `_1776_` and prefixes the namespace
/// (`swebench/image_builder/image_spec.py`). Locally built images carry
/// the plain tag, so scoring needs a one-time
/// `docker tag <testbed_image> <scored_image>` alias per instance —
/// otherwise the harness tries to pull from Docker Hub and 404s.
pub fn scored_image(instance_id: &str) -> String {
    format!(
        "swebench/sweb.eval.x86_64.{}:latest",
        instance_id.to_lowercase().replace("__", "_1776_")
    )
}

/// Env vars that may carry the provider's API key, mirroring the
/// engine's auth table (`loom-engine` `provider_env_names`). The first
/// one set on the host is forwarded into the container.
pub fn api_key_envs(provider: &str) -> Vec<String> {
    match provider {
        "deepseek" => vec!["DEEPSEEK_API_KEY".into()],
        "anthropic" => vec!["ANTHROPIC_API_KEY".into()],
        "openrouter" => vec!["OPENROUTER_API_KEY".into()],
        "glm" => vec!["ZAI_API_KEY".into(), "Z_AI_API_KEY".into()],
        "nvidia" => vec!["NVIDIA_API_KEY".into(), "NIM_API_KEY".into()],
        "groq" => vec!["GROQ_API_KEY".into()],
        "custom" => vec!["CUSTOM_API_KEY".into()],
        // A named provider (`loom provider add`) derives its env var
        // the same way the engine does (`registry::env_var_name`).
        other => vec![loom_core::engine::registry::env_var_name(other)],
    }
}

/// The script the container runs. Values arrive as environment
/// (`LOOM_*` + the provider key) so the script itself needs no host
/// interpolation. The patch is extracted even when loom fails or times
/// out — partial work still scores.
///
/// The container *is* the isolation boundary, so the in-container
/// command sandbox is passthrough: the testbed images ship no
/// `bubblewrap`, nested user namespaces are blocked under docker
/// anyway, and the seeded `.loom/commands.toml` admits the toolchain
/// (python/pip/shell/git) with `sandbox = false`. `LOOM_SKIP_SANDBOX_
/// PREFLIGHT` skips the bwrap probe for the same reason. The manifest
/// is removed before the diff so it never leaks into `model_patch`.
pub const CONTAINER_SCRIPT: &str = r#"mkdir -p /testbed/.loom && cat > /testbed/.loom/commands.toml <<'EOF'
[[command]]
program = "bash"
sandbox = false

[[command]]
program = "sh"
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

/// Which agent the container runs. `Loom` is the product under test;
/// `Mini` swaps in mini-swe-agent — the published 100-line bash-only
/// baseline — while the prompt, image, patch extraction, and scoring
/// stay byte-identical, so a loom-vs-mini contrast isolates the agent
/// loop itself rather than the harness around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    /// The staged loom binary (`CONTAINER_SCRIPT`).
    #[default]
    Loom,
    /// mini-swe-agent from PyPI (`MINI_CONTAINER_SCRIPT`).
    Mini,
}

/// The script for the [`AgentKind::Mini`] baseline arm. Same contract
/// as [`CONTAINER_SCRIPT`]: env in, `patch.diff` + `run.err` out, the
/// patch extracted even when the agent fails or times out — partial
/// work still scores. The agent runs through litellm's OpenAI
/// provider, so the model becomes `openai/<name>` and the endpoint is
/// `OPENAI_BASE_URL`/`OPENAI_API_BASE`; the key arrives as
/// `OPENAI_API_KEY` (the runner renames the provider key to match).
/// No `/out/run.json`: mini emits no event stream, so turn/token
/// metrics read as zero for this arm and contrasts carry score deltas
/// only. Their shipped `swebench.yaml` is used verbatim — its prompts
/// and step limit ARE the baseline being measured.
pub const MINI_CONTAINER_SCRIPT: &str = r#"python -m pip install -q mini-swe-agent && CFG=$(python -c 'import pathlib, minisweagent; print(pathlib.Path(minisweagent.__file__).parent / "config" / "swebench.yaml")') && cd /testbed && export OPENAI_BASE_URL="$LOOM_BASE_URL" OPENAI_API_BASE="$LOOM_BASE_URL" && timeout "$LOOM_TIMEOUT_SECS" mini --config "$CFG" --model "openai/$LOOM_MODEL" -t "$(cat /prompt.txt)" --exit-immediately > /out/run.log 2> /out/run.err; status=$?; cd /testbed && git add -A && git diff --cached > /out/patch.diff 2> /out/patch.err; exit $status"#;

impl AgentKind {
    /// Parses the `--agent` flag value (`loom` / `mini`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "loom" => Some(Self::Loom),
            "mini" => Some(Self::Mini),
            _ => None,
        }
    }
}

/// The container script for an agent kind.
pub fn container_script(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Loom => CONTAINER_SCRIPT,
        AgentKind::Mini => MINI_CONTAINER_SCRIPT,
    }
}
/// One isolated agent run: a container per (instance, run).
pub struct ContainerSpec<'a> {
    /// Docker image (see [`testbed_image`]).
    pub image: &'a str,
    /// Host path to the linux loom binary; bind-mounted at `/loom/loom`.
    /// `None` for arms that run no loom build (mini) — no mount, and
    /// provenance records no digest for the sweep.
    pub loom_binary: Option<&'a Path>,
    /// Host path to the prompt file; bind-mounted read-only at `/prompt.txt`.
    pub prompt_file: &'a Path,
    /// Host output directory; bind-mounted at `/out` (run.json, run.err,
    /// patch.diff, patch.err land here).
    pub out_dir: &'a Path,
    /// `--provider` value for the in-container loom run.
    pub provider: &'a str,
    /// `--model` value for the in-container loom run.
    pub model: &'a str,
    /// Chat API base URL for OpenAI-compatible gateways (the `custom`
    /// provider). Forwarded as `LOOM_BASE_URL` and injected into the
    /// loom invocation as `--base-url`; `None` for the vendors whose
    /// provider adapters ship their own default endpoint.
    pub base_url: Option<&'a str>,
    /// Env var name that carries the API key (see [`api_key_envs`]).
    pub api_key_env: &'a str,
    /// The API key value forwarded via `-e`.
    pub api_key: &'a str,
    /// Which agent the container runs (see [`AgentKind`]). Controls the
    /// script `docker_run_argv` executes; the loom binary mount stays
    /// for both arms — provenance hashes it either way.
    pub agent: AgentKind,
    /// Wall-clock budget for the whole loom run, in seconds.
    pub timeout_secs: u64,
    /// `--max-turns` budget for the in-container loom run.
    pub max_turns: usize,
    /// Selects the bare-loop arm (roadmap item 1): forwards `LOOM_BARE=1`
    /// so the in-container binary strips every harness lever (facts,
    /// pruning, completion gate, failure ledger, fast paths). The
    /// container script itself stays byte-identical — the env var is
    /// consumed by the loom binary, like `LOOM_SKIP_SANDBOX_PREFLIGHT`.
    pub bare: bool,
    /// Additional `LOOM_*` environment overlay for the in-container
    /// engine (roadmap item 20.3, the self-optimization loop's probes).
    ///
    /// Each pair is forwarded as `-e K=V` after the fixed env above and
    /// before the volumes; the container script stays byte-identical —
    /// the in-container engine applies these through the same
    /// `LOOM_*` override seams (`budgets_with_env`, `prune_with_env`)
    /// that a shell user would set. Names colliding with the fixed set
    /// ([`RESERVED_CONTAINER_ENV`]) are a bug in the caller and panic
    /// here rather than silently shadowing the run's own provider or
    /// budget.
    pub extra_env: &'a [(String, String)],
    /// Unique container name (`--name`), e.g. `loom-<slug>-run1`.
    pub container_name: &'a str,
}

/// Env names `ContainerSpec` already owns; [`ContainerSpec::extra_env`]
/// entries with these names panic in [`docker_run_argv`].
pub const RESERVED_CONTAINER_ENV: [&str; 7] = [
    "LOOM_PROVIDER",
    "LOOM_MODEL",
    "LOOM_BASE_URL",
    "LOOM_BARE",
    "LOOM_SKIP_SANDBOX_PREFLIGHT",
    "LOOM_TIMEOUT_SECS",
    "LOOM_MAX_TURNS",
];

/// Assembles the `docker run` argv for one container run. Pure argv —
/// no host shell — so paths and values survive verbatim.
pub fn docker_run_argv(spec: &ContainerSpec<'_>) -> Vec<String> {
    for (name, _) in spec.extra_env {
        assert!(
            !RESERVED_CONTAINER_ENV.contains(&name.as_str()),
            "extra_env collides with a fixed container env var: {name}"
        );
        assert!(
            name != spec.api_key_env,
            "extra_env collides with the API key env var: {name}"
        );
    }
    let prompt_mount = format!("{}:/prompt.txt:ro", spec.prompt_file.display());
    let out_mount = format!("{}:/out", spec.out_dir.display());
    let timeout = spec.timeout_secs.to_string();
    let max_turns = spec.max_turns.to_string();
    let mut argv = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        spec.container_name.into(),
        "-e".into(),
        format!("LOOM_PROVIDER={}", spec.provider),
        "-e".into(),
        format!("LOOM_MODEL={}", spec.model),
    ];
    // Only loom-running arms carry the binary; mini needs no mount.
    let loom_mount = spec
        .loom_binary
        .map(|loom_binary| format!("{}:/loom/loom:ro", loom_binary.display()));
    if let Some(base_url) = spec.base_url {
        argv.extend(["-e".into(), format!("LOOM_BASE_URL={base_url}")]);
    }
    if spec.bare {
        argv.extend(["-e".into(), "LOOM_BARE=1".into()]);
    }
    for (name, value) in spec.extra_env {
        argv.extend(["-e".into(), format!("{name}={value}")]);
    }
    argv.extend([
        "-e".into(),
        "LOOM_SKIP_SANDBOX_PREFLIGHT=1".into(),
        "-e".into(),
        format!("LOOM_TIMEOUT_SECS={timeout}"),
        "-e".into(),
        format!("LOOM_MAX_TURNS={max_turns}"),
        "-e".into(),
        format!("{}={}", spec.api_key_env, spec.api_key),
        "-v".into(),
        prompt_mount,
        "-v".into(),
        out_mount,
    ]);
    if let Some(loom_mount) = loom_mount {
        argv.extend(["-v".into(), loom_mount]);
    }
    argv.extend([
        spec.image.into(),
        "sh".into(),
        "-c".into(),
        container_script(spec.agent).into(),
    ]);
    argv
}

/// Reads a run's JSONL event stream and reduces it to what the sweep
/// report needs: the event count and the terminal `task_failed` reason
/// (if any). The stream is line-delimited, so a whole-file
/// `from_str` would always fail; this walks events instead.
pub fn run_outcome(run_json: &Path) -> (usize, Option<String>) {
    let Ok(body) = std::fs::read_to_string(run_json) else {
        return (0, None);
    };
    let mut events = 0usize;
    let mut failure = None;
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        events += 1;
        if value["kind"]["type"] == "task_failed"
            && let Some(reason) = value["kind"]["reason"].as_str()
        {
            failure = Some(reason.to_owned());
        }
    }
    (events, failure)
}

/// Model turns in a run's JSONL event stream: the count of
/// `turn_started` events (the terminal `task_completed` event carries
/// no fields, so this is the only turn signal the artifacts hold).
pub fn run_turns(run_json: &Path) -> u64 {
    let Ok(body) = std::fs::read_to_string(run_json) else {
        return 0;
    };
    body.lines()
        .filter(|line| line.contains("\"turn_started\""))
        .count() as u64
}

/// The per-run numbers a paired contrast reports (roadmap item 1):
/// turns plus the token totals from [`run_usage`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RunMetrics {
    /// `turn_started` events.
    pub turns: u64,
    /// Token totals across model requests.
    pub usage: RunUsage,
}

/// Reads a run's turns and token totals in one pass.
pub fn run_metrics(run_json: &Path) -> RunMetrics {
    RunMetrics {
        turns: run_turns(run_json),
        usage: run_usage(run_json),
    }
}

/// Outcome map from an official scorer report: instance id → resolved.
/// The scorer writes a wrapper object whose `*_ids` arrays partition the
/// run — `resolved_ids` true; `unresolved_ids`, `incomplete_ids`,
/// `error_ids`, `empty_patch_ids`, `infra_failure_ids`, and
/// `ambiguous_failure_ids` false.
pub fn scorer_outcomes(report_json: &Path) -> std::io::Result<Vec<(String, bool)>> {
    /// The scorer's id partitions; every other `*_ids` key (e.g.
    /// `unremoved_images`) is bookkeeping, not outcomes.
    fn partition(key: &str) -> Option<bool> {
        match key {
            "resolved_ids" | "resolved_instances" => Some(true),
            // `incomplete_*` ids were never submitted — they carry no
            // outcome and stay out of the map entirely, so contrasts
            // pair attempted work only.
            "incomplete_ids" | "incomplete_instances" => None,
            "unresolved_ids"
            | "unresolved_instances"
            | "error_ids"
            | "error_instances"
            | "empty_patch_ids"
            | "empty_patch_instances"
            | "infra_failure_ids"
            | "infra_failure_instances"
            | "ambiguous_failure_ids"
            | "ambiguous_failure_instances" => Some(false),
            _ => None,
        }
    }

    let body = std::fs::read_to_string(report_json)?;
    let value: serde_json::Value = serde_json::from_str(&body)?;
    let mut outcomes = std::collections::BTreeMap::new();
    for (key, entry) in value.as_object().into_iter().flatten() {
        // An id appears in several partitions (`submitted_ids` overlaps
        // `resolved_ids`); resolution is sticky and never downgraded.
        let Some(listed_resolved) = partition(key) else {
            continue;
        };
        for id in entry.as_array().into_iter().flatten() {
            if let Some(id) = id.as_str() {
                let resolved = outcomes.get(id).copied().unwrap_or(false) || listed_resolved;
                outcomes.insert(id.to_owned(), resolved);
            }
        }
    }
    Ok(outcomes.into_iter().collect())
}

/// Token totals reduced from a run's JSONL event stream: summed over
/// every `model_completed` event's usage block. `cached_input_tokens`
/// stays `None` until some event reports a cache split, so a provider
/// that never reports one is visibly unmeasured rather than reading as
/// a misleading 0% hit rate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    /// Summed prompt tokens across model requests.
    pub input_tokens: u64,
    /// Summed cache-hit share of the prompt, once any request
    /// reported it.
    pub cached_input_tokens: Option<u64>,
    /// Summed completion tokens.
    pub output_tokens: u64,
    /// Number of model requests that carried a usage block.
    pub model_requests: u64,
}

impl RunUsage {
    /// Folds one `model_completed` usage object into the totals.
    fn observe(&mut self, usage: &serde_json::Value) {
        self.model_requests += 1;
        self.input_tokens += usage["input_tokens"].as_u64().unwrap_or(0);
        self.output_tokens += usage["output_tokens"].as_u64().unwrap_or(0);
        let cached = usage["cached_input_tokens"].as_u64();
        self.cached_input_tokens = match (self.cached_input_tokens, cached) {
            (Some(accumulated), Some(cached)) => Some(accumulated + cached),
            (None, Some(cached)) => Some(cached),
            (accumulated, None) => accumulated,
        };
    }

    /// Merge another run's totals into these (sweep-level roll-up).
    pub fn merge(&mut self, other: RunUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.model_requests += other.model_requests;
        self.cached_input_tokens = match (self.cached_input_tokens, other.cached_input_tokens) {
            (Some(accumulated), Some(cached)) => Some(accumulated + cached),
            (None, cached) => cached,
            (accumulated, None) => accumulated,
        };
    }

    /// Cache-hit share of input tokens, when this run reported any
    /// split at all.
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        self.cached_input_tokens
            .filter(|_| self.input_tokens > 0)
            .map(|cached| cached as f64 / self.input_tokens as f64)
    }
}

/// Reads a run's JSONL event stream and sums every model response's
/// usage into [`RunUsage`] (the per-request split providers report on
/// the authoritative `model_completed` event).
pub fn run_usage(run_json: &Path) -> RunUsage {
    let mut totals = RunUsage::default();
    let Ok(body) = std::fs::read_to_string(run_json) else {
        return totals;
    };
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["kind"]["type"] == "model_completed" {
            totals.observe(&value["kind"]["response"]["usage"]);
        }
    }
    totals
}

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
pub fn write_predictions(path: &Path, rows: &[Prediction<'_>]) {
    let body = rows
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, body).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// Filesystem-safe run id: `/` and `:` folded to `-` (the same slug
/// rule the in-house harness used).
pub fn slug(id: &str) -> String {
    id.replace(['/', ':'], "-")
}

// ------------------------------------------------------- provenance ---

/// The frozen prompt's version marker (doc comment on
/// [`build_prompt`] is the changelog; this is the machine-readable
/// digest input). Bumping it invalidates every recorded baseline.
pub const PROMPT_VERSION: &str = "3";

/// Provenance stamped into every sweep report so a headline number is
/// attributable to one engine build, one container script, and one
/// prompt version (honesty rules in docs/eval.md). The binary digest
/// is the strong claim: the staged linux `loom` is hashed on the host
/// (executing the ELF is impossible there), so a rebuilt binary can
/// never silently extend a sweep recorded under the old build — the
/// exact failure behind the v3-100 headline spanning two builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepProvenance {
    /// sha256 of the staged linux loom binary, hex. `None` for agent
    /// arms that run no loom build (mini) — there is no engine build
    /// to attribute, and the span guard treats it like the
    /// pre-provenance legacy case.
    pub loom_sha256: Option<String>,
    /// Size of the staged binary in bytes (cheap cross-check).
    pub loom_bytes: Option<u64>,
    /// sha256 of the frozen container script, hex.
    pub container_script_sha256: String,
    /// [`PROMPT_VERSION`] at sweep time.
    pub prompt_version: &'static str,
    /// loom-eval's own package version (the runner's identity).
    pub runner_version: &'static str,
}

/// sha256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Reads and hashes the staged binary (when the arm runs one); fails
/// loudly rather than stamping a report with no provenance.
pub fn sweep_provenance(
    loom_bin: Option<&Path>,
    agent: AgentKind,
) -> std::io::Result<SweepProvenance> {
    let loom_sha256 = match loom_bin {
        Some(loom_bin) => {
            let bytes = std::fs::read(loom_bin)?;
            Some((sha256_hex(&bytes), bytes.len() as u64))
        }
        None => None,
    };
    Ok(SweepProvenance {
        loom_sha256: loom_sha256.as_ref().map(|(digest, _)| digest.clone()),
        loom_bytes: loom_sha256.map(|(_, bytes)| bytes),
        container_script_sha256: sha256_hex(container_script(agent).as_bytes()),
        prompt_version: PROMPT_VERSION,
        runner_version: env!("CARGO_PKG_VERSION"),
    })
}

/// The digest recorded in a sweep report's `provenance.loom_sha256`
/// (absent in pre-2026-10-25 reports).
pub fn recorded_loom_digest(sweep_json: &serde_json::Value) -> Option<&str> {
    sweep_json.get("provenance")?.get("loom_sha256")?.as_str()
}

/// One headline, one engine build: aggregating (contrasting, summing)
/// sweep reports that record *different* binary digests mixes builds
/// into one number and is refused. Reports without provenance
/// predate the rule and pass — callers warn on them separately.
pub fn span_one_build(left: &serde_json::Value, right: &serde_json::Value) -> Result<(), String> {
    let (Some(left), Some(right)) = (recorded_loom_digest(left), recorded_loom_digest(right))
    else {
        return Ok(());
    };
    if left == right {
        Ok(())
    } else {
        Err(format!(
            "sweep reports span two engine builds ({left} vs {right}); \
             a headline number must come from one build — re-run the \
             later batch under a single binary before aggregating"
        ))
    }
}

// ------------------------------------------------------------ tests ---

/// A two-instance fixture exercising both test-list encodings.
#[cfg(test)]
fn fixture_jsonl() -> String {
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

#[test]
fn prompt_carries_the_problem_and_the_tree_contract() {
    let instances = parse_dataset(&fixture_jsonl());
    let prompt = build_prompt(&instances[0]);
    assert!(prompt.contains("Repository: org/one"));
    assert!(prompt.contains("It crashes."));
    assert!(prompt.contains("<problem_statement>"));
    assert!(prompt.contains("leave your changes in the working tree"));
    // v2 directives: iterate early, run existing tests, clean scratch.
    assert!(prompt.contains("start editing and iterate"));
    assert!(prompt.contains("existing tests that cover the"));
    assert!(prompt.contains("Delete any scratch or reproduction scripts"));
    assert!(prompt.contains("and finish — do not wait for further input"));
    // v3 directives: scope minimality, reproduce-and-verify.
    assert!(prompt.contains("no new restrictions"));
    assert!(prompt.contains("that fails (nonzero exit) while the bug"));
    assert!(prompt.contains("including any exact expected values it states"));
    // No hints in the fixture → no hints block.
    assert!(!prompt.contains("<hints>"));

    let mut hinted = instances[1].clone();
    hinted.hints_text = "see docs/x".to_owned();
    assert!(build_prompt(&hinted).contains("<hints>\nsee docs/x"));
}

#[test]
fn testbed_image_folds_instance_ids_like_the_harness() {
    // Classic Verified ids are already `org__repo-num`; fold anyway so
    // variants with `/` or `:` still map to the harness's tag rule.
    assert_eq!(
        testbed_image("sympy__sympy-20590"),
        "sweb.eval.x86_64.sympy__sympy-20590:latest"
    );
    assert_eq!(
        testbed_image("Django/Django:16379"),
        "sweb.eval.x86_64.django_django_16379:latest"
    );
}

#[test]
fn scored_image_applies_the_harness_dunder_rewrite() {
    assert_eq!(
        scored_image("astropy__astropy-12907"),
        "swebench/sweb.eval.x86_64.astropy_1776_astropy-12907:latest"
    );
    // Multi-dunder repo names rewrite every `__`.
    assert_eq!(
        scored_image("scikit-learn__scikit-learn-13240"),
        "swebench/sweb.eval.x86_64.scikit-learn_1776_scikit-learn-13240:latest"
    );
}

#[test]
fn api_key_envs_mirror_the_engine_auth_table() {
    assert_eq!(api_key_envs("deepseek"), ["DEEPSEEK_API_KEY"]);
    assert_eq!(api_key_envs("custom"), ["CUSTOM_API_KEY"]);
    assert_eq!(api_key_envs("glm"), ["ZAI_API_KEY", "Z_AI_API_KEY"]);
    // Named providers derive their key var the engine's way.
    assert_eq!(api_key_envs("orcarouter"), ["ORCAROUTER_API_KEY"]);
}

#[test]
fn docker_argv_mounts_and_env_without_a_shell() {
    let spec = ContainerSpec {
        agent: AgentKind::Loom,
        image: "sweb.eval.x86_64.org__one-1:latest",
        loom_binary: Some(Path::new("/eval/loom-linux/loom")),
        prompt_file: Path::new("/eval/swebench/instances/org-one-1/prompt-run1.txt"),
        out_dir: Path::new("/eval/swebench/instances/org-one-1/run-1"),
        provider: "deepseek",
        model: "deepseek-flash",
        base_url: None,
        api_key_env: "DEEPSEEK_API_KEY",
        api_key: "sk-test",
        timeout_secs: 3600,
        max_turns: 60,
        bare: false,
        extra_env: &[],
        container_name: "loom-org-one-1-run1",
    };
    let argv = docker_run_argv(&spec);
    let joined = argv.join("\n");
    assert_eq!(&argv[0], "run");
    assert!(joined.contains("--rm\n"));
    assert!(joined.contains("--name\nloom-org-one-1-run1\n"));
    assert!(joined.contains("-e\nLOOM_PROVIDER=deepseek\n-e\nLOOM_MODEL=deepseek-flash"));
    // Full arm: no LOOM_BARE is forwarded.
    assert!(!joined.contains("LOOM_BARE"));
    // The container is the isolation boundary; the in-container bwrap
    // probe would fail (no bubblewrap in testbed images).
    assert!(joined.contains("-e\nLOOM_SKIP_SANDBOX_PREFLIGHT=1"));
    assert!(joined.contains("-e\nLOOM_TIMEOUT_SECS=3600\n"));
    assert!(joined.contains("-e\nLOOM_MAX_TURNS=60\n"));
    assert!(joined.contains("-e\nDEEPSEEK_API_KEY=sk-test\n"));
    assert!(joined.contains("-v\n/eval/loom-linux/loom:/loom/loom:ro\n"));
    assert!(
        joined.contains("-v\n/eval/swebench/instances/org-one-1/prompt-run1.txt:/prompt.txt:ro\n")
    );
    assert!(joined.contains("-v\n/eval/swebench/instances/org-one-1/run-1:/out\n"));
    // The script is a single argv element passed to sh -c.
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], CONTAINER_SCRIPT);
    assert!(argv.contains(&"sweb.eval.x86_64.org__one-1:latest".to_owned()));
    // The script reads its knobs from env, extracts the patch even on
    // failure, and preserves loom's exit status.
    assert!(CONTAINER_SCRIPT.contains("timeout \"$LOOM_TIMEOUT_SECS\""));
    assert!(CONTAINER_SCRIPT.contains("--no-save"));
    assert!(CONTAINER_SCRIPT.contains("git diff --cached > /out/patch.diff"));
    assert!(CONTAINER_SCRIPT.ends_with("exit $status"));
    // It seeds a passthrough commands manifest (the container is the
    // sandbox) and removes it again so the patch stays clean.
    assert!(CONTAINER_SCRIPT.contains("/testbed/.loom/commands.toml"));
    assert!(CONTAINER_SCRIPT.contains("program = \"python3\""));
    assert!(CONTAINER_SCRIPT.contains("sandbox = false"));
    assert!(CONTAINER_SCRIPT.contains("rm -rf /testbed/.loom;"));
    // No gateway base URL → no LOOM_BASE_URL env assignment (the
    // script's own guard still references the variable, which stays
    // unset); a gateway spec below asserts the assignment appears.
    assert!(!joined.contains("-e\nLOOM_BASE_URL="));
    assert!(CONTAINER_SCRIPT.contains("set -- --base-url \"$LOOM_BASE_URL\""));

    let gateway = ContainerSpec {
        agent: AgentKind::Loom,
        base_url: Some("https://free.empexaura.com/v1"),
        provider: "custom",
        model: "nano-model",
        api_key_env: "CUSTOM_API_KEY",
        api_key: "nano-test",
        ..spec
    };
    let argv = docker_run_argv(&gateway);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nLOOM_BASE_URL=https://free.empexaura.com/v1"));
    assert!(joined.contains("-e\nCUSTOM_API_KEY=nano-test"));

    // The bare-loop arm (roadmap item 1): one env var, and the frozen
    // container script stays byte-identical — the binary consumes the
    // knob, not the script.
    let bare_arm = ContainerSpec {
        bare: true,
        ..gateway
    };
    let argv = docker_run_argv(&bare_arm);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nLOOM_BARE=1\n"));
    assert!(!CONTAINER_SCRIPT.contains("LOOM_BARE"));

    // The self-optimization overlay (roadmap item 20.3): forwarded after
    // the fixed env, and the frozen script still stays byte-identical —
    // the in-container engine consumes the knobs.
    let overlay = vec![
        ("LOOM_PRUNE_TRIGGER_PERCENT".to_owned(), "70".to_owned()),
        ("LOOM_READ_HEAD_BYTES".to_owned(), "32768".to_owned()),
    ];
    let overlaid = ContainerSpec {
        agent: AgentKind::Loom,
        extra_env: &overlay,
        container_name: "loom-org-one-1-selfopt-p1",
        ..bare_arm
    };
    let argv = docker_run_argv(&overlaid);
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], CONTAINER_SCRIPT);
    let joined = argv.join("\n");
    let overlay_pos = joined
        .find("-e\nLOOM_PRUNE_TRIGGER_PERCENT=70\n-e\nLOOM_READ_HEAD_BYTES=32768")
        .expect("overlay forwarded in order");
    let fixed_pos = joined
        .find("-e\nLOOM_TIMEOUT_SECS=3600")
        .expect("fixed env present");
    assert!(
        overlay_pos > joined.find("-e\nLOOM_BARE=1").unwrap(),
        "overlay comes after the fixed env"
    );
    assert!(
        overlay_pos < joined.find("-v\n/eval/loom-linux/loom").unwrap(),
        "overlay comes before the volumes"
    );
    let _ = fixed_pos;

    // Reserved names are caller bugs: shadowing the run's own provider,
    // budget, or key would corrupt the probe silently.
    for reserved in RESERVED_CONTAINER_ENV {
        let collision = ContainerSpec {
            extra_env: &[(reserved.to_owned(), "1".to_owned())],
            ..overlaid
        };
        let result = std::panic::catch_unwind(|| docker_run_argv(&collision));
        assert!(result.is_err(), "reserved name must panic: {reserved}");
    }
    let key_collision = ContainerSpec {
        extra_env: &[("CUSTOM_API_KEY".to_owned(), "x".to_owned())],
        ..overlaid
    };
    assert!(std::panic::catch_unwind(|| docker_run_argv(&key_collision)).is_err());
}

#[test]
fn run_outcome_reads_the_event_stream() {
    let dir = std::env::temp_dir().join(format!("loom-swe-outcome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.json");

    // Missing file: a run that never started.
    assert_eq!(run_outcome(&path), (0, None));

    // Healthy stream: events, no failure.
    std::fs::write(
        &path,
        "{\"schema_version\":10,\"sequence\":0,\"kind\":{\"type\":\"task_started\"}}\n\
         {\"schema_version\":10,\"sequence\":1,\"kind\":{\"type\":\"turn_started\"}}\n",
    )
    .unwrap();
    assert_eq!(run_outcome(&path), (2, None));

    // Rate-limited death: exactly what the orchestrator retries on.
    std::fs::write(
        &path,
        "{\"schema_version\":10,\"sequence\":0,\"kind\":{\"type\":\"task_started\"}}\n\
         {\"schema_version\":10,\"sequence\":9,\"kind\":{\"type\":\"task_failed\",\"reason\":\"model request failed: [rate_limited] HTTP 429\"}}\n",
    )
    .unwrap();
    let (events, failure) = run_outcome(&path);
    assert_eq!(events, 2);
    assert_eq!(
        failure.as_deref(),
        Some("model request failed: [rate_limited] HTTP 429")
    );

    // Malformed trailing junk must not sink the scan.
    std::fs::write(&path, "not json\n").unwrap();
    assert_eq!(run_outcome(&path), (0, None));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_turns_counts_turn_started_events() {
    let dir = std::env::temp_dir().join(format!("loom-swe-turns-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.json");

    // Missing file: a run that never started.
    assert_eq!(run_turns(&path), 0);

    // Three turns among other events; task_completed carries nothing.
    std::fs::write(
        &path,
        "{\"schema_version\":10,\"kind\":{\"type\":\"task_started\"}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"turn_started\"}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"model_completed\",\"response\":{}}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"turn_started\"}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"tool_completed\"}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"turn_started\"}}\n\
         {\"schema_version\":10,\"kind\":{\"type\":\"task_completed\"}}\n",
    )
    .unwrap();
    assert_eq!(run_turns(&path), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scorer_outcomes_reads_a_report_object() {
    let dir = std::env::temp_dir().join(format!("loom-swe-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.json");

    // Missing file is a caller error, not an empty map.
    assert!(scorer_outcomes(&path).is_err());

    std::fs::write(
        &path,
        "{\"total_instances\":500,\"resolved_ids\":[\"a__a-1\"],\
          \"unresolved_ids\":[\"b__b-2\"],\"error_ids\":[\"c__c-3\"],\
          \"incomplete_ids\":[\"d__d-4\"],\"unremoved_images\":[\"img-x\"]}",
    )
    .unwrap();
    let outcomes = scorer_outcomes(&path).unwrap();
    // d-4 was never submitted (incomplete) and img-x is bookkeeping:
    // neither carries an outcome.
    assert_eq!(
        outcomes,
        vec![
            ("a__a-1".to_owned(), true),
            ("b__b-2".to_owned(), false),
            ("c__c-3".to_owned(), false),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_usage_sums_model_completed_events() {
    let dir = std::env::temp_dir().join(format!("loom-swe-usage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.json");

    // Missing file: nothing observed.
    assert_eq!(run_usage(&path), RunUsage::default());

    // Two completed requests, one reporting a cache split, plus noise
    // events that must not contribute.
    std::fs::write(
        &path,
        "{\"schema_version\":10,\"sequence\":0,\"kind\":{\"type\":\"task_started\"}}\n\
         {\"schema_version\":10,\"sequence\":1,\"kind\":{\"type\":\"model_completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":50,\"cached_input_tokens\":900}}}}\n\
         {\"schema_version\":10,\"sequence\":2,\"kind\":{\"type\":\"tool_completed\"}}\n\
         {\"schema_version\":10,\"sequence\":3,\"kind\":{\"type\":\"model_completed\",\"response\":{\"usage\":{\"input_tokens\":2000,\"output_tokens\":80}}}}\n",
    )
    .unwrap();
    let totals = run_usage(&path);
    assert_eq!(totals.model_requests, 2);
    assert_eq!(totals.input_tokens, 3000);
    assert_eq!(totals.output_tokens, 130);
    assert_eq!(totals.cached_input_tokens, Some(900));
    let rate = totals.hit_rate().expect("hit rate measured");
    assert!((rate - 0.3).abs() < 1e-9, "{rate}");

    // A provider that never reports a split stays None, not 0%.
    std::fs::write(
        &path,
        "{\"schema_version\":10,\"sequence\":1,\"kind\":{\"type\":\"model_completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}}\n",
    )
    .unwrap();
    let silent = run_usage(&path);
    assert_eq!(silent.cached_input_tokens, None);
    assert_eq!(silent.hit_rate(), None);

    // Roll-up: merging keeps the split once any side measured one.
    let mut sweep = RunUsage::default();
    sweep.merge(totals);
    sweep.merge(silent);
    assert_eq!(sweep.input_tokens, 3010);
    assert_eq!(sweep.cached_input_tokens, Some(900));
    assert_eq!(sweep.model_requests, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

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
fn slug_folds_separators() {
    assert_eq!(slug("sympy__sympy-20590"), "sympy__sympy-20590");
    assert_eq!(slug("org/one-1"), "org-one-1");
    assert_eq!(slug("a:b"), "a-b");
}

#[test]
fn sha256_matches_the_known_vector() {
    // FIPS 180-4 test vector: sha256("abc").
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sweep_provenance_hashes_the_staged_binary_and_script() {
    let dir = std::env::temp_dir().join(format!("loom-prov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("loom");
    std::fs::write(&bin, b"pretend elf").unwrap();
    let provenance = sweep_provenance(Some(&bin), AgentKind::Loom).unwrap();
    assert_eq!(
        provenance.loom_sha256.as_deref(),
        Some(sha256_hex(b"pretend elf").as_str())
    );
    assert_eq!(provenance.loom_bytes, Some(11));
    assert_eq!(
        provenance.container_script_sha256,
        sha256_hex(CONTAINER_SCRIPT.as_bytes())
    );
    assert_eq!(provenance.prompt_version, PROMPT_VERSION);
    // A different binary must yield a different digest — the whole
    // point of the mixed-build guard.
    std::fs::write(&bin, b"pretend elf v2").unwrap();
    let rebuilt = sweep_provenance(Some(&bin), AgentKind::Loom).unwrap();
    assert_ne!(rebuilt.loom_sha256, provenance.loom_sha256);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(test)]
fn sweep_with_digest(digest: Option<&str>) -> serde_json::Value {
    let mut report = serde_json::json!({"model": "m", "instances": []});
    if let Some(digest) = digest {
        report["provenance"]["loom_sha256"] = serde_json::json!(digest);
    }
    report
}

#[test]
fn span_one_build_accepts_matching_and_legacy_reports() {
    // Same build twice — fine.
    assert!(
        span_one_build(
            &sweep_with_digest(Some("aa")),
            &sweep_with_digest(Some("aa"))
        )
        .is_ok()
    );
    // Pre-provenance (legacy) reports carry no digest — allowed, the
    // caller warns.
    assert!(span_one_build(&sweep_with_digest(Some("aa")), &sweep_with_digest(None)).is_ok());
    assert!(span_one_build(&sweep_with_digest(None), &sweep_with_digest(None)).is_ok());
}

#[test]
fn span_one_build_refuses_mixed_builds() {
    let error = span_one_build(
        &sweep_with_digest(Some("aa")),
        &sweep_with_digest(Some("bb")),
    )
    .unwrap_err();
    assert!(error.contains("two engine builds"), "{error}");
    assert!(error.contains("aa") && error.contains("bb"), "{error}");
}

#[test]
fn mini_agent_arm_selects_its_script_and_opens_the_openai_dialect() {
    // Same spec shape as the loom arm, one field flipped: the argv must
    // run MINI_CONTAINER_SCRIPT and the key must ride as OPENAI_API_KEY
    // (litellm reads nothing else).
    let spec = ContainerSpec {
        agent: AgentKind::Mini,
        image: "sweb.eval.x86_64.org__one-1:latest",
        loom_binary: None,
        prompt_file: Path::new("/eval/swebench/instances/org-one-1/prompt-run1.txt"),
        out_dir: Path::new("/eval/swebench/instances/org-one-1/run-1"),
        provider: "kilo",
        model: "nano-model",
        base_url: Some("https://free.empexaura.com/v1"),
        api_key_env: "OPENAI_API_KEY",
        api_key: "nano-test",
        timeout_secs: 3600,
        max_turns: 60,
        bare: false,
        extra_env: &[],
        container_name: "mini-org-one-1-run1",
    };
    let argv = docker_run_argv(&spec);
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], MINI_CONTAINER_SCRIPT);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nOPENAI_API_KEY=nano-test"));
    assert!(joined.contains("-e\nLOOM_BASE_URL=https://free.empexaura.com/v1"));
    // No loom build runs in this arm: no mount, nothing to shadow.
    assert!(!joined.contains("/loom/loom"));
}

#[test]
fn mini_script_keeps_the_shared_extraction_contract() {
    // The baseline arm must score the same way the loom arm does: the
    // patch is extracted even after a failure (`status=$?` then diff,
    // not `set -e` semantics), and the model is routed through
    // litellm's OpenAI provider against the forwarded base URL.
    assert!(
        MINI_CONTAINER_SCRIPT.contains(
            "status=$?; cd /testbed && git add -A && git diff --cached > /out/patch.diff"
        )
    );
    assert!(MINI_CONTAINER_SCRIPT.contains("--model \"openai/$LOOM_MODEL\""));
    assert!(MINI_CONTAINER_SCRIPT.contains("OPENAI_BASE_URL=\"$LOOM_BASE_URL\""));
    // Their shipped swebench config is the baseline being measured —
    // never a hand-rolled prompt.
    assert!(MINI_CONTAINER_SCRIPT.contains("swebench.yaml"));
}

#[test]
fn provenance_hashes_the_selected_agents_script() {
    let dir = std::env::temp_dir().join(format!("loom-prov-mini-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("loom");
    std::fs::write(&bin, b"pretend elf").unwrap();
    let loom = sweep_provenance(Some(&bin), AgentKind::Loom).expect("provenance");
    let mini = sweep_provenance(Some(&bin), AgentKind::Mini).expect("provenance");
    assert_eq!(
        loom.container_script_sha256,
        sha256_hex(CONTAINER_SCRIPT.as_bytes())
    );
    assert_eq!(
        mini.container_script_sha256,
        sha256_hex(MINI_CONTAINER_SCRIPT.as_bytes())
    );
    assert_ne!(loom.container_script_sha256, mini.container_script_sha256);
}
