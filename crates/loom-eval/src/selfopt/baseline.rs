//! Baseline loading: the sweep report plus per-run `run.json`
//! artifacts, reduced to per-run statistics.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::swebench::{self, RunUsage};

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
