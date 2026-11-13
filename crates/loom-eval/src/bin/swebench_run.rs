//! `swebench-run`: orchestrates a SWE-bench Verified sweep in container
//! isolation. One container per (instance, run); the agent is
//! mini-swe-agent from `PyPI` (default — the published baseline whose
//! score gives loom's number meaning through `swebench-contrast`) or
//! the staged linux loom binary bind-mounted into the official
//! per-instance testbed image (`--agent loom`); the patch is extracted
//! inside the container (never on the host); the official scorer
//! consumes the emitted predictions files.
//!
//! Host prerequisites: docker. Missing testbed images are pulled
//! automatically from the swebench Docker Hub prebuilts — the same
//! registry names the scorer itself looks up — and retagged to the
//! local tag. Local builds (`swebench images build`) are only the
//! offline fallback (`--no-pull`). `--build-loom` cross-builds the
//! linux binary inside a docker rust image, so the host toolchain
//! stays untouched.

// A harness binary: panicking on a setup failure is the intended
// behavior.

// Terminal output is this harness's product; the workspace-wide
// `clippy::print_stdout`/`print_stderr` denies target library crates.
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_eval::swebench::{
    self, AgentKind, ContainerSpec, EXPECTED_ROWS, RunUsage, SweInstance, api_key_envs,
    build_prompt, dataset_cache_path, docker_run_argv, fetch_dataset_verified, load_dataset_cached,
    run_outcome, run_usage, select_instances, testbed_image,
};

struct Args {
    instances: Vec<String>,
    repos: Vec<String>,
    max: usize,
    per_repo: Option<usize>,
    runs: usize,
    jobs: usize,
    provider: String,
    model: String,
    base_url: Option<String>,
    loom_bin: Option<PathBuf>,
    build_loom: bool,
    pull_images: bool,
    timeout_secs: u64,
    max_turns: usize,
    /// `full` (default) or `bare` — the roadmap-item-1 A/B arm.
    arm: String,
    /// `mini` (default) or `loom` — the agent under test
    /// (`AgentKind`).
    agent: String,
    eval_dir: PathBuf,
    /// Optional structural suite-audit artifact. Can be paired with
    /// --suite-audit-results to embed official mutant verdicts.
    suite_audit: Option<PathBuf>,
    /// Optional official mutant-result aggregate for --suite-audit.
    suite_audit_results: Option<PathBuf>,
    /// Per-turn reasoning-effort policy forwarded to the in-container
    /// loom (roadmap item 9): `fixed` (default) or `progressive`.
    /// Recorded in sweep provenance and forwarded as
    /// `LOOM_REASONING_SCHEDULE` — `fixed` included, so the report's
    /// claim and the container env cannot disagree.
    reasoning_schedule: String,
    /// Opening turns at high effort under `progressive`.
    reasoning_open_turns: u16,
}

fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

fn env_csv(name: &str) -> Vec<String> {
    std::env::var(name).ok().map_or_default(|v| csv(&v))
}

fn usage() -> ! {
    eprintln!(
        "usage: swebench-run [options]

  --instances <csv>    explicit SWE-bench instance ids (every id must resolve)
  --repos <csv>        filter to org/repo values (e.g. sympy/sympy)
  --max <n>            cap on instances in dataset order (default 5)
  --per-repo <n>       instead of --max: first n instances of each --repos repo
  --runs <n>           runs per instance (default 1)
  --jobs <n>           instances to run in parallel (default 1; container
                       names and artifact dirs are per-instance, so parallel
                       runs cannot collide)
  --no-pull            never pull missing testbed images; skip the instance
                       with the local build command printed instead
  --agent <name>       agent under test: `mini` (default) or `loom`
                       (mini = mini-swe-agent from PyPI, the published
                       baseline loom is contrasted against; runs through litellm's
                       OpenAI dialect, so the key travels as
                       OPENAI_API_KEY and --base-url is required —
                       registry providers auto-fill it)
  --provider <name>    provider for the in-container loom (default $LOOM_EVAL_PROVIDER or deepseek)
  --model <name>       model for the in-container loom (default $LOOM_EVAL_MODEL or deepseek-flash)
  --base-url <url>     chat API base URL for OpenAI-compatible gateways (custom provider);
                      forwarded as --base-url to the in-container loom
                      (default $LOOM_EVAL_BASE_URL, unset for vendors with default endpoints)
  --loom-bin <path>    linux x86_64 loom binary (--agent loom only) (default <eval-dir>/swebench/loom-linux/loom)
  --build-loom         cross-build the linux binary inside a docker rust image (--agent loom only)
  --timeout-secs <n>   wall-clock budget per run (default 3600)
  --max-turns <n>      --max-turns for the in-container loom run (default 100)
  --arm <full|bare>    harness arm (--agent loom only; default $LOOM_EVAL_ARM or full): `bare`
                       forwards LOOM_BARE=1 so the in-container loom strips
                       every harness lever (facts, pruning, completion gate,
                       failure ledger, fast paths) — the roadmap-item-1 A/B
                       baseline. The container script is identical in both
                       arms; run each arm in its own --eval-dir
  --eval-dir <path>    artifacts root (default $LOOM_EVAL_DIR or <cwd>/target/eval)
  --suite-audit <path> structural suite_audit.json: embeds weak flags in provenance
  --suite-audit-results <path>
                        optional official mutant-results.json aggregate; verdicts
                        are validated and digest-bound into provenance
  --reasoning-schedule <fixed|progressive>
                       per-turn reasoning-effort policy for the in-container
                       loom (default $LOOM_EVAL_REASONING_SCHEDULE or fixed):
                       `progressive` forwards LOOM_REASONING_SCHEDULE=progressive
                       so opening and post-verify turns run at high effort
                       (roadmap item 9); the container script stays identical
  --reasoning-open-turns <n>
                       opening turns at high effort under progressive
                       (default $LOOM_EVAL_REASONING_OPEN_TURNS or 2)

Knobs mirror the environment forms: LOOM_EVAL_SWEBENCH_INSTANCES,
LOOM_EVAL_SWEBENCH_REPOS, LOOM_EVAL_PROVIDER, LOOM_EVAL_MODEL,
LOOM_EVAL_BASE_URL, LOOM_EVAL_DIR, LOOM_EVAL_SWEBENCH_JSONL (dataset override).
The API key comes from the loom account store (`loom auth login <provider>`)
or the provider's env var (e.g. DEEPSEEK_API_KEY). Named providers
(`loom provider add`) pick up their recorded --base-url automatically."
    );
    std::process::exit(2);
}

// Returns the parsed args plus any `loom provider add --header` pairs
// recorded for the chosen provider: main() forwards them into every
// eval container as `LOOM_HEADERS` alongside the correlation headers.
#[allow(clippy::too_many_lines)] // one exhaustive flag list; each arm is a single assignment
fn parse_args() -> (Args, Vec<(String, String)>) {
    let mut args = Args {
        instances: env_csv("LOOM_EVAL_SWEBENCH_INSTANCES"),
        repos: env_csv("LOOM_EVAL_SWEBENCH_REPOS"),
        max: 5,
        per_repo: None,
        runs: 1,
        jobs: 1,
        provider: std::env::var("LOOM_EVAL_PROVIDER").unwrap_or_else(|_| "deepseek".into()),
        model: std::env::var("LOOM_EVAL_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        base_url: std::env::var("LOOM_EVAL_BASE_URL").ok(),
        loom_bin: None,
        build_loom: false,
        pull_images: true,
        timeout_secs: 3600,
        // v2 (2026-10-21): 60 starved one astropy run into a pure
        // exploration loop with no edit; 100 gives iterating room while
        // --timeout-secs still bounds wall clock.
        max_turns: 100,
        arm: std::env::var("LOOM_EVAL_ARM").unwrap_or_else(|_| "full".into()),
        agent: std::env::var("LOOM_EVAL_AGENT").unwrap_or_else(|_| "mini".into()),
        eval_dir: std::env::var("LOOM_EVAL_DIR")
            .map_or_else(|_| PathBuf::from("target/eval"), PathBuf::from),
        suite_audit: None,
        suite_audit_results: None,
        reasoning_schedule: std::env::var("LOOM_EVAL_REASONING_SCHEDULE")
            .unwrap_or_else(|_| "fixed".into()),
        reasoning_open_turns: std::env::var("LOOM_EVAL_REASONING_OPEN_TURNS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--instances" => args.instances = csv(&value()),
            "--repos" => args.repos = csv(&value()),
            "--max" => args.max = value().parse().unwrap_or_else(|_| usage()),
            "--per-repo" => args.per_repo = Some(value().parse().unwrap_or_else(|_| usage())),
            "--runs" => args.runs = value().parse().unwrap_or_else(|_| usage()),
            "--jobs" => args.jobs = value().parse().unwrap_or_else(|_| usage()),
            "--provider" => args.provider = value(),
            "--model" => args.model = value(),
            "--base-url" => args.base_url = Some(value()),
            "--loom-bin" => args.loom_bin = Some(PathBuf::from(value())),
            "--build-loom" => args.build_loom = true,
            "--no-pull" => args.pull_images = false,
            "--timeout-secs" => args.timeout_secs = value().parse().unwrap_or_else(|_| usage()),
            "--max-turns" => args.max_turns = value().parse().unwrap_or_else(|_| usage()),
            "--arm" => args.arm = value(),
            "--agent" => args.agent = value(),
            "--eval-dir" => args.eval_dir = PathBuf::from(value()),
            "--suite-audit" => args.suite_audit = Some(PathBuf::from(value())),
            "--suite-audit-results" => args.suite_audit_results = Some(PathBuf::from(value())),
            "--reasoning-schedule" => args.reasoning_schedule = value(),
            "--reasoning-open-turns" => {
                args.reasoning_open_turns = value().parse().unwrap_or_else(|_| usage());
            }
            _ => usage(), // covers `--help`/`-h` too
        }
    }
    if args.runs == 0 || args.max == 0 {
        eprintln!("--runs and --max must be positive");
        std::process::exit(2);
    }
    if args.jobs == 0 {
        eprintln!("--jobs must be positive");
        std::process::exit(2);
    }
    if args.per_repo == Some(0) {
        eprintln!("--per-repo must be positive");
        std::process::exit(2);
    }
    if args.arm != "full" && args.arm != "bare" {
        eprintln!("--arm must be `full` or `bare`");
        std::process::exit(2);
    }
    if args.agent != "loom" && args.agent != "mini" {
        eprintln!("--agent must be `loom` or `mini`");
        std::process::exit(2);
    }
    if args.reasoning_schedule != "fixed" && args.reasoning_schedule != "progressive" {
        eprintln!("--reasoning-schedule must be `fixed` or `progressive`");
        std::process::exit(2);
    }
    if args.reasoning_open_turns == 0 {
        eprintln!("--reasoning-open-turns must be positive");
        std::process::exit(2);
    }
    // A named provider (`loom provider add`) already recorded its
    // endpoint; fill --base-url from the registry so callers needn't
    // restate it (the eval containers run registry-less). Registry
    // headers ride along even when --base-url is explicit: they are
    // identity for the gateway's own logs, not endpoint metadata.
    let mut registry_headers: Vec<(String, String)> = Vec::new();
    if let Some(entry) =
        loom_core::engine::registry::ProviderRegistry::load(&loom_core::paths::db_path())
            .get(&args.provider)
    {
        if args.base_url.is_none() {
            args.base_url = Some(entry.base_url.clone());
        }
        registry_headers = entry
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
    }
    // mini-swe-agent speaks litellm's OpenAI dialect: it needs an
    // explicit --base-url (registry providers auto-fill it above, so
    // this only fires for vendors with baked-in endpoints).
    if args.agent == "mini" && args.base_url.is_none() {
        eprintln!(
            "--agent mini routes traffic through an OpenAI-compatible endpoint: \
             pass --base-url (or use a `loom provider add`-ed provider)"
        );
        std::process::exit(2);
    }
    (args, registry_headers)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_api_key(provider: &str) -> (String, String) {
    // The engine's single resolution order (ADR-009): the active
    // `loom auth login <provider>` account, then the provider's env
    // vars. Either way the key is carried into the container under the
    // provider's first env-var name.
    let Ok(credential) = loom_core::engine::auth::Auth::open(&loom_core::paths::db_path())
        .and_then(|auth| auth.resolve(provider, None))
    else {
        eprintln!(
            "no API key for provider `{provider}`: run `loom auth login {provider}` \
             or export one of {} (see swebench-run --help)",
            api_key_envs(provider).join(" / ")
        );
        std::process::exit(2);
    };
    (
        api_key_envs(provider).into_iter().next().unwrap(),
        credential.secret.expose().to_owned(),
    )
}

/// Cross-builds the linux loom binary inside a docker rust image and
/// copies it next to the sweep artifacts. No host toolchain changes.
///
/// Builds the static musl target: testbed images ship older glibc than
/// the rust image (Ubuntu 22.04 glibc 2.35 vs Debian trixie 2.41), so
/// a gnu-linked binary dies on `GLIBC_2.38 not found`. The static
/// binary runs on every testbed.
fn build_loom_linux(base: &Path) -> PathBuf {
    let root = repo_root();
    println!("building linux loom inside docker (this compiles the workspace)…");
    let status = Command::new("docker")
        .args([
            "run",
            "--platform",
            "linux/amd64",
            "--rm",
            "-v",
            &format!("{}:/src", root.display()),
            "-w",
            "/src",
            "rust:1",
            "sh",
            "-c",
            "rustup target add x86_64-unknown-linux-musl && \
             apt-get update -qq && apt-get install -y -qq musl-tools > /dev/null && \
             cargo build --release -p loom-cli --target x86_64-unknown-linux-musl",
        ])
        .status()
        .expect("spawning docker");
    assert!(status.success(), "linux loom build failed");
    let built = root.join("target/x86_64-unknown-linux-musl/release/loom");
    let dest_dir = base.join("loom-linux");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("loom");
    std::fs::copy(&built, &dest).unwrap();
    println!("linux loom binary: {}", dest.display());
    dest
}

fn image_exists(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Makes sure the local testbed tag exists. Missing images are pulled
/// from the swebench Docker Hub prebuilts under the scorer's own
/// lookup name (`swebench/sweb.eval.x86_64.<id, __→_1776_>`) and
/// retagged to the local name — pulled images are scoring-ready as a
/// side effect, no separate alias step. Pulls serialize on the lock so
/// parallel jobs share bandwidth instead of hammering the registry;
/// a failed pull skips the instance (with the manual build command)
/// rather than aborting the sweep.
fn ensure_image(
    instance_id: &str,
    image: &str,
    pull: bool,
    pull_lock: &std::sync::Mutex<()>,
) -> bool {
    if image_exists(image) {
        return true;
    }
    if !pull {
        eprintln!(
            "SKIP {instance_id} — image missing; build it with: swebench images build SWE-bench/swe-bench-tasks --dataset verified -i {instance_id}"
        );
        return false;
    }
    let remote = swebench::scored_image(instance_id);
    let _guard = pull_lock.lock().unwrap();
    // A parallel worker may have pulled it while we waited.
    if image_exists(image) {
        return true;
    }
    println!("pulling {remote} (prebuilt testbed)…");
    // The prebuilts are x86_64-only; without an explicit platform a
    // docker pull on arm64 hosts (Apple Silicon) requests arm64 and
    // fails with "no matching manifest".
    let status = Command::new("docker")
        .args(["pull", "--platform", "linux/amd64", &remote])
        .status()
        .unwrap_or_else(|error| panic!("spawning docker pull for {remote}: {error}"));
    if !status.success() {
        eprintln!(
            "SKIP {instance_id} — pull {remote} failed; build locally with: swebench images build SWE-bench/swe-bench-tasks --dataset verified -i {instance_id}"
        );
        return false;
    }
    let status = Command::new("docker")
        .args(["tag", &remote, image])
        .status()
        .unwrap_or_else(|error| panic!("spawning docker tag for {remote}: {error}"));
    assert!(status.success(), "tagging {remote} as {image} failed");
    true
}

/// One instance's contribution to the sweep: the report row, per-run
/// owned prediction rows (`instance_id`, `model`, `patch`), and the
/// instance's rolled-up token usage for the sweep-level cache summary.
type InstanceOutcome = Option<(
    serde_json::Value,
    Vec<Vec<(String, String, String)>>,
    RunUsage,
)>;

// Free-tier gateways rate-limit under concurrent sweeps; a 429-killed
// run is infrastructure, not model competence, so it is retried after
// a cooldown before it can count. Free capacity comes in windows
// (shared pools), so retry persistently: 10 x 3min ≈ 30min per run.
const MAX_ATTEMPTS: usize = 10;
const RATE_LIMIT_COOLDOWN_SECS: u64 = 180;

#[allow(clippy::too_many_lines)] // one linear sweep: validate → dataset → runs → score → report
fn main() {
    let (args, registry_headers) = parse_args();
    if args.suite_audit_results.is_some() && args.suite_audit.is_none() {
        eprintln!("--suite-audit-results requires --suite-audit");
        std::process::exit(2);
    }
    let base = args.eval_dir.join("swebench");
    std::fs::create_dir_all(base.join("instances")).unwrap();

    // Dataset: explicit file, cache, or live fetch (then cached).
    let dataset = if let Some(dataset) = load_dataset_cached(&args.eval_dir) {
        println!("dataset: {} instances (cached)", dataset.len());
        dataset
    } else {
        println!(
            "fetching {} from the datasets-server…",
            swebench::DATASET_ID
        );
        let body = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(fetch_dataset_verified())
            .unwrap_or_else(|error| panic!("dataset fetch: {error}"));
        let dataset = swebench::parse_dataset(&body);
        assert_eq!(dataset.len(), EXPECTED_ROWS);
        let cache = dataset_cache_path(&args.eval_dir);
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, &body).unwrap();
        println!(
            "dataset: {} instances (fetched, cached at {})",
            dataset.len(),
            cache.display()
        );
        dataset
    };

    let selected = select_instances(
        &dataset,
        &args.instances,
        &args.repos,
        args.max,
        args.per_repo,
    );
    println!(
        "sweep: {} instance(s) x {} run(s), provider={}, model={}, arm={}",
        selected.len(),
        args.runs,
        args.provider,
        args.model,
        args.arm
    );
    if selected.is_empty() {
        eprintln!("no instances selected (filters too tight?)");
        std::process::exit(2);
    }

    // Validate optional suite-audit inputs before pulling images or spending
    // on model runs. Their provenance is embedded in the final sweep report.
    let suite_audit_block = args.suite_audit.as_ref().map(|path| {
        let bytes =
            std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let sweep_ids: Vec<String> = selected
            .iter()
            .map(|instance| instance.instance_id.clone())
            .collect();
        let results_bytes = args.suite_audit_results.as_ref().map(|results_path| {
            std::fs::read(results_path)
                .unwrap_or_else(|error| panic!("{}: {error}", results_path.display()))
        });
        let weak = loom_eval::suite_audit::sweep_weak_suite_with_results(
            &bytes,
            results_bytes.as_deref(),
            &sweep_ids,
        )
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let dataset_digest = std::fs::read(dataset_cache_path(&args.eval_dir))
            .map_or_default(|body| swebench::sha256_hex(&body));
        if dataset_digest != weak.dataset_sha256 {
            eprintln!(
                "refusing to run: {path} audited dataset {digest}, but this\n\
                 sweep runs {actual} — the weak flags would describe another suite.",
                path = path.display(),
                digest = weak.dataset_sha256,
                actual = if dataset_digest.is_empty() {
                    "an uncached (overridden?) dataset".to_owned()
                } else {
                    dataset_digest
                }
            );
            std::process::exit(2);
        }
        if weak.weak_ids.is_empty() && weak.weak_total > 0 {
            eprintln!(
                "warning: the audit flags {total} weak instances, none of them in\n\
                 this sweep's {n} instance(s) — is this the right audit artifact?",
                total = weak.weak_total,
                n = sweep_ids.len()
            );
        }
        weak
    });

    let (key_env, key_value) = resolve_api_key(&args.provider);
    // mini-swe-agent authenticates through litellm, which only reads
    // OPENAI_API_KEY; carry the same secret under that name.
    let key_env = if args.agent == "mini" {
        "OPENAI_API_KEY".to_owned()
    } else {
        key_env
    };
    let agent = AgentKind::parse(&args.agent).expect("validated in parse_args");
    // Only the loom arm runs a build; mini needs no staged binary,
    // mount, or digest — its provenance records the script hash alone.
    let loom_bin = if agent == AgentKind::Loom {
        let loom_bin = match (&args.loom_bin, args.build_loom) {
            (Some(path), _) => path.clone(),
            (None, true) => build_loom_linux(&base),
            (None, false) => {
                let default = base.join("loom-linux/loom");
                if !default.exists() {
                    eprintln!(
                        "no linux loom binary at {} — pass --loom-bin or --build-loom",
                        default.display()
                    );
                    std::process::exit(2);
                }
                default
            }
        };
        // Docker reads `-v src:dst` with a RELATIVE src as a named
        // volume, not a bind mount — the mounted path must be absolute.
        Some(
            loom_bin
                .canonicalize()
                .unwrap_or_else(|error| panic!("{}: {error}", loom_bin.display())),
        )
    } else {
        None
    };

    let file_model = swebench::slug(&args.model);

    // Honesty rule: one headline, one engine build. Hash the staged
    // binary (the host cannot execute the linux ELF) and refuse to
    // extend an eval dir whose recorded sweeps ran a different build —
    // silently mixing builds into one number is how the v3-100
    // headline spanned two binaries.
    let provenance =
        swebench::sweep_provenance(loom_bin.as_deref(), agent, &args.reasoning_schedule)
            .unwrap_or_else(|error| panic!("hashing the staged loom binary: {error}"));
    for entry in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("swebench-sweep-"))
        {
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(recorded) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            // A mini run records no digest; so does a pre-provenance
            // report. Both extend freely — only two DIFFERENT loom
            // builds in one eval dir are refused.
            if let Some(digest) = provenance.loom_sha256.as_deref()
                && let Some(recorded) = swebench::recorded_loom_digest(&recorded)
                && recorded != digest
            {
                eprintln!(
                    "refusing to run: {path} records engine build {recorded}, but the staged\n\
                     binary is {digest} — a headline sweep must run one build.\n\
                     Start a fresh --eval-dir (or remove the old sweeps) instead of\n\
                     extending a mixed-build directory.",
                    path = path.display()
                );
                std::process::exit(2);
            }
        }
    }
    let loom_digest_path = base.join("loom.sha256");
    if let Some(loom_sha256) = provenance.loom_sha256.as_deref() {
        std::fs::write(&loom_digest_path, format!("{loom_sha256}\n"))
            .unwrap_or_else(|error| panic!("{}: {error}", loom_digest_path.display()));
    }
    // Owned rows (instance_id, model, patch): patches must outlive the
    // per-run scope they were collected in; borrowed `Prediction`s are
    // built from these at write time.
    let mut predictions: Vec<Vec<(String, String, String)>> = vec![Vec::new(); args.runs];
    let mut report: Vec<serde_json::Value> = Vec::new();
    let mut prediction_paths: Vec<PathBuf> = Vec::new();

    // Pulls serialize across workers; runs stay as parallel as --jobs.
    let pull_lock = std::sync::Mutex::new(());
    let run_instance = |instance: &SweInstance| -> InstanceOutcome {
        let slug = swebench::slug(&instance.instance_id);
        let image = testbed_image(&instance.instance_id);
        let instance_dir = base.join("instances").join(&slug);
        std::fs::create_dir_all(&instance_dir).unwrap();

        if !ensure_image(&instance.instance_id, &image, args.pull_images, &pull_lock) {
            return None;
        }

        let prompt_path = instance_dir.join("prompt.txt");
        std::fs::write(&prompt_path, build_prompt(instance)).unwrap();
        let prompt_path = prompt_path
            .canonicalize()
            .unwrap_or_else(|error| panic!("{}: {error}", prompt_path.display()));

        let mut runs = Vec::new();
        let mut per_run: Vec<Vec<(String, String, String)>> = vec![Vec::new(); args.runs];
        let mut usage_total = RunUsage::default();
        for run in 1..=args.runs {
            let out_dir = instance_dir.join(format!("run-{run}"));
            std::fs::create_dir_all(&out_dir).unwrap();
            let out_dir = out_dir
                .canonicalize()
                .unwrap_or_else(|error| panic!("{}: {error}", out_dir.display()));
            let container_name = format!("loom-{slug}-run{run}");
            // Free-tier gateways rate-limit under concurrent sweeps; retry
            // policy is the module-level MAX_ATTEMPTS/RATE_LIMIT_COOLDOWN_SECS.
            let mut exit_code = -1;
            let mut patch = String::new();
            let mut outcome = (0usize, None);
            for attempt in 1..=MAX_ATTEMPTS {
                // Correlation headers: gateway-side logs become
                // greppable per instance, run, and attempt — the
                // debugging role retry counters play for hosted SDK
                // clients. `LOOM_HEADERS` is re-parsed by the
                // in-container CLI's `--header`, so the container
                // script stays byte-identical.
                let mut header_pairs = registry_headers.clone();
                header_pairs.push(("X-Loom-Run-Id".to_owned(), instance.instance_id.clone()));
                header_pairs.push(("X-Loom-Attempt".to_owned(), attempt.to_string()));
                let mut extra_env = vec![(
                    "LOOM_HEADERS".to_owned(),
                    swebench::headers_env_value(&header_pairs),
                )];
                // Roadmap item 9: the effort policy rides env, not the
                // frozen script. `fixed` is forwarded too, so the
                // sweep report's provenance and the container's env
                // cannot disagree.
                extra_env.push((
                    "LOOM_REASONING_SCHEDULE".to_owned(),
                    args.reasoning_schedule.clone(),
                ));
                if args.reasoning_schedule == "progressive" {
                    extra_env.push((
                        "LOOM_REASONING_OPEN_TURNS".to_owned(),
                        args.reasoning_open_turns.to_string(),
                    ));
                }
                let spec = ContainerSpec {
                    agent,
                    script_override: None,
                    image: &image,
                    loom_binary: loom_bin.as_deref(),
                    prompt_file: &prompt_path,
                    out_dir: &out_dir,
                    provider: &args.provider,
                    model: &args.model,
                    base_url: args.base_url.as_deref(),
                    api_key_env: &key_env,
                    api_key: &key_value,
                    timeout_secs: args.timeout_secs,
                    max_turns: args.max_turns,
                    bare: args.arm == "bare",
                    extra_env: &extra_env,
                    docker_args: &[],
                    container_name: &container_name,
                };
                println!("run {slug} #{run} (attempt {attempt}) in {image}…");
                let status = Command::new("docker")
                    .args(docker_run_argv(&spec))
                    .status()
                    .unwrap_or_else(|error| {
                        panic!("spawning docker for {container_name}: {error}")
                    });
                exit_code = status.code().unwrap_or(-1);
                outcome = run_outcome(&out_dir.join("run.json"));
                patch = std::fs::read_to_string(out_dir.join("patch.diff")).unwrap_or_default();
                let rate_limited = outcome
                    .1
                    .as_deref()
                    .is_some_and(|reason| reason.contains("rate_limited"));
                if !rate_limited || attempt == MAX_ATTEMPTS {
                    if rate_limited {
                        eprintln!(
                            "run {slug} #{run}: still rate-limited after {MAX_ATTEMPTS} attempts, keeping the result"
                        );
                    }
                    break;
                }
                eprintln!(
                    "run {slug} #{run}: rate-limited, retrying in {RATE_LIMIT_COOLDOWN_SECS}s"
                );
                std::thread::sleep(std::time::Duration::from_secs(RATE_LIMIT_COOLDOWN_SECS));
            }
            let (events, task_failed) = outcome;
            let usage = run_usage(&out_dir.join("run.json"));
            usage_total.merge(usage);
            // F-05 enforcement: a trajectory that fetched the fix (or
            // installed packages mid-run) is flagged in the row; the
            // sweep summary derives its total from these rows and
            // refuses a clean-sweep read.
            let flags = swebench::integrity_flags(&out_dir.join("run.json"));
            if !flags.is_empty() {
                eprintln!(
                    "run {slug} #{run}: INTEGRITY FLAGS (not a clean solve): {}",
                    flags.join("; ")
                );
            }
            let row = serde_json::json!({
                "run": run,
                "container": container_name,
                "exit_code": exit_code,
                "patch_bytes": patch.len(),
                "patch_empty": patch.trim().is_empty(),
                "events": events,
                "task_failed": task_failed,
                "integrity_flags": flags,
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "cached_input_tokens": usage.cached_input_tokens,
                    "output_tokens": usage.output_tokens,
                    "model_requests": usage.model_requests,
                    "hit_rate": usage.hit_rate(),
                },
            });
            println!(
                "run {slug} #{run}: exit {exit_code}, patch {} bytes",
                patch.len()
            );
            runs.push(row);
            per_run[run - 1].push((instance.instance_id.clone(), args.model.clone(), patch));
        }

        Some((
            serde_json::json!({
                "instance_id": instance.instance_id,
                "repo": instance.repo,
                "base_commit": instance.base_commit,
                "image": image,
                "runs": runs,
            }),
            per_run,
            usage_total,
        ))
    };

    // Workers pull instances off a shared index. Artifacts live in
    // per-instance directories and container names carry the instance
    // slug, so parallel runs cannot collide; results are re-assembled
    // in selection order below regardless of completion order.
    let outcomes: Vec<InstanceOutcome> = (0..selected.len()).map(|_| None).collect();
    let results = std::sync::Mutex::new(outcomes);
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..args.jobs.min(selected.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(instance) = selected.get(index) else {
                        break;
                    };
                    let outcome = run_instance(instance);
                    results.lock().unwrap()[index] = outcome;
                }
            });
        }
    });
    let outcomes = results.into_inner().unwrap();
    let mut sweep_usage = RunUsage::default();
    for (row, per_run, instance_usage) in outcomes.into_iter().flatten() {
        report.push(row);
        sweep_usage.merge(instance_usage);
        for (run_index, rows) in per_run.into_iter().enumerate() {
            predictions[run_index].extend(rows);
        }
    }

    prediction_paths.extend(swebench::write_prediction_sets(
        &base,
        &format!("predictions-{file_model}"),
        &predictions,
        args.runs,
    ));
    if args.runs > 1 && !predictions.is_empty() {
        println!(
            "best-of-{} selection: {}",
            args.runs,
            prediction_paths.last().unwrap().display()
        );
    }

    let mut provenance_json = serde_json::json!({
        "loom_sha256": provenance.loom_sha256,
        "loom_bytes": provenance.loom_bytes,
        "container_script_sha256": provenance.container_script_sha256,
        "prompt_version": provenance.prompt_version,
        "runner_version": provenance.runner_version,
        "reasoning_schedule": provenance.reasoning_schedule,
    });
    if let Some(weak) = &suite_audit_block {
        provenance_json["suite_audit"] =
            serde_json::to_value(weak).expect("weak-suite block serializes");
    }

    let sweep = serde_json::json!({
        "provider": args.provider,
        "model": args.model,
        "runs": args.runs,
        "selection": if args.runs > 1 { "majority-normalized-diff" } else { "single-run" },
        "base_url": args.base_url,
        "arm": args.arm,
        "dataset": swebench::DATASET_ID,
        // F-05: zero means every trajectory stayed inside the
        // testbed; anything else names runs whose score must be read
        // as assisted, not clean (details in each run row's
        // integrity_flags).
        "integrity_flagged_runs": report
            .iter()
            .filter_map(|row| row["runs"].as_array())
            .map(|runs| runs
                .iter()
                .filter(|run| run["integrity_flags"]
                    .as_array()
                    .is_some_and(|flags| !flags.is_empty()))
                .count())
            .sum::<usize>(),
        "provenance": provenance_json,
        "instances": report,
        "predictions_files": prediction_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
    });
    let sweep_path = base.join(format!("swebench-sweep-{file_model}.json"));
    std::fs::write(&sweep_path, serde_json::to_string_pretty(&sweep).unwrap()).unwrap();
    println!("sweep report: {}", sweep_path.display());

    // Cache observability: the whole point of the frozen prompt and
    // append-only projection is provider prefix-cache hits; report the
    // measured rate so regressions are visible per sweep.
    if sweep_usage.model_requests > 0 {
        match sweep_usage.hit_rate() {
            Some(rate) => eprintln!(
                "cache: {}/{} prompt tokens served from cache ({:.1}%) across {} model requests",
                sweep_usage.cached_input_tokens.unwrap_or_default(),
                sweep_usage.input_tokens,
                rate * 100.0,
                sweep_usage.model_requests
            ),
            None => eprintln!(
                "cache: provider reported no cache split ({} prompt tokens across {} model requests)",
                sweep_usage.input_tokens, sweep_usage.model_requests
            ),
        }
    }

    if prediction_paths.is_empty() {
        eprintln!("no runs completed — nothing to score");
        std::process::exit(1);
    }
    let flagged: usize =
        usize::try_from(sweep["integrity_flagged_runs"].as_u64().unwrap_or(0)).unwrap_or(0);
    if flagged > 0 {
        eprintln!(
            "\nINTEGRITY: {flagged} run(s) reached outside the testbed (git fetch/clone/pull,\n\
             github.com, or pip install/download — see each row's integrity_flags).\n\
             Their patches still score, but the sweep headline must not count them as\n\
             clean solves; the sweep report records integrity_flagged_runs = {flagged}."
        );
    }
    eprintln!("\nnext: score each predictions file with the official harness —");
    eprintln!("if any images were built locally (not pulled), alias them to");
    eprintln!("the scorer's names first (idempotent for pulled images):");
    for instance in &selected {
        eprintln!(
            "  docker tag {} {}",
            swebench::testbed_image(&instance.instance_id),
            swebench::scored_image(&instance.instance_id)
        );
    }
    for path in &prediction_paths {
        let run = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit('-').next())
            .unwrap_or("1");
        eprintln!(
            "  swebench eval verified -p {} --run-id loom-{file_model}-{run} -j 4",
            path.display()
        );
    }
}
