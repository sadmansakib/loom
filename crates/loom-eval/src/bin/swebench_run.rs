//! `swebench-run`: orchestrates a SWE-bench Verified sweep in container
//! isolation. One container per (instance, run); the agent is a linux
//! loom binary bind-mounted into the official per-instance testbed
//! image; the patch is extracted inside the container (never on the
//! host); the official scorer consumes the emitted predictions files.
//!
//! Host prerequisites: docker, and the per-instance testbed images
//! (built on arm64 via `swebench images build SWE-bench/swe-bench-tasks --dataset verified`).
//! `--build-loom` cross-builds the linux binary inside a docker rust
//! image, so the host toolchain stays untouched.

// A harness binary: panicking on a setup failure is the intended
// behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use loom_eval::swebench::{
    self, ContainerSpec, EXPECTED_ROWS, Prediction, SweInstance, api_key_envs, build_prompt,
    dataset_cache_path, docker_run_argv, fetch_dataset_verified, load_dataset_cached, run_outcome,
    select_instances, testbed_image,
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
    timeout_secs: u64,
    max_turns: usize,
    eval_dir: PathBuf,
}

fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

fn env_csv(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| csv(&v))
        .unwrap_or_default()
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
  --provider <name>    provider for the in-container loom (default $LOOM_EVAL_PROVIDER or deepseek)
  --model <name>       model for the in-container loom (default $LOOM_EVAL_MODEL or deepseek-flash)
  --base-url <url>     chat API base URL for OpenAI-compatible gateways (custom provider);
                      forwarded as --base-url to the in-container loom
                      (default $LOOM_EVAL_BASE_URL, unset for vendors with default endpoints)
  --loom-bin <path>    linux x86_64 loom binary (default <eval-dir>/swebench/loom-linux/loom)
  --build-loom         cross-build the linux binary inside a docker rust image
  --timeout-secs <n>   wall-clock budget per run (default 3600)
  --max-turns <n>      --max-turns for the in-container loom run (default 100)
  --eval-dir <path>    artifacts root (default $LOOM_EVAL_DIR or <cwd>/target/eval)

Knobs mirror the environment forms: LOOM_EVAL_SWEBENCH_INSTANCES,
LOOM_EVAL_SWEBENCH_REPOS, LOOM_EVAL_PROVIDER, LOOM_EVAL_MODEL,
LOOM_EVAL_BASE_URL, LOOM_EVAL_DIR, LOOM_EVAL_SWEBENCH_JSONL (dataset override).
The API key is read from the provider's env var (e.g. DEEPSEEK_API_KEY)."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
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
        timeout_secs: 3600,
        // v2 (2026-10-21): 60 starved one astropy run into a pure
        // exploration loop with no edit; 100 gives iterating room while
        // --timeout-secs still bounds wall clock.
        max_turns: 100,
        eval_dir: std::env::var("LOOM_EVAL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target/eval")),
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().unwrap_or_else(|| usage());
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
            "--timeout-secs" => args.timeout_secs = value().parse().unwrap_or_else(|_| usage()),
            "--max-turns" => args.max_turns = value().parse().unwrap_or_else(|_| usage()),
            "--eval-dir" => args.eval_dir = PathBuf::from(value()),
            "--help" | "-h" => usage(),
            _ => usage(),
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
    args
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_api_key(provider: &str) -> (String, String) {
    for name in api_key_envs(provider) {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return (name.to_string(), value);
        }
    }
    eprintln!(
        "no API key for provider `{provider}`: export one of {} (see swebench-run --help)",
        api_key_envs(provider).join(" / ")
    );
    std::process::exit(2);
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
    let built = root
        .join("target/x86_64-unknown-linux-musl/release/loom")
        .to_owned();
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
        .map(|status| status.success())
        .unwrap_or(false)
}

fn main() {
    let args = parse_args();
    let base = args.eval_dir.join("swebench");
    std::fs::create_dir_all(base.join("instances")).unwrap();

    // Dataset: explicit file, cache, or live fetch (then cached).
    let dataset = match load_dataset_cached(&args.eval_dir) {
        Some(dataset) => {
            println!("dataset: {} instances (cached)", dataset.len());
            dataset
        }
        None => {
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
        }
    };

    let selected = select_instances(
        &dataset,
        &args.instances,
        &args.repos,
        args.max,
        args.per_repo,
    );
    println!(
        "sweep: {} instance(s) x {} run(s), provider={}, model={}",
        selected.len(),
        args.runs,
        args.provider,
        args.model
    );
    if selected.is_empty() {
        eprintln!("no instances selected (filters too tight?)");
        std::process::exit(2);
    }

    let (key_env, key_value) = resolve_api_key(&args.provider);
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

    // Docker reads `-v src:dst` with a RELATIVE src as a named volume,
    // not a bind mount — every mounted path must be absolute.
    let loom_bin = loom_bin
        .canonicalize()
        .unwrap_or_else(|error| panic!("{}: {error}", loom_bin.display()));
    let file_model = swebench::slug(&args.model);
    // Owned rows (instance_id, model, patch): patches must outlive the
    // per-run scope they were collected in; borrowed `Prediction`s are
    // built from these at write time.
    let mut predictions: Vec<Vec<(String, String, String)>> = vec![Vec::new(); args.runs];
    let mut report: Vec<serde_json::Value> = Vec::new();
    let mut prediction_paths: Vec<PathBuf> = Vec::new();

    // One instance's contribution: the report row plus per-run owned
    // prediction rows (instance_id, model, patch).
    type InstanceOutcome = Option<(serde_json::Value, Vec<Vec<(String, String, String)>>)>;
    let run_instance = |instance: &SweInstance| -> InstanceOutcome {
        let slug = swebench::slug(&instance.instance_id);
        let image = testbed_image(&instance.instance_id);
        let instance_dir = base.join("instances").join(&slug);
        std::fs::create_dir_all(&instance_dir).unwrap();

        if !image_exists(&image) {
            eprintln!(
                "SKIP {} — image missing; build it with: swebench images build SWE-bench/swe-bench-tasks --dataset verified -i {}",
                instance.instance_id, instance.instance_id
            );
            return None;
        }

        let prompt_path = instance_dir.join("prompt.txt");
        std::fs::write(&prompt_path, build_prompt(instance)).unwrap();
        let prompt_path = prompt_path
            .canonicalize()
            .unwrap_or_else(|error| panic!("{}: {error}", prompt_path.display()));

        let mut runs = Vec::new();
        let mut per_run: Vec<Vec<(String, String, String)>> = vec![Vec::new(); args.runs];
        for run in 1..=args.runs {
            let out_dir = instance_dir.join(format!("run-{run}"));
            std::fs::create_dir_all(&out_dir).unwrap();
            let out_dir = out_dir
                .canonicalize()
                .unwrap_or_else(|error| panic!("{}: {error}", out_dir.display()));
            let container_name = format!("loom-{slug}-run{run}");
            // Free-tier gateways rate-limit under concurrent sweeps; a
            // 429-killed run is infrastructure, not model competence,
            // so it is retried after a cooldown before it can count.
            const MAX_ATTEMPTS: usize = 3;
            const RATE_LIMIT_COOLDOWN_SECS: u64 = 120;
            let mut exit_code = -1;
            let mut patch = String::new();
            let mut outcome = (0usize, None);
            for attempt in 1..=MAX_ATTEMPTS {
                let spec = ContainerSpec {
                    image: &image,
                    loom_binary: &loom_bin,
                    prompt_file: &prompt_path,
                    out_dir: &out_dir,
                    provider: &args.provider,
                    model: &args.model,
                    base_url: args.base_url.as_deref(),
                    api_key_env: &key_env,
                    api_key: &key_value,
                    timeout_secs: args.timeout_secs,
                    max_turns: args.max_turns,
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
            let row = serde_json::json!({
                "run": run,
                "container": container_name,
                "exit_code": exit_code,
                "patch_bytes": patch.len(),
                "patch_empty": patch.trim().is_empty(),
                "events": events,
                "task_failed": task_failed,
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
    for (row, per_run) in outcomes.into_iter().flatten() {
        report.push(row);
        for (run_index, rows) in per_run.into_iter().enumerate() {
            predictions[run_index].extend(rows);
        }
    }

    // Per-run predictions files (one row per instance, never duplicated).
    for (index, rows) in predictions.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let borrowed: Vec<Prediction<'_>> = rows
            .iter()
            .map(|(instance_id, model, patch)| Prediction {
                instance_id,
                model_name_or_path: model,
                model_patch: patch,
            })
            .collect();
        let path = base.join(format!("predictions-{file_model}-{}.jsonl", index + 1));
        swebench::write_predictions(&path, &borrowed);
        prediction_paths.push(path);
    }

    let sweep = serde_json::json!({
        "provider": args.provider,
        "model": args.model,
        "base_url": args.base_url,
        "dataset": swebench::DATASET_ID,
        "instances": report,
        "predictions_files": prediction_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
    });
    let sweep_path = base.join(format!("swebench-sweep-{file_model}.json"));
    std::fs::write(&sweep_path, serde_json::to_string_pretty(&sweep).unwrap()).unwrap();
    println!("sweep report: {}", sweep_path.display());

    if prediction_paths.is_empty() {
        eprintln!("no runs completed — nothing to score");
        std::process::exit(1);
    }
    eprintln!("\nnext: score each predictions file with the official harness —");
    eprintln!("one-time: alias local testbed images to the scorer's names (it");
    eprintln!("pulls from Docker Hub otherwise):");
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
