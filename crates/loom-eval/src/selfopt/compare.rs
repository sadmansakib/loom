//! Paired comparison and the adoption gate: per-instance aggregates,
//! baseline-versus-probe deltas, the adopt/promote verdict, the Pareto
//! frontier, and hard-first probe ordering.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::baseline::RunStat;

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
#[must_use]
#[allow(clippy::cast_precision_loss)] // median aggregation works in f64; counters are far below 2^53
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
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        sorted[middle - 1].midpoint(sorted[middle])
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
#[must_use]
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
#[must_use]
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
/// improvement, events improvement, probe failures) — `HarnessPro`'s
/// cost tiers: each frontier entry is a defensible operating point,
/// no other adopted overlay beats it on all three. Input order.
#[must_use]
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
#[must_use]
pub fn order_instances(runs: &[RunStat]) -> Vec<String> {
    #[derive(Default)]
    struct Agg {
        failed: bool,
        input_tokens: u64,
    }
    let mut agg: std::collections::BTreeMap<String, Agg> = BTreeMap::default();
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
