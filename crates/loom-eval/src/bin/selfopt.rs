//! `selfopt`: the offline self-optimization loop (roadmap item 20.3).
//!
//! Drives the Harness²-style propose–probe–compose cycle over the
//! artifacts a `swebench-run` sweep already left on disk:
//!
//! 1. Loads the baseline sweep (report + per-run `run.json`) and the
//!    retained lessons ledger.
//! 2. Proposes env overlays: deterministic knob halvings/doublings,
//!    optionally plus model proposals (any table-driven provider,
//!    reading the reduced traces and lessons — never raw transcripts).
//! 3. Probes each proposal by re-running the same instances in the
//!    same images with one overlay per probe — the in-container engine
//!    consumes the `LOOM_*` seams, so the frozen container script and
//!    the baseline binary are reused verbatim.
//! 4. Adopts only on paired evidence (median input tokens down ≥ 5%,
//!    events not regressed, no new failures), merges the winners, and
//!    re-probes the merged overlay before it counts.
//! 5. Appends everything to the ledger (lessons only where a contrast
//!    supports them) and writes a markdown report with the winning
//!    `LOOM_*` export lines.
//!
//! Host prerequisites: docker, the sweep's testbed images already
//! pulled (the sweep did that), and the linux loom binary the sweep
//! used. `--dry-run` stops after proposals and prints the plan.

// A harness binary: panicking on a setup failure is the intended
// behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use loom_core::kernel::model::{Message, ModelEvent, ModelRequest};
use loom_eval::selfopt::{
    self as opt, ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT, Decision, LedgerEntry, Lesson, Overlay,
    Proposal, ProposalClass, RunStat,
};
use loom_eval::swebench::{
    self, AgentKind, ContainerSpec, api_key_envs, docker_run_argv, testbed_image,
};

struct Args {
    eval_dir: PathBuf,
    sweep: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    runs: usize,
    jobs: usize,
    max_proposals: usize,
    instances: usize,
    rules_only: bool,
    dry_run: bool,
    pre_probe: Vec<PathBuf>,
    pre_probe_window: u32,
    pre_probe_reserved: u32,
    loom_bin: Option<PathBuf>,
    timeout_secs: u64,
    max_turns: usize,
    ledger: Option<PathBuf>,
    tier: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: selfopt [options]

  --eval-dir <path>     artifacts root; the sweep report is looked up under
                        <eval-dir>/swebench (default $LOOM_EVAL_DIR or <cwd>/target/eval)
  --sweep <path>        sweep report to optimize against (default
                        <eval-dir>/swebench/swebench-sweep-<model>.json)
  --provider <name>     probe/model-proposer provider (default: the sweep's)
  --model <name>        probe/model-proposer model (default: the sweep's)
  --base-url <url>      gateway base URL (default: the sweep's)
  --runs <n>            probe runs per instance (default 5 — the roadmap's
                        N>=5 medians rule)
  --jobs <n>            (instance, run) containers in parallel per probe (default 1)
  --max-proposals <n>   cap on total proposals (default 8; rules take at
                        most half, the model proposer fills the rest)
  --instances <n>       cap on baseline instances probed (default all)
  --rules-only          skip the model proposer
  --dry-run             print the proposal plan as JSON and exit (no probes)
  --pre-probe <path>    trace directory (repeatable) recorded with
                        loom run --trace; projection-affecting proposals
                        are replayed offline first and rejected when the
                        projected median input improvement is below the
                        adoption gate — docker probes are never spent on
                        them. Default off.
  --pre-probe-window <n>
                        context window assumed by the replay pre-probe
                        (default 200000; set it to the sweep model's)
  --pre-probe-reserved <n>
                        output reservation assumed by the pre-probe
                        (default 32768)
  --loom-bin <path>     linux loom binary (default <eval-dir>/swebench/loom-linux/loom)
  --timeout-secs <n>    wall-clock budget per probe run (default 3600)
  --max-turns <n>       --max-turns for the in-container loom run (default 100)
  --ledger <path>       lessons ledger (default <eval-dir>/swebench/selfopt-ledger.json)
  --tier <name>         which verified overlay to export (default heavy;
                        lite = the single best adopted overlay, heavy = the
                        re-probed merge)

The API key comes from the provider's env var (e.g. DEEPSEEK_API_KEY)."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        eval_dir: std::env::var("LOOM_EVAL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target/eval")),
        sweep: None,
        provider: None,
        model: None,
        base_url: None,
        runs: 5,
        jobs: 1,
        max_proposals: 8,
        instances: usize::MAX,
        rules_only: std::env::var("LOOM_SELFOPT_RULES_ONLY").is_ok_and(|v| v == "1"),
        dry_run: false,
        pre_probe: Vec::new(),
        pre_probe_window: 200_000,
        pre_probe_reserved: 32_768,
        loom_bin: None,
        timeout_secs: 3600,
        max_turns: 100,
        ledger: None,
        tier: "heavy".to_owned(),
    };
    let mut rest = std::env::args().skip(1);
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--eval-dir" => args.eval_dir = PathBuf::from(value()),
            "--sweep" => args.sweep = Some(PathBuf::from(value())),
            "--provider" => args.provider = Some(value()),
            "--model" => args.model = Some(value()),
            "--base-url" => args.base_url = Some(value()),
            "--runs" => args.runs = value().parse().unwrap_or_else(|_| usage()),
            "--jobs" => args.jobs = value().parse().unwrap_or_else(|_| usage()),
            "--max-proposals" => {
                args.max_proposals = value().parse().unwrap_or_else(|_| usage());
            }
            "--instances" => args.instances = value().parse().unwrap_or_else(|_| usage()),
            "--rules-only" => args.rules_only = true,
            "--dry-run" => args.dry_run = true,
            "--pre-probe" => args.pre_probe.push(PathBuf::from(value())),
            "--pre-probe-window" => {
                args.pre_probe_window = value().parse().unwrap_or_else(|_| usage());
            }
            "--pre-probe-reserved" => {
                args.pre_probe_reserved = value().parse().unwrap_or_else(|_| usage());
            }
            "--loom-bin" => args.loom_bin = Some(PathBuf::from(value())),
            "--timeout-secs" => args.timeout_secs = value().parse().unwrap_or_else(|_| usage()),
            "--max-turns" => args.max_turns = value().parse().unwrap_or_else(|_| usage()),
            "--ledger" => args.ledger = Some(PathBuf::from(value())),
            "--tier" => {
                let tier = value();
                if tier != "lite" && tier != "heavy" {
                    eprintln!("--tier must be lite or heavy, got {tier}");
                    usage();
                }
                args.tier = tier;
            }
            _ => usage(),
        }
    }
    args
}

/// The provider's key, resolved exactly the way `swebench-run` does.
fn resolve_api_key(provider: &str) -> (String, String) {
    for env in &api_key_envs(provider) {
        if let Ok(value) = std::env::var(env)
            && !value.is_empty()
        {
            return (env.clone(), value);
        }
    }
    eprintln!(
        "no API key for {provider}: set one of {:?}",
        api_key_envs(provider)
    );
    std::process::exit(2);
}

/// One model-proposer call: one-shot through the same validated
/// adapter construction the engine uses.
fn model_proposals(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key: &str,
    prompt: &str,
    max: usize,
) -> Result<(Vec<Proposal>, usize), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("tokio runtime: {error}"))?;
    runtime.block_on(async {
        let handle = loom_core::engine::one_shot_provider(provider, model, base_url, api_key)
            .map_err(|error| format!("provider setup: {error}"))?;
        let request = ModelRequest {
            model: model.to_owned(),
            messages: vec![
                Message::System {
                    text: opt::PROPOSER_SYSTEM_PROMPT.to_owned(),
                },
                Message::User {
                    text: prompt.to_owned(),
                },
            ],
            tools: Vec::new(),
            max_tokens: Some(8192),
            temperature: None,
            stable_prefix_messages: None,
        };
        let mut stream = handle.provider.stream(&request);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|error| format!("model stream: {error}"))? {
                ModelEvent::TextDelta(delta) => text.push_str(&delta),
                ModelEvent::ReasoningDelta(_) => {}
                ModelEvent::Completed(response) if response.message.content.len() > text.len() => {
                    // Completed is terminal and authoritative; deltas
                    // are the fallback when a provider sends no body.
                    text = response.message.content;
                }
                _ => {}
            }
        }
        Ok(opt::parse_proposal_response(&text, max))
    })
}

/// Runs one (instance, run) probe container for a proposal overlay,
/// with the same rate-limit retry the sweep applies. Returns the
/// run's reduced stats; a run that never produced `run.json` reads
/// as a failure so a dead probe can never look like an improvement.
#[allow(clippy::too_many_arguments)]
fn probe_run(
    label: &str,
    overlay: &Overlay,
    instance: &str,
    run: u64,
    args: &Args,
    base: &Path,
    loom_bin: &Path,
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_env: &str,
    api_key: &str,
) -> RunStat {
    let slug = swebench::slug(instance);
    let prompt_path = base.join("instances").join(&slug).join("prompt.txt");
    if !prompt_path.exists() {
        return failed_stat(instance, run, "baseline prompt.txt missing");
    }
    let out_dir = base
        .join("instances")
        .join(&slug)
        .join("selfopt")
        .join(label)
        .join(format!("run-{run}"));
    std::fs::create_dir_all(&out_dir).unwrap();
    let out_dir = out_dir
        .canonicalize()
        .unwrap_or_else(|error| panic!("{}: {error}", out_dir.display()));
    // Docker names allow [a-zA-Z0-9_.-]; '+' is not in that set.
    let container_name = format!("loom-{slug}-so-{}-r{run}", label.replace('+', "-"));
    let extra: Vec<(String, String)> = overlay
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    const MAX_ATTEMPTS: usize = 3;
    const RATE_LIMIT_COOLDOWN_SECS: u64 = 120;
    for attempt in 1..=MAX_ATTEMPTS {
        let spec = ContainerSpec {
            agent: AgentKind::Loom,
            image: &testbed_image(instance),
            loom_binary: Some(loom_bin),
            prompt_file: &prompt_path,
            out_dir: &out_dir,
            provider,
            model,
            base_url,
            api_key_env,
            api_key,
            timeout_secs: args.timeout_secs,
            max_turns: args.max_turns,
            bare: false,
            extra_env: &extra,
            container_name: &container_name,
        };
        println!("  probe {label} {slug} run {run} (attempt {attempt})…");
        let status = Command::new("docker")
            .args(docker_run_argv(&spec))
            .status()
            .unwrap_or_else(|error| panic!("spawning docker for {container_name}: {error}"));
        let _ = status.code().unwrap_or(-1);
        let run_json = out_dir.join("run.json");
        if !run_json.exists() {
            continue;
        }
        let (events, task_failed) = swebench::run_outcome(&run_json);
        if let Some(reason) = &task_failed
            && reason.contains("rate_limited")
            && attempt < MAX_ATTEMPTS
        {
            println!("  rate-limited; cooling down {RATE_LIMIT_COOLDOWN_SECS}s…");
            std::thread::sleep(std::time::Duration::from_secs(RATE_LIMIT_COOLDOWN_SECS));
            continue;
        }
        let metrics = swebench::run_metrics(&run_json);
        return RunStat {
            instance: instance.to_owned(),
            run,
            events: events as u64,
            turns: metrics.turns,
            usage: metrics.usage,
            task_failed,
        };
    }
    failed_stat(instance, run, "no run.json produced (docker failed?)")
}

/// A probe that never started: zero tokens but marked failed, so the
/// adoption gate (no new failures) rejects it.
fn failed_stat(instance: &str, run: u64, reason: &str) -> RunStat {
    RunStat {
        instance: instance.to_owned(),
        run,
        events: 0,
        turns: 0,
        usage: Default::default(),
        task_failed: Some(reason.to_owned()),
    }
}

/// Probes one proposal across all selected instances and runs.
#[allow(clippy::too_many_arguments)]
fn probe_proposal(
    proposal: &Proposal,
    instances: &[String],
    args: &Args,
    base: &Path,
    loom_bin: &Path,
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_env: &str,
    api_key: &str,
) -> Vec<RunStat> {
    let tasks: Vec<(String, u64)> = instances
        .iter()
        .flat_map(|instance| (1..=args.runs as u64).map(|run| (instance.clone(), run)))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = Mutex::new(Vec::<RunStat>::new());
    let workers = args.jobs.max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some((instance, run)) = tasks.get(index).cloned() else {
                        break;
                    };
                    let stat = probe_run(
                        &proposal.label,
                        &proposal.overlay,
                        &instance,
                        run,
                        args,
                        base,
                        loom_bin,
                        provider,
                        model,
                        base_url,
                        api_key_env,
                        api_key,
                    );
                    results.lock().unwrap().push(stat);
                }
            });
        }
    });
    let mut stats = results.into_inner().unwrap();
    stats.sort_by(|a, b| (&a.instance, a.run).cmp(&(&b.instance, b.run)));
    stats
}

fn main() {
    let args = parse_args();
    let base = args.eval_dir.join("swebench");
    let model_hint = args
        .model
        .clone()
        .or_else(|| std::env::var("LOOM_EVAL_MODEL").ok())
        .unwrap_or_else(|| "deepseek-flash".to_owned());
    let sweep_path = args.sweep.clone().unwrap_or_else(|| {
        base.join(format!(
            "swebench-sweep-{}.json",
            swebench::slug(&model_hint)
        ))
    });
    println!("baseline sweep: {}", sweep_path.display());

    let baseline = opt::load_baseline(&sweep_path).unwrap_or_else(|error| {
        eprintln!("loading {}: {error}", sweep_path.display());
        std::process::exit(2);
    });
    let provider = args
        .provider
        .clone()
        .unwrap_or_else(|| baseline.provider.clone());
    let model = args.model.clone().unwrap_or_else(|| baseline.model.clone());
    let base_url = args.base_url.clone().or_else(|| baseline.base_url.clone());
    let (key_env, key_value) = resolve_api_key(&provider);
    let loom_bin = args
        .loom_bin
        .clone()
        .unwrap_or_else(|| base.join("loom-linux/loom"));
    if !loom_bin.exists() {
        eprintln!(
            "no linux loom binary at {} — pass --loom-bin",
            loom_bin.display()
        );
        std::process::exit(2);
    }
    let loom_bin = loom_bin.canonicalize().expect("canonicalize loom binary");

    let ledger_path = args
        .ledger
        .clone()
        .unwrap_or_else(|| base.join("selfopt-ledger.json"));
    let ledger = opt::load_ledger(&ledger_path).unwrap_or_else(|error| {
        eprintln!("ledger {}: {error}", ledger_path.display());
        std::process::exit(2);
    });
    // The most recent lessons are the live ones; cap the context the
    // proposer reads.
    let lessons: Vec<Lesson> = ledger
        .iter()
        .rev()
        .take(10)
        .flat_map(|entry| entry.lessons.iter().rev())
        .cloned()
        .collect();
    println!(
        "baseline: {} run(s) over {} instance(s), provider={provider}, model={model}",
        baseline.runs.len(),
        baseline
            .runs
            .iter()
            .map(|r| r.instance.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
    );
    if !lessons.is_empty() {
        println!("ledger: {} retained lesson(s)", lessons.len());
    }

    // Selected instances: hard-first (RHO) — capped, deterministic.
    let mut instances: Vec<String> = opt::order_instances(&baseline.runs);
    instances.truncate(args.instances);
    let baseline_runs: Vec<RunStat> = baseline
        .runs
        .iter()
        .filter(|run| instances.contains(&run.instance))
        .cloned()
        .collect();

    // Propose: rules first (bounded to half the budget), then the
    // model proposer fills the rest.
    let base_overlay = Overlay::new();
    let mut proposals = opt::rule_proposals(&base_overlay, args.max_proposals / 2);
    if !args.rules_only
        && let Some(remaining) = args.max_proposals.checked_sub(proposals.len())
        && remaining > 0
    {
        let stats = opt::per_instance(&baseline_runs);
        let prompt = opt::build_proposal_prompt(&baseline, &stats, &lessons);
        match model_proposals(
            &provider,
            &model,
            base_url.as_deref(),
            &key_value,
            &prompt,
            remaining,
        ) {
            Ok((model_made, dropped)) => {
                if dropped > 0 {
                    println!("model proposer: {dropped} proposal(s) dropped as invalid/duplicate");
                }
                for proposal in model_made {
                    if proposals
                        .iter()
                        .all(|existing: &Proposal| existing.overlay != proposal.overlay)
                    {
                        proposals.push(proposal);
                    }
                }
            }
            Err(error) => {
                eprintln!("model proposer failed ({error}); continuing with rules only");
            }
        }
    }
    println!("proposals: {}", proposals.len());
    for proposal in &proposals {
        println!(
            "  [{}] {} ({:?}) — {}",
            proposal.origin, proposal.label, proposal.class, proposal.hypothesis
        );
    }

    if args.dry_run {
        let plan = serde_json::json!({
            "sweep": sweep_path.display().to_string(),
            "provider": provider,
            "model": model,
            "instances": instances,
            "runs": args.runs,
            "proposals": proposals,
        });
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        return;
    }

    // Replay pre-probe: reject projection-affecting proposals whose
    // projected improvement cannot clear the adoption gate, before any
    // docker probes are spent (replay-context integration).
    let proposals: Vec<Proposal> = if args.pre_probe.is_empty() {
        proposals
    } else {
        println!(
            "pre-probe: replaying {} trace dir(s) (window {}, reserved {})",
            args.pre_probe.len(),
            args.pre_probe_window,
            args.pre_probe_reserved,
        );
        let mut kept = Vec::new();
        for proposal in proposals {
            let outcome = opt::pre_probe_proposal(
                &proposal.overlay,
                &args.pre_probe,
                args.pre_probe_window,
                args.pre_probe_reserved,
            );
            println!(
                "  {} → {} ({})",
                proposal.label,
                if outcome.kept { "keep" } else { "REJECT" },
                outcome.note
            );
            if outcome.kept {
                kept.push(proposal);
            }
        }
        kept
    };

    // Probe every proposal, then decide.
    let mut decisions: Vec<Decision> = Vec::new();
    let mut adopted: Vec<&Proposal> = Vec::new();
    for proposal in &proposals {
        println!("probing {}…", proposal.label);
        let probe = probe_proposal(
            proposal,
            &instances,
            &args,
            &base,
            &loom_bin,
            &provider,
            &model,
            base_url.as_deref(),
            &key_env,
            &key_value,
        );
        let comparison = opt::compare(&baseline_runs, &probe);
        let decision = opt::decide(&proposal.label, &comparison);
        println!(
            "  {} → {} ({})",
            proposal.label,
            if decision.adopted { "ADOPT" } else { "reject" },
            decision.reason
        );
        if decision.adopted {
            adopted.push(proposal);
        }
        decisions.push(decision);
    }

    // Compose: merge the winners and re-probe the merge (Harness²'s
    // selective composition — the merge earns its own evidence).
    let mut merged = Overlay::new();
    let mut merged_decision: Option<Decision> = None;
    let adopted_refs: Vec<&Overlay> = adopted.iter().map(|p| &p.overlay).collect();
    if adopted.len() > 1 {
        match opt::merge_overlays(&adopted_refs) {
            Ok(candidate) => {
                let label = opt::overlay_label(&candidate);
                println!("composed overlay {label}; re-probing…");
                let merged_proposal = Proposal::labeled(
                    candidate,
                    ProposalClass::Invariant,
                    "merge of independently-winning overlays".to_owned(),
                    format!("composed from {} adopted proposals", adopted.len()),
                    "compose",
                );
                let probe = probe_proposal(
                    &merged_proposal,
                    &instances,
                    &args,
                    &base,
                    &loom_bin,
                    &provider,
                    &model,
                    base_url.as_deref(),
                    &key_env,
                    &key_value,
                );
                let comparison = opt::compare(&baseline_runs, &probe);
                let decision = opt::decide(&label, &comparison);
                println!(
                    "  {label} → {} ({})",
                    if decision.adopted { "ADOPT" } else { "reject" },
                    decision.reason
                );
                if decision.adopted {
                    merged = merged_proposal.overlay;
                    merged_decision = Some(decision);
                }
            }
            Err(conflict) => {
                eprintln!("compose: conflicting knobs, keeping individual adoptions ({conflict})");
            }
        }
    } else if let [only] = adopted.as_slice() {
        merged = only.overlay.clone();
    }
    // Tiers (HarnessPro): the merge is `heavy`; `lite` is the single
    // best adopted overlay. Both earned their own probes — no
    // unverified interpolation between them.
    let frontier = opt::pareto_frontier(&decisions);
    let promoted_labels: Vec<String> = decisions
        .iter()
        .filter(|d| d.promoted)
        .map(|d| d.label.clone())
        .collect();
    let exported: Overlay = if args.tier == "lite" {
        decisions
            .iter()
            .filter(|d| d.adopted)
            .max_by(|a, b| {
                a.input_improvement_pct
                    .total_cmp(&b.input_improvement_pct)
                    .then(b.probe_failures.cmp(&a.probe_failures))
            })
            .and_then(|best| {
                adopted
                    .iter()
                    .find(|p| p.label == best.label)
                    .map(|p| p.overlay.clone())
            })
            .unwrap_or_else(|| merged.clone())
    } else {
        merged.clone()
    };

    // Lessons: only contrast-supported statements (GEPA rule) — each
    // individually adopted proposal earned its own probe contrast.
    let mut new_lessons: Vec<Lesson> = adopted
        .iter()
        .map(|proposal| {
            let decision = decisions
                .iter()
                .find(|d| d.label == proposal.label)
                .expect("decision per proposal");
            Lesson {
                statement: proposal.hypothesis.clone(),
                evidence: format!("{}: {}", proposal.label, decision.reason),
            }
        })
        .collect();
    if let Some(decision) = &merged_decision
        && decision.adopted
    {
        new_lessons.push(Lesson {
            statement: "composed overlays keep their wins when merged".to_owned(),
            evidence: format!("{}: {}", decision.label, decision.reason),
        });
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned());
    let entry = LedgerEntry {
        timestamp: timestamp.clone(),
        sweep: sweep_path.display().to_string(),
        provider: provider.clone(),
        model: model.clone(),
        base_overlay: base_overlay.clone(),
        proposals: proposals.clone(),
        decisions: decisions.clone(),
        adopted: adopted.iter().map(|p| p.label.clone()).collect(),
        promoted: promoted_labels.clone(),
        merged: merged.clone(),
        lessons: new_lessons.clone(),
    };
    opt::append_ledger(&ledger_path, &entry)
        .unwrap_or_else(|error| panic!("ledger {}: {error}", ledger_path.display()));

    // Report: markdown next to the sweep artifacts, with the export
    // lines a later sweep would set.
    let mut report = String::new();
    report.push_str(&format!(
        "# selfopt report {timestamp}\n\nBaseline: `{}` ({provider}/{model}, {} instance(s), {} run(s)). Adoption gate: median input tokens −{ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT}% minimum, events not regressed, no new failures.\n\n## probes\n\n| proposal | origin | class | verdict | evidence |\n|---|---|---|---|---|\n",
        sweep_path.display(),
        instances.len(),
        baseline_runs.len(),
    ));
    for (index, decision) in decisions.iter().enumerate() {
        let proposal = &proposals[index];
        let verdict = if decision.adopted {
            "ADOPT"
        } else if decision.promoted {
            "promoted"
        } else {
            "reject"
        };
        report.push_str(&format!(
            "| `{}` | {} | {:?} | {verdict} | {} |\n",
            decision.label, proposal.origin, proposal.class, decision.reason,
        ));
    }
    if !frontier.is_empty() {
        report.push_str("\n## tiers (Pareto frontier over input/events/failures)\n\n| overlay | input | events | failures | frontier |\n|---|---|---|---|---|\n");
        for decision in decisions.iter().filter(|d| d.adopted) {
            let on = frontier.contains(&decision.label);
            report.push_str(&format!(
                "| `{}` | {:+.1}% | {:+.1}% | {} | {} |\n",
                decision.label,
                decision.input_improvement_pct,
                decision.events_improvement_pct,
                decision.probe_failures,
                if on { "yes" } else { "dominated" },
            ));
        }
        report.push_str("\n`--tier` exports one verified point: lite = best single overlay, heavy = the re-probed merge.\n");
    }
    if !promoted_labels.is_empty() {
        report.push_str("\n## promoted, not deployed\n\n");
        for decision in decisions.iter().filter(|d| d.promoted) {
            report.push_str(&format!(
                "- **{}** — input and failure gates passed; the events trade blocked deployment ({})\n",
                decision.label, decision.reason
            ));
        }
    }
    if !exported.is_empty() {
        report.push_str(&format!(
            "\n## exported overlay ({} tier)\n\n```sh\n",
            args.tier
        ));
        for (env, value) in &exported {
            report.push_str(&format!("export {env}={value}\n"));
        }
        report.push_str("```\n");
    } else {
        report.push_str("\nNo overlay adopted; the shipped defaults stand.\n");
    }
    if !new_lessons.is_empty() {
        report.push_str("\n## lessons (contrast-supported)\n\n");
        for lesson in &new_lessons {
            report.push_str(&format!(
                "- **{}** — {}\n",
                lesson.statement, lesson.evidence
            ));
        }
    }
    let report_path = base.join(format!("selfopt-report-{timestamp}.md"));
    std::fs::write(&report_path, &report)
        .unwrap_or_else(|error| panic!("{}: {error}", report_path.display()));
    println!(
        "\ndone: {} adopted ({} promoted), exported {} tier — report {}, ledger {}",
        adopted.len(),
        promoted_labels.len(),
        args.tier,
        report_path.display(),
        ledger_path.display()
    );
}
