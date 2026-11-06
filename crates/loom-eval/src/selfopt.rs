//! The offline self-optimization outer loop (roadmap item 20.3).
//!
//! Reads the artifacts a `swebench-run` sweep already produced — the
//! sweep report plus each run's NDJSON event stream — and treats the
//! harness itself as the search space: every `LOOM_*` override seam
//! from item 20.2 (`loom_core::engine::BUDGET_ENV_VARS` /
//! [`PRUNE_ENV_VARS`]) is a knob, and a *proposal* is an env overlay
//! the in-container engine consumes with zero script or binary change.
//!
//! The loop is the contrastive shape Harness² (arXiv 2610.14224)
//! describes — *propose, probe, compose* — restricted to what an
//! offline harness can measure honestly:
//!
//! 1. **Propose** — a deterministic rule proposer walks every knob
//!    (halve/double, clamped), and optionally a model proposer reads
//!    the baseline traces plus retained lessons and returns
//!    hypothesis-driven overlays, each tagged `invariant` (a
//!    context-management change that should hold across domains) or
//!    `domain` (task-class-specific), mirroring Harness²'s two
//!    candidate classes.
//! 2. **Probe** — re-run the *same* instances with one overlay per
//!    probe (`--jobs` containers at a time), the same model and image
//!    the baseline used. Paired by instance, so the signal is the
//!    per-instance delta, never a cross-run average.
//! 3. **Compose** — adopt a proposal only on evidence (median input
//!    tokens down ≥ [`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`], events
//!    not regressed beyond [`EVENTS_REGRESSION_MAX_PERCENT`], no new
//!    failures), then merge the adopted overlays — Harness²'s
//!    selective composition — and re-probe the merged overlay before
//!    it wins anything.
//!
//! GEPA (arXiv 2507.19457) supplies the lesson discipline: the model
//! proposer reads *full traces* (here: the per-run usage and failure
//! reductions, not raw transcripts), and a lesson enters the ledger
//! only when a probe contrast supports it — never on vibes.
//!
//! Everything here is offline-testable: the docker probes live in the
//! `selfopt` binary; this module owns the pure parts (knob table,
//! validation, proposals, paired comparison, composition, ledger).
//!
//! The replay pre-probe ([`pre_probe_proposal`]) sits between propose
//! and probe: a projection-affecting overlay is replayed offline
//! through `loom_core::engine::replay` against locally recorded trace
//! directories, and a proposal whose projected median input-token
//! improvement is below the adoption gate is rejected before any
//! docker probes are spent. Same gate constants as the real probe —
//! the pre-probe only ever tightens, never loosens.

// Harness internals: panicking on a malformed fixture is the intended
// behavior in tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::swebench::{self, RunUsage};

// --------------------------------------------------------- knobs ---

/// One tunable `LOOM_*` seam the overlay may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knob {
    /// Environment variable the in-container engine reads.
    pub env: &'static str,
    /// Short name for labels and reports (e.g. `prune_trigger_percent`).
    pub name: &'static str,
    /// The value `ToolBudgets::default` / `PruneConfig::default`
    /// produce — the harness's shipped behavior, hardcoded here
    /// because the crate-private constants in `loom-tools` are not
    /// exported (and re-deriving them would couple eval to tools).
    pub default: u64,
    /// Inclusive lower bound for proposals.
    pub floor: u64,
    /// Inclusive upper bound for proposals.
    pub ceiling: u64,
    /// Unit for report rendering (`bytes`, `tokens`, `percent`,
    /// `turns`, `lines`).
    pub unit: &'static str,
    /// One line of what the knob trades off, shown to the model
    /// proposer so hypotheses name a mechanism, not just a direction.
    pub doc: &'static str,
}

/// The sweepable knob table: every pure env seam from roadmap item
/// 20.2. Kept in lockstep with the engine's `BUDGET_ENV_VARS` and
/// `PRUNE_ENV_VARS` — a seam added there without a row here is
/// invisible to the optimizer, and a row without a seam is inert.
pub const KNOBS: &[Knob] = &[
    Knob {
        env: "LOOM_READ_HEAD_BYTES",
        name: "read_head_bytes",
        default: 56 * 1024,
        floor: 8 * 1024,
        ceiling: 256 * 1024,
        unit: "bytes",
        doc: "head of a read file shown whole; smaller hides more of large files",
    },
    Knob {
        env: "LOOM_COMMAND_HEAD_BYTES",
        name: "command_head_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "head of command output kept; smaller truncates noisy tools sooner",
    },
    Knob {
        env: "LOOM_COMMAND_TAIL_BYTES",
        name: "command_tail_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "tail of command output kept alongside the head (errors live at the end)",
    },
    Knob {
        env: "LOOM_EDIT_ECHO_MAX_BYTES",
        name: "edit_echo_max_bytes",
        default: 8 * 1024,
        floor: 1024,
        ceiling: 32 * 1024,
        unit: "bytes",
        doc: "cap on the edited-region echo; smaller echoes save context per edit",
    },
    Knob {
        env: "LOOM_EDIT_ECHO_LINES",
        name: "edit_echo_lines",
        default: 3,
        floor: 1,
        ceiling: 16,
        unit: "lines",
        doc: "context lines around an edit in the echo; more lines, more orientation",
    },
    Knob {
        env: "LOOM_SYMBOLS_OVERVIEW_MAX_BYTES",
        name: "symbols_overview_max_bytes",
        default: 8 * 1024,
        floor: 1024,
        ceiling: 32 * 1024,
        unit: "bytes",
        doc: "cap on the symbol-tree overview; larger keeps more of the outline",
    },
    Knob {
        env: "LOOM_SKILL_HEAD_BYTES",
        name: "skill_head_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "head of a loaded skill body; smaller may cut instructions mid-way",
    },
    Knob {
        env: "LOOM_REPLACE_SYMBOL_HEAD_BYTES",
        name: "replace_symbol_head_bytes",
        default: 400,
        floor: 100,
        ceiling: 4 * 1024,
        unit: "bytes",
        doc: "head echoed after replace_symbol; confirmation context per edit",
    },
    Knob {
        env: "LOOM_MCP_IDLE_EVICT_TURNS",
        name: "mcp_idle_evict_turns",
        default: 8,
        floor: 1,
        ceiling: 64,
        unit: "turns",
        doc: "idle turns before a loaded MCP tool demotes back to the menu",
    },
    Knob {
        env: "LOOM_PRUNE_TRIGGER_PERCENT",
        name: "prune_trigger_percent",
        default: 80,
        floor: 50,
        ceiling: 95,
        unit: "percent",
        doc: "context fill that triggers pruning; lower prunes earlier, more often",
    },
    Knob {
        env: "LOOM_PRUNE_FLOOR_TOKENS",
        name: "prune_floor_tokens",
        default: 64_000,
        floor: 16_000,
        ceiling: 256_000,
        unit: "tokens",
        doc: "memory the compiler reclaims down to when pruning fires",
    },
    Knob {
        env: "LOOM_PRUNE_MIN_RECLAIM_TOKENS",
        name: "prune_min_reclaim_tokens",
        default: 20_000,
        floor: 5_000,
        ceiling: 100_000,
        unit: "tokens",
        doc: "reclaim below this is not worth a rewrite of the window",
    },
    Knob {
        env: "LOOM_PHASE_MASK_TURNS",
        name: "phase_mask_turns",
        default: 0,
        floor: 1,
        ceiling: 16,
        unit: "turns",
        doc: "0 = off; during the writing phase, stub exploration reads older than this window",
    },
    Knob {
        env: "LOOM_PRUNE_TAIL_TOKENS",
        name: "prune_tail_tokens",
        default: 20_000,
        floor: 4_000,
        ceiling: 80_000,
        unit: "tokens",
        doc: "recent turns the compiler never elides; larger protects live chains",
    },
    Knob {
        env: "LOOM_INITIAL_TURNS",
        name: "initial_turns",
        default: 50,
        floor: 8,
        ceiling: 50,
        unit: "turns",
        doc: "turns granted up front (engine default: the full ceiling); smaller grants cut wasted spins on hopeless tasks",
    },
    Knob {
        env: "LOOM_EXTENSION_TURNS",
        name: "extension_turns",
        default: 8,
        floor: 2,
        ceiling: 16,
        unit: "turns",
        doc: "turns added per progress-evidence grant (engine default: off); the staged-budget lever of item 26",
    },
    Knob {
        env: "LOOM_MAX_TURN_EXTENSIONS",
        name: "max_turn_extensions",
        default: 2,
        floor: 1,
        ceiling: 5,
        unit: "grants",
        doc: "extension grants per task (engine default: off); bounds how far initial_turns can grow toward the ceiling",
    },
];

/// Looks a knob up by its env var name.
pub fn knob_by_env(env: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|knob| knob.env == env)
}

/// An env overlay: knob env var → value. Ordered, so serialization is
/// deterministic.
pub type Overlay = BTreeMap<String, String>;

/// Validates an overlay against the knob table: every key must be a
/// known seam, every value an integer inside the knob's bounds.
pub fn validate_overlay(overlay: &Overlay) -> Result<(), String> {
    for (env, value) in overlay {
        let knob = knob_by_env(env).ok_or_else(|| format!("unknown knob: {env}"))?;
        let parsed: u64 = value
            .parse()
            .map_err(|_| format!("{env}: not an integer: {value}"))?;
        if parsed < knob.floor || parsed > knob.ceiling {
            return Err(format!(
                "{env}: {parsed} outside [{}, {}]",
                knob.floor, knob.ceiling
            ));
        }
    }
    Ok(())
}

/// Deterministic, filesystem-safe label for an overlay — the knob
/// short names and values, joined: `prune_trigger_percent-70` or
/// `prune_trigger_percent-70+read_head_bytes-32768`. Used for probe
/// directories and report rows.
pub fn overlay_label(overlay: &Overlay) -> String {
    if overlay.is_empty() {
        return "baseline".to_owned();
    }
    let mut parts: Vec<String> = overlay
        .iter()
        .map(|(env, value)| {
            let name = knob_by_env(env).map(|knob| knob.name).unwrap_or("knob");
            format!("{name}-{value}")
        })
        .collect();
    parts.sort();
    parts.join("+")
}

// ----------------------------------------------------- proposals ---

/// Harness²'s two candidate classes: an `Invariant` proposal claims a
/// context-management improvement that should hold across domains; a
/// `Domain` proposal is specific to the task class being probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalClass {
    /// Cross-domain (planning/tool-use/context-management) change.
    Invariant,
    /// Task-class-specific change.
    Domain,
}

/// One candidate: an overlay plus the claim behind it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// Deterministic label (also the probe directory name).
    pub label: String,
    /// The env overlay the probe runs with.
    pub overlay: Overlay,
    /// Harness² candidate class.
    pub class: ProposalClass,
    /// One sentence of what the proposer expects to change, in the
    /// trace (mechanism, not just direction).
    pub hypothesis: String,
    /// Why the proposer believes it — the evidence it read.
    pub rationale: String,
    /// `rule` or `model`.
    pub origin: String,
}

impl Proposal {
    /// Builds a rule proposal's [`Proposal::label`] via
    /// [`overlay_label`]; model proposals get labels the same way
    /// after validation.
    pub fn labeled(
        overlay: Overlay,
        class: ProposalClass,
        hypothesis: String,
        rationale: String,
        origin: &str,
    ) -> Self {
        let label = overlay_label(&overlay);
        Self {
            label,
            overlay,
            class,
            hypothesis,
            rationale,
            origin: origin.to_owned(),
        }
    }
}

/// Deterministic first-pass proposer: for every knob, halve and double
/// the shipped default, clamped to the knob's bounds. No-ops (the
/// clamp rounds back to the baseline value) are skipped. This exists
/// so the loop has signal before the model proposer earns trust, and
/// so regressions in the model path never leave the loop empty.
pub fn rule_proposals(baseline: &Overlay, max: usize) -> Vec<Proposal> {
    let mut out = Vec::new();
    for knob in KNOBS {
        let current: u64 = baseline
            .get(knob.env)
            .and_then(|value| value.parse().ok())
            .unwrap_or(knob.default);
        let candidates = [current / 2, current.saturating_mul(2)];
        for (raw, direction) in candidates.into_iter().zip(["halve", "double"]) {
            let candidate = raw.clamp(knob.floor, knob.ceiling);
            if candidate == current {
                continue;
            }
            let mut overlay = baseline.clone();
            overlay.insert(knob.env.to_owned(), candidate.to_string());
            if validate_overlay(&overlay).is_err()
                || out.iter().any(|p: &Proposal| p.overlay == overlay)
            {
                continue;
            }
            let hypothesis = format!("{} {} → {}: {}", knob.env, current, candidate, knob.doc);
            let rationale = format!(
                "deterministic {direction} of the shipped default ({} {}), clamped to bounds",
                knob.default, knob.unit
            );
            out.push(Proposal::labeled(
                overlay,
                ProposalClass::Invariant,
                hypothesis,
                rationale,
                "rule",
            ));
            if out.len() >= max {
                return out;
            }
        }
    }
    out
}

// -------------------------------------------------- baseline load ---

/// One run's reduced trace, loaded from the sweep report plus the
/// per-run `run.json` the sweep left behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStat {
    /// Dataset instance id.
    pub instance: String,
    /// 1-based run index within the sweep.
    pub run: u64,
    /// Event count (`run_outcome`).
    pub events: u64,
    /// `turn_started` count (`run_turns`).
    pub turns: u64,
    /// Token totals (`run_usage`).
    pub usage: RunUsage,
    /// Terminal `task_failed` reason, if the run died.
    pub task_failed: Option<String>,
}

/// The baseline a sweep report describes: identity plus per-run stats.
#[derive(Debug, Clone)]
pub struct Baseline {
    /// Provider the sweep ran.
    pub provider: String,
    /// Model the sweep ran.
    pub model: String,
    /// Gateway base URL, if the sweep used one.
    pub base_url: Option<String>,
    /// `full` or `bare`.
    pub arm: String,
    /// Per-run reduced stats, sweep order.
    pub runs: Vec<RunStat>,
}

/// Loads a sweep report and its per-run `run.json` artifacts.
///
/// The report rows already carry events, failures, and usage; turns
/// are read from `instances/<slug>/run-<n>/run.json` next to the
/// report (best-effort: a missing stream reads as 0 turns, the same
/// convention [`swebench::run_turns`] applies).
pub fn load_baseline(sweep_json: &Path) -> std::io::Result<Baseline> {
    let body = std::fs::read_to_string(sweep_json)?;
    let value: serde_json::Value = serde_json::from_str(&body)?;
    let base_dir = sweep_json.parent().unwrap_or_else(|| Path::new("."));
    let mut runs = Vec::new();
    for instance in value["instances"].as_array().into_iter().flatten() {
        let instance_id = instance["instance_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let slug = swebench::slug(&instance_id);
        for run in instance["runs"].as_array().into_iter().flatten() {
            let index = run["run"].as_u64().unwrap_or_default();
            let run_json = base_dir
                .join("instances")
                .join(&slug)
                .join(format!("run-{index}"))
                .join("run.json");
            let usage_json = &run["usage"];
            let usage = RunUsage {
                input_tokens: usage_json["input_tokens"].as_u64().unwrap_or(0),
                cached_input_tokens: usage_json["cached_input_tokens"].as_u64(),
                output_tokens: usage_json["output_tokens"].as_u64().unwrap_or(0),
                model_requests: usage_json["model_requests"].as_u64().unwrap_or(0),
            };
            runs.push(RunStat {
                instance: instance_id.clone(),
                run: index,
                events: run["events"].as_u64().unwrap_or(0),
                turns: swebench::run_turns(&run_json),
                usage,
                task_failed: run["task_failed"].as_str().map(str::to_owned),
            });
        }
    }
    Ok(Baseline {
        provider: value["provider"].as_str().unwrap_or_default().to_owned(),
        model: value["model"].as_str().unwrap_or_default().to_owned(),
        base_url: value["base_url"].as_str().map(str::to_owned),
        arm: value["arm"].as_str().unwrap_or("full").to_owned(),
        runs,
    })
}

// ------------------------------------------------ paired compare ---

/// Per-instance paired aggregates: medians over that instance's runs.
#[derive(Debug, Clone, Default)]
pub struct InstanceStat {
    /// Dataset instance id.
    pub instance: String,
    /// Median input tokens across runs.
    pub median_input_tokens: f64,
    /// Median event count across runs.
    pub median_events: f64,
    /// Median turn count across runs.
    pub median_turns: f64,
    /// Runs that ended in `task_failed`.
    pub failed_runs: usize,
    /// Run count the median came from.
    pub runs: usize,
}

/// Groups a run list into per-instance median aggregates, sorted by
/// instance id (deterministic order for reports and comparisons).
pub fn per_instance(runs: &[RunStat]) -> Vec<InstanceStat> {
    let mut by_instance: BTreeMap<&str, Vec<&RunStat>> = BTreeMap::new();
    for run in runs {
        by_instance
            .entry(run.instance.as_str())
            .or_default()
            .push(run);
    }
    by_instance
        .into_iter()
        .map(|(instance, group)| InstanceStat {
            instance: instance.to_owned(),
            median_input_tokens: median(
                &group
                    .iter()
                    .map(|r| r.usage.input_tokens as f64)
                    .collect::<Vec<_>>(),
            ),
            median_events: median(&group.iter().map(|r| r.events as f64).collect::<Vec<_>>()),
            median_turns: median(&group.iter().map(|r| r.turns as f64).collect::<Vec<_>>()),
            failed_runs: group.iter().filter(|r| r.task_failed.is_some()).count(),
            runs: group.len(),
        })
        .collect()
}

/// Median of an arbitrary sample (sorted middle, mean of the middle
/// pair when even). Empty samples read as `0.0` — a probe that never
/// started must not read as an improvement.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// A probe compared against its baseline, paired by instance.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// Per-instance deltas, baseline order.
    pub per_instance: Vec<InstanceDelta>,
    /// Median over instances of the input-token improvement percent
    /// (positive = the probe used fewer tokens).
    pub median_input_improvement_pct: f64,
    /// Median over instances of the event-count improvement percent
    /// (positive = fewer events).
    pub median_events_improvement_pct: f64,
    /// Baseline runs that failed vs probe runs that failed (summed).
    pub baseline_failures: usize,
    /// Probe failures — must not exceed the baseline's.
    pub probe_failures: usize,
}

/// One instance's paired delta.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceDelta {
    /// Dataset instance id.
    pub instance: String,
    /// Baseline median input tokens.
    pub baseline_input_tokens: f64,
    /// Probe median input tokens.
    pub probe_input_tokens: f64,
    /// `(baseline - probe) / baseline * 100`; positive = improvement.
    pub input_improvement_pct: f64,
    /// Baseline median events.
    pub baseline_events: f64,
    /// Probe median events.
    pub probe_events: f64,
    /// `(baseline - probe) / baseline * 100` on events.
    pub events_improvement_pct: f64,
}

/// Pairs a probe against a baseline by instance id. Instances only the
/// probe ran are ignored; instances only the baseline ran count as
/// missing evidence (no delta) — the caller's instance list is the
/// baseline's by construction in the binary.
pub fn compare(baseline: &[RunStat], probe: &[RunStat]) -> Comparison {
    let base_stats = per_instance(baseline);
    let probe_stats = per_instance(probe);
    let mut per_instance = Vec::new();
    for base in &base_stats {
        let Some(probe) = probe_stats.iter().find(|s| s.instance == base.instance) else {
            continue;
        };
        let input_pct = if base.median_input_tokens > 0.0 {
            (base.median_input_tokens - probe.median_input_tokens) / base.median_input_tokens
                * 100.0
        } else {
            0.0
        };
        let events_pct = if base.median_events > 0.0 {
            (base.median_events - probe.median_events) / base.median_events * 100.0
        } else {
            0.0
        };
        per_instance.push(InstanceDelta {
            instance: base.instance.clone(),
            baseline_input_tokens: base.median_input_tokens,
            probe_input_tokens: probe.median_input_tokens,
            input_improvement_pct: input_pct,
            baseline_events: base.median_events,
            probe_events: probe.median_events,
            events_improvement_pct: events_pct,
        });
    }
    let input_pcts: Vec<f64> = per_instance
        .iter()
        .map(|d| d.input_improvement_pct)
        .collect();
    let event_pcts: Vec<f64> = per_instance
        .iter()
        .map(|d| d.events_improvement_pct)
        .collect();
    Comparison {
        median_input_improvement_pct: median(&input_pcts),
        median_events_improvement_pct: median(&event_pcts),
        baseline_failures: baseline.iter().filter(|r| r.task_failed.is_some()).count(),
        probe_failures: probe.iter().filter(|r| r.task_failed.is_some()).count(),
        per_instance,
    }
}

/// The evidence bar a probe must clear to adopt its proposal.
pub const ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT: f64 = 5.0;
/// Events may not regress (grow) beyond this percent — a knob that
/// saves tokens by making the model flail is a bad trade.
pub const EVENTS_REGRESSION_MAX_PERCENT: f64 = 10.0;

/// The verdict on one probed proposal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    /// Proposal label.
    pub label: String,
    /// `adopt` or `reject`.
    pub adopted: bool,
    /// The numbers behind the verdict, rendered as one line.
    pub reason: String,
    /// Median input-token improvement percent (positive = fewer).
    #[serde(default)]
    pub input_improvement_pct: f64,
    /// Median event-count improvement percent (positive = fewer).
    #[serde(default)]
    pub events_improvement_pct: f64,
    /// Baseline failed runs (for tier ranking).
    #[serde(default)]
    pub baseline_failures: usize,
    /// Probe failed runs (for tier ranking).
    #[serde(default)]
    pub probe_failures: usize,
    /// Promotion/deployment split (ParetoSaddler): input and failure
    /// gates passed — the candidate stays in play for later sweeps —
    /// but the events gate blocked deployment.
    #[serde(default)]
    pub promoted: bool,
}

/// Applies the adoption gate to one comparison: median input tokens
/// down ≥ [`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`], events not up beyond
/// [`EVENTS_REGRESSION_MAX_PERCENT`], and no new failed runs.
pub fn decide(label: &str, comparison: &Comparison) -> Decision {
    let input_ok = comparison.median_input_improvement_pct >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT;
    let events_ok = comparison.median_events_improvement_pct >= -EVENTS_REGRESSION_MAX_PERCENT;
    let failures_ok = comparison.probe_failures <= comparison.baseline_failures;
    let adopted = input_ok && events_ok && failures_ok;
    let reason = format!(
        "input {:+.1}%, events {:+.1}%, failures {}→{}",
        comparison.median_input_improvement_pct,
        comparison.median_events_improvement_pct,
        comparison.baseline_failures,
        comparison.probe_failures
    );
    Decision {
        label: label.to_owned(),
        adopted,
        reason,
        input_improvement_pct: comparison.median_input_improvement_pct,
        events_improvement_pct: comparison.median_events_improvement_pct,
        baseline_failures: comparison.baseline_failures,
        probe_failures: comparison.probe_failures,
        // Deployed only on the full gate; promoted when everything but
        // the events trade cleared, so the ledger keeps the candidate.
        promoted: !adopted && input_ok && failures_ok,
    }
}

/// Labels of adopted proposals on the Pareto frontier of (input
/// improvement, events improvement, probe failures) — HarnessPro's
/// cost tiers: each frontier entry is a defensible operating point,
/// no other adopted overlay beats it on all three. Input order.
pub fn pareto_frontier(decisions: &[Decision]) -> Vec<String> {
    let adopted: Vec<&Decision> = decisions.iter().filter(|d| d.adopted).collect();
    adopted
        .iter()
        .filter(|d| {
            !adopted.iter().any(|other| {
                other.label != d.label
                    && other.input_improvement_pct >= d.input_improvement_pct
                    && other.events_improvement_pct >= d.events_improvement_pct
                    && other.probe_failures <= d.probe_failures
                    && (other.input_improvement_pct > d.input_improvement_pct
                        || other.events_improvement_pct > d.events_improvement_pct
                        || other.probe_failures < d.probe_failures)
            })
        })
        .map(|d| d.label.clone())
        .collect()
}

/// Instance ids ordered for probing, hard-first (RHO): instances with
/// any failed baseline run come first — they carry the most signal
/// per probe dollar — then by total baseline input tokens descending,
/// ties by id. Every instance keeps exactly one entry.
pub fn order_instances(runs: &[RunStat]) -> Vec<String> {
    #[derive(Default)]
    struct Agg {
        failed: bool,
        input_tokens: u64,
    }
    let mut agg: std::collections::BTreeMap<String, Agg> = Default::default();
    for run in runs {
        let entry = agg.entry(run.instance.clone()).or_default();
        entry.failed |= run.task_failed.is_some();
        entry.input_tokens += run.usage.input_tokens;
    }
    let mut ordered: Vec<(String, Agg)> = agg.into_iter().collect();
    ordered.sort_by(|a, b| {
        b.1.failed
            .cmp(&a.1.failed)
            .then(b.1.input_tokens.cmp(&a.1.input_tokens))
            .then(a.0.cmp(&b.0))
    });
    ordered.into_iter().map(|(id, _)| id).collect()
}

// ------------------------------------------------------ compose ---

/// Merges adopted overlays in adoption order. Conflicting values for
/// the same knob abort the merge with the conflict named — Harness²
/// composes selectively, and a silent last-write-wins would hide which
/// won. (The binary re-probes the merged overlay either way.)
pub fn merge_overlays(overlays: &[&Overlay]) -> Result<Overlay, String> {
    let mut merged = Overlay::new();
    for overlay in overlays.iter().copied() {
        for (env, value) in overlay {
            if let Some(existing) = merged.get(env) {
                if existing != value {
                    return Err(format!("{env}: {existing} vs {value}"));
                }
                continue;
            }
            merged.insert(env.clone(), value.clone());
        }
    }
    Ok(merged)
}

// ------------------------------------------------------- ledger ---

/// A retained lesson — GEPA's discipline: only contrast-supported
/// statements survive. The evidence string names the probes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    /// One-sentence imperative ("prune at 70% for long test-loop tasks").
    pub statement: String,
    /// The probe contrasts that back it (labels + numbers).
    pub evidence: String,
}

/// One loop iteration's record — the ledger entry is the unit a later
/// run reads (via [`build_proposal_prompt`]) instead of re-deriving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// RFC 3339-ish local timestamp (the binary formats it).
    pub timestamp: String,
    /// Sweep report path the baseline came from.
    pub sweep: String,
    /// Provider / model the probes ran.
    pub provider: String,
    /// Model the probes ran.
    pub model: String,
    /// The overlay the baseline ran with (usually empty = shipped).
    pub base_overlay: Overlay,
    /// Proposals considered (label, class, hypothesis).
    pub proposals: Vec<Proposal>,
    /// Per-proposal decisions.
    pub decisions: Vec<Decision>,
    /// Labels adopted into the merged overlay.
    pub adopted: Vec<String>,
    /// Labels promoted but not deployed (events gate blocked): kept
    /// as candidates for later sweeps. Serde-defaulted — old ledgers
    /// predate the promotion/deployment split.
    #[serde(default)]
    pub promoted: Vec<String>,
    /// The merged overlay that resulted (empty when nothing adopted).
    pub merged: Overlay,
    /// Contrast-supported lessons only.
    pub lessons: Vec<Lesson>,
}

/// Reads the ledger; a missing file is an empty ledger (first run).
pub fn load_ledger(path: &Path) -> std::io::Result<Vec<LedgerEntry>> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&body).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    })
}

/// Appends one entry, rewriting the whole ledger atomically (write to
/// a sibling temp file, rename over) so a crash never truncates it.
pub fn append_ledger(path: &Path, entry: &LedgerEntry) -> std::io::Result<()> {
    let mut entries = load_ledger(path)?;
    entries.push(entry.clone());
    let body = serde_json::to_string_pretty(&entries).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body + "\n")?;
    std::fs::rename(&temp, path)
}

// ------------------------------------------------ model proposer ---

/// The model proposer's system contract: JSON in, JSON out, no
/// narration. Kept here so the prompt is reviewable and testable
/// alongside its parser.
pub const PROPOSER_SYSTEM_PROMPT: &str = "You are proposing harness-parameter changes for a coding agent you cannot see. You read reduced run statistics (tokens, events, turns, failures per instance) and retained lessons, then propose a small set of environment-variable overlays as your only output. Respond with a JSON array and nothing else. Each element: {\"overlay\": {\"LOOM_*\": value}, \"class\": \"invariant\"|\"domain\", \"hypothesis\": one sentence naming the mechanism you expect to change in the trace, \"rationale\": the statistic or lesson you read that supports it}. Rules: only keys from the provided knob table, values inside its bounds, integers; at most one overlay per knob; prefer few proposals with distinct mechanisms over many variations; never repeat an overlay the lessons already rejected.";

/// Builds the user prompt for the model proposer: the knob table, the
/// baseline aggregates, per-instance deltas worth seeing, and the
/// retained lessons — the full Actionable Side Information GEPA
/// prescribes, reduced to what the loop can defend.
pub fn build_proposal_prompt(
    baseline: &Baseline,
    stats: &[InstanceStat],
    lessons: &[Lesson],
) -> String {
    let knobs: Vec<serde_json::Value> = KNOBS
        .iter()
        .map(|knob| {
            serde_json::json!({
                "env": knob.env,
                "default": knob.default,
                "floor": knob.floor,
                "ceiling": knob.ceiling,
                "unit": knob.unit,
                "doc": knob.doc,
            })
        })
        .collect();
    let per_instance: Vec<serde_json::Value> = stats
        .iter()
        .map(|stat| {
            serde_json::json!({
                "instance": stat.instance,
                "median_input_tokens": stat.median_input_tokens,
                "median_events": stat.median_events,
                "median_turns": stat.median_turns,
                "failed_runs": stat.failed_runs,
                "runs": stat.runs,
            })
        })
        .collect();
    let lessons: Vec<serde_json::Value> = lessons
        .iter()
        .map(|lesson| {
            serde_json::json!({
                "statement": lesson.statement,
                "evidence": lesson.evidence,
            })
        })
        .collect();
    format!(
        "Baseline sweep: provider {} model {} arm {}.\n\nKnob table (the only keys you may set):\n{}\n\nPer-instance baseline medians:\n{}\n\nRetained lessons (contrast-supported only):\n{}\n\nPropose overlays as a JSON array.",
        baseline.provider,
        baseline.model,
        baseline.arm,
        serde_json::to_string_pretty(&knobs).unwrap_or_default(),
        serde_json::to_string_pretty(&per_instance).unwrap_or_default(),
        serde_json::to_string_pretty(&lessons).unwrap_or_default(),
    )
}

/// The model proposer's wire shape (raw JSON, before validation).
#[derive(Debug, Deserialize)]
struct RawProposal {
    overlay: Overlay,
    class: Option<String>,
    hypothesis: Option<String>,
    rationale: Option<String>,
}

/// Parses a model response into validated proposals: tolerant of code
/// fences and surrounding prose, dropping anything that fails
/// [`validate_overlay`] or duplicates an earlier overlay. Silent
/// drops are counted in the return so the binary can report drift.
pub fn parse_proposal_response(text: &str, max: usize) -> (Vec<Proposal>, usize) {
    let body = extract_json_array(text).unwrap_or_default();
    let Ok(raw) = serde_json::from_str::<Vec<RawProposal>>(&body) else {
        return (Vec::new(), 1);
    };
    let mut proposals = Vec::new();
    let mut dropped = 0usize;
    for item in raw {
        if proposals.len() >= max {
            dropped += 1;
            continue;
        }
        if validate_overlay(&item.overlay).is_err() {
            dropped += 1;
            continue;
        }
        if proposals
            .iter()
            .any(|p: &Proposal| p.overlay == item.overlay)
        {
            dropped += 1;
            continue;
        }
        let class = match item.class.as_deref() {
            Some("domain") => ProposalClass::Domain,
            _ => ProposalClass::Invariant,
        };
        let proposal = Proposal::labeled(
            item.overlay,
            class,
            item.hypothesis.unwrap_or_else(|| "unstated".to_owned()),
            item.rationale.unwrap_or_else(|| "unstated".to_owned()),
            "model",
        );
        proposals.push(proposal);
    }
    (proposals, dropped)
}

/// Finds the outermost JSON array in a response: direct parse first,
/// then fence-stripped, then the first `[` to the last `]`.
fn extract_json_array(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        return Some(trimmed.to_owned());
    }
    let unfenced = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.split_once('\n'))
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim);
    if let Some(body) = unfenced
        && body.starts_with('[')
    {
        return Some(body.to_owned());
    }
    let source = unfenced.unwrap_or(trimmed);
    let start = source.find('[')?;
    let end = source.rfind(']')?;
    (start < end).then(|| source[start..=end].to_owned())
}

// ------------------------------------------------------- pre-probe ---

/// The verdict of replaying one proposal offline before any docker
/// probes are spent on it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreProbeOutcome {
    /// `false` means the proposal is rejected without probing.
    pub kept: bool,
    /// The projected median input-token improvement across usable
    /// traces, in percent — `None` when replay could not price the
    /// overlay at all.
    pub projected_improvement_pct: Option<f64>,
    /// Why: always printed by the binary, kept in sweep reports.
    pub note: String,
}

/// Offline pre-probe (replay-context integration): replays each trace
/// directory under the provider's baseline config and under the
/// proposal overlay, and rejects the proposal when the projected median
/// input-token improvement stays below the adoption gate
/// ([`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`]).
///
/// Contract:
///
/// - Overlays that cannot change the projection (budget-only knobs —
///   the recorded transcript is fixed) are kept with a note; replay
///   cannot price them, so the docker probe decides.
/// - A trace whose baseline arm overruns past rescue is skipped and
///   named (its overlay comparison would not be apples-to-apples).
/// - A proposal with no usable traces is kept: the pre-probe tightens
///   on evidence, it never overrides the loop on none.
/// - Same gate constant as the docker probe, so a pre-probe reject can
///   never disagree with what the probe would have required.
#[must_use]
pub fn pre_probe_proposal(
    overlay: &Overlay,
    traces: &[std::path::PathBuf],
    window_tokens: u32,
    reserved_output_tokens: u32,
) -> PreProbeOutcome {
    let pairs: Vec<String> = overlay
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !loom_core::engine::replay::overlay_affects_projection(&pairs) {
        return PreProbeOutcome {
            kept: true,
            projected_improvement_pct: None,
            note: "overlay does not affect the projection (budget-only \
                   knobs); replay cannot price it, the docker probe decides"
                .to_owned(),
        };
    }

    let mut improvements: Vec<f64> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for trace_dir in traces {
        let trace = match loom_core::engine::replay::TraceInput::read(trace_dir) {
            Ok(trace) => trace,
            Err(error) => {
                skipped.push(format!("{} (unreadable: {error})", trace_dir.display()));
                continue;
            }
        };
        let (base_config, _) = loom_core::engine::engine::prune_with_env(&trace.provider, |_| None);
        let mut options = loom_core::engine::replay::ReplayOptions::new(&trace.provider);
        options.window_tokens = window_tokens;
        options.reserved_output_tokens = reserved_output_tokens;
        let Ok(baseline) = loom_core::engine::replay::replay_session(
            &trace.session,
            &trace.task,
            &options,
            base_config,
        ) else {
            skipped.push(format!("{} (baseline replay failed)", trace_dir.display()));
            continue;
        };
        if baseline.handoff_stopped_at.is_some() {
            skipped.push(format!(
                "{} (baseline overran past rescue at turn {})",
                trace_dir.display(),
                baseline.handoff_stopped_at.unwrap_or_default(),
            ));
            continue;
        }
        let Ok((config, _)) = loom_core::engine::replay::overlay_config(base_config, &pairs) else {
            skipped.push(format!("{} (overlay rejected)", trace_dir.display()));
            continue;
        };
        let Ok(overlay_replay) = loom_core::engine::replay::replay_session(
            &trace.session,
            &trace.task,
            &options,
            config,
        ) else {
            skipped.push(format!("{} (overlay replay failed)", trace_dir.display()));
            continue;
        };
        improvements.push(loom_core::engine::replay::projected_improvement_percent(
            &baseline,
            &overlay_replay,
        ));
    }

    if improvements.is_empty() {
        let reason = if skipped.is_empty() {
            "no traces supplied".to_owned()
        } else {
            format!("no usable traces: {}", skipped.join("; "))
        };
        return PreProbeOutcome {
            kept: true,
            projected_improvement_pct: None,
            note: format!("kept without pre-probe evidence ({reason})"),
        };
    }
    let improvement = median(&improvements);
    PreProbeOutcome {
        kept: improvement >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT,
        projected_improvement_pct: Some(improvement),
        note: format!(
            "projected median input improvement {improvement:+.1}% across {} \
             trace(s){} (gate {ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT}%){}",
            improvements.len(),
            if skipped.is_empty() {
                String::new()
            } else {
                format!("; skipped {}", skipped.len())
            },
            if improvement >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT {
                ""
            } else {
                " — rejected before docker"
            },
        ),
    }
}

// ------------------------------------------------------------ tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(pairs: &[(&str, &str)]) -> Overlay {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn knob_table_covers_every_engine_env_seam() {
        // A seam the engine reads but the table lacks is invisible to
        // the optimizer; the inverse is inert. Both are drift.
        let mut expected: Vec<&str> = loom_core::engine::BUDGET_ENV_VARS.to_vec();
        expected.extend(loom_core::engine::PRUNE_ENV_VARS);
        expected.extend(loom_core::engine::TURN_BUDGET_ENV_VARS);
        let have: Vec<&str> = KNOBS.iter().map(|knob| knob.env).collect();
        assert_eq!(have.len(), expected.len());
        for env in expected {
            assert!(have.contains(&env), "missing knob: {env}");
        }
    }

    #[test]
    fn validate_rejects_unknown_out_of_range_and_non_integer() {
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")])).is_ok());
        assert!(validate_overlay(&overlay(&[("LOOM_NOPE", "1")])).is_err());
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "10")])).is_err());
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "hot")])).is_err());
    }

    #[test]
    fn labels_are_deterministic_and_sorted() {
        let one = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]);
        assert_eq!(overlay_label(&one), "prune_trigger_percent-70");
        let two = overlay(&[
            ("LOOM_READ_HEAD_BYTES", "32768"),
            ("LOOM_PRUNE_TRIGGER_PERCENT", "70"),
        ]);
        assert_eq!(
            overlay_label(&two),
            "prune_trigger_percent-70+read_head_bytes-32768"
        );
        assert_eq!(overlay_label(&Overlay::new()), "baseline");
    }

    #[test]
    fn rule_proposals_halve_and_double_within_bounds() {
        let proposals = rule_proposals(&Overlay::new(), 100);
        assert!(proposals.len() >= KNOBS.len(), "at least one per knob");
        assert!(proposals.len() <= KNOBS.len() * 2);
        for proposal in &proposals {
            assert_eq!(proposal.origin, "rule");
            assert_eq!(proposal.class, ProposalClass::Invariant);
            assert!(validate_overlay(&proposal.overlay).is_ok());
        }
        // Halving the trigger percent must appear (clamped up to the
        // knob floor of 50); doubling clamps to 95.
        let trigger = proposals
            .iter()
            .find(|p| p.overlay.contains_key("LOOM_PRUNE_TRIGGER_PERCENT"))
            .expect("trigger proposal");
        let value: u64 = trigger
            .overlay
            .get("LOOM_PRUNE_TRIGGER_PERCENT")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            value == 50 || value == 95,
            "halve (50, floor-clamped) or clamped double (95), got {value}"
        );
        // The max cap is honored.
        assert!(rule_proposals(&Overlay::new(), 3).len() <= 3);
    }

    #[test]
    fn rule_proposals_respect_an_existing_baseline() {
        let base = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "95")]);
        let proposals = rule_proposals(&base, 100);
        // Doubling 95 clamps back to 95 == current: skipped; only the
        // halve proposal CHANGES that knob (every other proposal
        // merely rides the baseline value along).
        let triggers: Vec<&Proposal> = proposals
            .iter()
            .filter(|p| {
                p.overlay
                    .get("LOOM_PRUNE_TRIGGER_PERCENT")
                    .is_some_and(|value| value != "95")
            })
            .collect();
        assert_eq!(triggers.len(), 1);
        // Baseline keys ride along on every proposal except the one
        // that deliberately moves that knob.
        for proposal in &proposals {
            let value = proposal
                .overlay
                .get("LOOM_PRUNE_TRIGGER_PERCENT")
                .map(String::as_str);
            assert!(
                value == Some("95") || std::ptr::eq(proposal, triggers[0]),
                "baseline rides along, got {value:?}"
            );
        }
    }

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    fn run_stat(instance: &str, run: u64, input: u64, events: u64, failed: bool) -> RunStat {
        RunStat {
            instance: instance.to_owned(),
            run,
            events,
            turns: events / 3,
            usage: RunUsage {
                input_tokens: input,
                cached_input_tokens: None,
                output_tokens: input / 4,
                model_requests: 1,
            },
            task_failed: failed.then(|| "boom".to_owned()),
        }
    }

    #[test]
    fn per_instance_takes_medians_over_runs() {
        let runs = vec![
            run_stat("a", 1, 100, 30, false),
            run_stat("a", 2, 200, 30, false),
            run_stat("a", 3, 10_000, 30, true), // outlier run, median unaffected
            run_stat("b", 1, 50, 20, false),
        ];
        let stats = per_instance(&runs);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].instance, "a");
        assert_eq!(stats[0].median_input_tokens, 200.0);
        assert_eq!(stats[0].failed_runs, 1);
        assert_eq!(stats[1].median_input_tokens, 50.0);
    }

    #[test]
    fn compare_pairs_by_instance_and_decides_on_the_gate() {
        let baseline = vec![
            run_stat("a", 1, 1000, 30, false),
            run_stat("b", 1, 2000, 40, false),
        ];
        // Probe: 20% fewer tokens everywhere, same events.
        let probe = vec![
            run_stat("a", 1, 800, 30, false),
            run_stat("b", 1, 1600, 40, false),
        ];
        let comparison = compare(&baseline, &probe);
        assert!((comparison.median_input_improvement_pct - 20.0).abs() < 1e-9);
        assert_eq!(comparison.baseline_failures, 0);
        let decision = decide("p", &comparison);
        assert!(decision.adopted, "{}", decision.reason);

        // Token win but events regress beyond the gate: rejected, but
        // promoted (input and failure gates passed).
        let flailing = vec![
            run_stat("a", 1, 500, 60, false),
            run_stat("b", 1, 1000, 80, false),
        ];
        let comparison = compare(&baseline, &flailing);
        let decision = decide("p", &comparison);
        assert!(!decision.adopted);
        assert!(decision.promoted, "{}", decision.reason);

        // New failures: rejected regardless of tokens, not promoted.
        let dying = vec![
            run_stat("a", 1, 100, 10, true),
            run_stat("b", 1, 100, 10, true),
        ];
        let comparison = compare(&baseline, &dying);
        let decision = decide("p", &comparison);
        assert!(!decision.adopted);
        assert!(!decision.promoted);
    }

    #[test]
    fn pareto_frontier_drops_dominated_overlays() {
        let decision = |label: &str, input: f64, events: f64, failures: usize| Decision {
            label: label.to_owned(),
            adopted: true,
            input_improvement_pct: input,
            events_improvement_pct: events,
            probe_failures: failures,
            ..Decision::default()
        };
        // b beats a on everything; c trades failures for events.
        let decisions = vec![
            decision("a", 10.0, 5.0, 2),
            decision("b", 20.0, 6.0, 1),
            decision("c", 12.0, 0.0, 0),
            Decision {
                label: "rejected".to_owned(),
                adopted: false,
                input_improvement_pct: 99.0,
                ..Decision::default()
            },
        ];
        let frontier = pareto_frontier(&decisions);
        assert_eq!(frontier, vec!["b".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn order_instances_puts_failures_first_then_cost() {
        let runs = vec![
            run_stat("cheap", 1, 100, 10, false),
            run_stat("hard", 1, 500, 30, true),
            run_stat("mid", 1, 300, 20, false),
            run_stat("mid", 2, 100, 20, false), // runs aggregate
        ];
        assert_eq!(
            order_instances(&runs),
            vec!["hard".to_owned(), "mid".to_owned(), "cheap".to_owned()]
        );
    }

    #[test]
    fn merge_overlays_conflicts_are_named() {
        let a = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]);
        let b = overlay(&[("LOOM_READ_HEAD_BYTES", "32768")]);
        let merged = merge_overlays(&[&a, &b]).expect("disjoint merges");
        assert_eq!(merged.len(), 2);
        let c = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "90")]);
        let error = merge_overlays(&[&a, &c]).expect_err("conflict");
        assert!(error.contains("LOOM_PRUNE_TRIGGER_PERCENT"));
    }

    #[test]
    fn parse_tolerates_fences_and_drops_invalid() {
        let raw = "```json\n[\n{\"overlay\":{\"LOOM_PRUNE_TRIGGER_PERCENT\":\"70\"},\"class\":\"domain\",\"hypothesis\":\"h\",\"rationale\":\"r\"},\n{\"overlay\":{\"LOOM_NOPE\":\"1\"},\"class\":\"domain\",\"hypothesis\":\"h\",\"rationale\":\"r\"}]\n```";
        let (proposals, dropped) = parse_proposal_response(raw, 10);
        assert_eq!(proposals.len(), 1);
        assert_eq!(dropped, 1);
        assert_eq!(proposals[0].class, ProposalClass::Domain);
        assert_eq!(proposals[0].origin, "model");
        assert_eq!(proposals[0].label, "prune_trigger_percent-70");
    }

    #[test]
    fn parse_survives_prose_around_the_array() {
        let raw = "Here you go:\n[{\"overlay\":{\"LOOM_PRUNE_TAIL_TOKENS\":\"8000\"}}]\nGood luck!";
        let (proposals, _) = parse_proposal_response(raw, 10);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].hypothesis, "unstated");
    }

    #[test]
    fn ledger_round_trips_and_missing_is_empty() {
        let dir = std::env::temp_dir().join(format!("loom-selfopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.json");
        assert!(load_ledger(&path).unwrap().is_empty());
        let entry = LedgerEntry {
            timestamp: "2026-10-25T00:00:00".to_owned(),
            sweep: "sweep.json".to_owned(),
            provider: "deepseek".to_owned(),
            model: "deepseek-flash".to_owned(),
            base_overlay: Overlay::new(),
            proposals: Vec::new(),
            decisions: vec![Decision {
                label: "p".to_owned(),
                adopted: true,
                reason: "input +6.0%".to_owned(),
                ..Decision::default()
            }],
            adopted: vec!["p".to_owned()],
            promoted: vec!["q".to_owned()],
            merged: overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]),
            lessons: vec![Lesson {
                statement: "prune earlier on test-loop-heavy tasks".to_owned(),
                evidence: "p: input +6.0%".to_owned(),
            }],
        };
        append_ledger(&path, &entry).unwrap();
        let loaded = load_ledger(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].lessons[0].statement, entry.lessons[0].statement);
        assert_eq!(loaded[0].merged, entry.merged);
        // Old ledgers without `promoted` load as empty.
        let legacy = serde_json::json!([{
            "timestamp": "t", "sweep": "s", "provider": "p", "model": "m",
            "base_overlay": {}, "proposals": [], "decisions": [],
            "adopted": [], "merged": {}, "lessons": []
        }]);
        std::fs::write(dir.join("legacy.json"), legacy.to_string()).unwrap();
        assert!(
            load_ledger(&dir.join("legacy.json")).unwrap()[0]
                .promoted
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_carries_knobs_stats_and_lessons() {
        let baseline = Baseline {
            provider: "deepseek".to_owned(),
            model: "deepseek-flash".to_owned(),
            base_url: None,
            arm: "full".to_owned(),
            runs: Vec::new(),
        };
        let stats = vec![InstanceStat {
            instance: "org/one-1".to_owned(),
            median_input_tokens: 12_345.0,
            median_events: 40.0,
            median_turns: 12.0,
            failed_runs: 0,
            runs: 5,
        }];
        let lessons = vec![Lesson {
            statement: "s".to_owned(),
            evidence: "e".to_owned(),
        }];
        let prompt = build_proposal_prompt(&baseline, &stats, &lessons);
        assert!(prompt.contains("LOOM_PRUNE_TRIGGER_PERCENT"));
        assert!(prompt.contains("org/one-1"));
        assert!(prompt.contains("12"));
        assert!(prompt.contains("\"statement\": \"s\""));
    }

    #[test]
    fn baseline_load_reads_report_rows_and_run_json_turns() {
        let dir = std::env::temp_dir().join(format!("loom-selfopt-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instances = dir.join("instances/org-one-1/run-1");
        std::fs::create_dir_all(&instances).unwrap();
        // turns come from run.json (2 turn_started lines).
        std::fs::write(
            instances.join("run.json"),
            "{\"kind\":{\"type\":\"turn_started\"}}\n{\"kind\":{\"type\":\"turn_started\"}}\n",
        )
        .unwrap();
        let report = serde_json::json!({
            "provider": "deepseek",
            "model": "deepseek-flash",
            "base_url": null,
            "arm": "full",
            "instances": [{
                "instance_id": "org/one-1",
                "runs": [{
                    "run": 1,
                    "events": 7,
                    "task_failed": null,
                    "usage": {"input_tokens": 1000, "output_tokens": 200,
                              "model_requests": 4}
                }]
            }]
        });
        let sweep_path = dir.join("swebench-sweep-test.json");
        std::fs::write(&sweep_path, serde_json::to_string(&report).unwrap()).unwrap();
        let baseline = load_baseline(&sweep_path).unwrap();
        assert_eq!(baseline.provider, "deepseek");
        assert_eq!(baseline.runs.len(), 1);
        assert_eq!(baseline.runs[0].turns, 2);
        assert_eq!(baseline.runs[0].events, 7);
        assert_eq!(baseline.runs[0].usage.input_tokens, 1000);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------- pre-probe ---

    /// Writes a minimal `loom run --trace`-shaped directory: a user
    /// task, `reads` rounds of a read_file call + result of roughly
    /// `read_bytes` bytes each, and a final end turn. No summary, no
    /// events — the pre-probe needs neither.
    fn write_replay_trace(
        dir: &std::path::Path,
        reads: usize,
        read_bytes: usize,
    ) -> std::path::PathBuf {
        use loom_core::kernel::id::{SessionId, TaskId};
        use loom_core::kernel::model::StopReason;
        use loom_core::kernel::session::TaskRecord;
        use loom_core::kernel::transcript::{
            AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
        };

        let mut items = vec![Item::User {
            text: "read big.txt over and over".to_owned(),
        }];
        let body = "lorem ipsum dolor sit amet\n".repeat(read_bytes / 27);
        for index in 0..reads {
            let call = ToolCall {
                id: ToolCallId::try_from(format!("c{index}")).unwrap(),
                name: "read_file".to_owned(),
                arguments: ToolArguments::try_from(serde_json::json!({
                    "path": "big.txt"
                }))
                .unwrap(),
            };
            items.push(Item::Assistant(AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call.clone()],
                stop_reason: StopReason::ToolCalls,
            }));
            items.push(Item::ToolResult(ToolResult::new(
                call.id.clone(),
                ToolStatus::Success,
                body.clone(),
                false,
            )));
        }
        items.push(Item::Assistant(AssistantMessage {
            content: "done".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        }));
        // `turns` must be non-zero or the replay treats the task as a
        // fast-path solve and compiles nothing.
        let mut task = TaskRecord::new(TaskId::new(), 0);
        task.turns = (reads + 1) as u32;
        let manifest = serde_json::json!({
            "harness": {"provider": "deepseek", "model": "m"},
            "task_id": task.id.to_string(),
        });
        let transcript = serde_json::json!({
            "session_id": SessionId::new().to_string(),
            "items": items
                .iter()
                .enumerate()
                .map(|(index, item)| serde_json::json!({"index": index, "item": item}))
                .collect::<Vec<_>>(),
            "tasks": [task],
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.join("transcript.json"), transcript.to_string()).unwrap();
        dir.to_owned()
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-preprobe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pre_probe_keeps_budget_only_overlays_without_pricing() {
        let overlay = overlay(&[("LOOM_READ_HEAD_BYTES", "8192")]);
        let outcome = pre_probe_proposal(&overlay, &[], 16_384, 1_024);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(outcome.note.contains("cannot price"));
        assert_eq!(outcome.projected_improvement_pct, None);
    }

    #[test]
    fn pre_probe_rejects_overlays_the_projection_cannot_pay_for() {
        let root = temp_root("reject");
        let trace = write_replay_trace(&root.join("t1"), 2, 2_000);
        // A floor so high the trigger is unreachable: the overlay arm
        // compiles exactly like the baseline, so the projected
        // improvement is zero and the gate rejects it before docker.
        let floor = overlay(&[("LOOM_PRUNE_FLOOR_TOKENS", "256000")]);
        let outcome = pre_probe_proposal(&floor, std::slice::from_ref(&trace), 16_384, 1_024);
        assert!(!outcome.kept, "{}", outcome.note);
        assert_eq!(outcome.projected_improvement_pct, Some(0.0));
        assert!(outcome.note.contains("rejected before docker"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pre_probe_keeps_an_overlay_that_stubs_more() {
        let root = temp_root("keep");
        // Nine reads of ~9KB under a 32K window: the baseline (provider
        // defaults, 80% trigger) never approaches its trigger, while the
        // aggressive overlay triggers at 30% and — as reads age out of
        // the protected tail turns — freezes and stubs them. Enough
        // turns run AFTER the first freeze that the median (the gate
        // statistic) shifts, not just the totals.
        let trace = write_replay_trace(&root.join("t1"), 9, 9_000);
        let aggressive = overlay(&[
            ("LOOM_PRUNE_TRIGGER_PERCENT", "30"),
            ("LOOM_PRUNE_FLOOR_TOKENS", "100"),
            ("LOOM_PRUNE_MIN_RECLAIM_TOKENS", "100"),
            ("LOOM_PRUNE_TAIL_TOKENS", "200"),
            ("LOOM_AGE_MASK_TURNS", "1"),
        ]);
        let outcome = pre_probe_proposal(&aggressive, std::slice::from_ref(&trace), 32_768, 512);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(
            outcome.projected_improvement_pct.unwrap_or(0.0) >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT,
            "{}",
            outcome.note
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pre_probe_keeps_everything_without_usable_traces() {
        let root = temp_root("unusable");
        let not_a_trace = root.join("empty");
        std::fs::create_dir_all(&not_a_trace).unwrap();
        let floor = overlay(&[("LOOM_PRUNE_FLOOR_TOKENS", "256000")]);
        let outcome = pre_probe_proposal(&floor, std::slice::from_ref(&not_a_trace), 16_384, 1_024);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(outcome.note.contains("without pre-probe evidence"));
        std::fs::remove_dir_all(&root).ok();
    }
}
