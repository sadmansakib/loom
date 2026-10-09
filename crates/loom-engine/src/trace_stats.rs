//! Attribution over exported traces (roadmap: item 20.1, phase 3 —
//! reading what the [`crate::trace`] export wrote).
//!
//! Each task directory under a trace root carries `manifest.json`
//! (outcome, harness, billed usage summary) and `events.ndjson` (the
//! event stream, including `context_pruned` reports). This module walks
//! those directories and aggregates them: per-task outcomes and usage,
//! plus the prune attribution the transcript store cannot offer.
//! Reclaimed tokens are estimator-side estimates (they were never
//! billed); the usage fields are the provider's own counts.
//!
//! The module is pure aggregation — typed rows in, typed totals and
//! JSON-value report blocks out, no terminal I/O. Frontends decide
//! what to print; the CLI's rendering lives in its stats command.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::trace::{EVENTS_FILE_NAME, MANIFEST_FILE_NAME};

/// Calls and reclaimed tokens attributed to one trigger or reason key.
#[derive(Debug, Default)]
pub struct PruneAgg {
    /// Prune events carrying this key.
    pub events: u64,
    /// Tool calls pruned under this key.
    pub calls: u64,
    /// Estimator-side reclaimed tokens.
    pub reclaimed_tokens: u64,
}

/// Per-trigger and per-reason prune attribution for one task.
#[derive(Debug, Default)]
pub struct PruneStats {
    /// Attribution keyed by prune trigger (`window`, `age_mask`, …).
    pub by_trigger: BTreeMap<String, PruneAgg>,
    /// Attribution keyed by per-call reason (`TurnAge`, …).
    pub by_reason: BTreeMap<String, PruneAgg>,
    /// `events.ndjson` lines that did not parse as JSON.
    pub malformed_lines: u64,
}

/// One task directory: manifest distilled plus its prune stats.
#[derive(Debug)]
pub struct TaskRow {
    /// Directory name under the trace root.
    pub dir_name: String,
    /// True when the manifest exists but will not parse.
    pub corrupt: bool,
    /// Task id from the manifest, when present.
    pub task_id: Option<String>,
    /// Provider name from the manifest, when present.
    pub provider: Option<String>,
    /// Model id from the manifest, when present.
    pub model: Option<String>,
    /// Outcome status (`completed`, `cancelled`, `failed`), when present.
    pub status: Option<String>,
    /// Stable snake_case end-reason token, when present.
    pub end_reason: Option<String>,
    /// The manifest's billed-usage summary block, when present.
    pub summary: Option<Value>,
    /// Events the writer appended to `events.ndjson`.
    pub events_written: u64,
    /// Appends the writer counted as failed.
    pub events_write_failures: u64,
    /// Prune attribution from the event stream.
    pub prunes: PruneStats,
}

impl TaskRow {
    /// The placeholder row for a directory whose manifest will not parse.
    fn corrupt() -> Self {
        Self {
            dir_name: String::new(),
            corrupt: true,
            task_id: None,
            provider: None,
            model: None,
            status: None,
            end_reason: None,
            summary: None,
            events_written: 0,
            events_write_failures: 0,
            prunes: PruneStats::default(),
        }
    }
}

/// Totals across every task directory of one trace root.
#[derive(Debug, Default)]
pub struct Totals {
    /// Task directories seen, corrupt ones included.
    pub tasks: usize,
    /// Directories whose manifest would not parse.
    pub corrupt: usize,
    /// Tasks per outcome status.
    pub by_status: BTreeMap<String, usize>,
    /// Billed turns, summed over manifests carrying a summary.
    pub turns: u64,
    /// Billed model requests.
    pub model_requests: u64,
    /// Billed input tokens.
    pub input_tokens: u64,
    /// Billed output tokens.
    pub output_tokens: u64,
    /// Billed cached input tokens.
    pub cached_input_tokens: u64,
    /// Billed thinking tokens.
    pub thinking_tokens: u64,
    /// Billed cache-creation tokens.
    pub cache_creation_input_tokens: u64,
    /// Billed wall time, milliseconds.
    pub wall_time_millis: u64,
    /// Manifests that carried a summary block.
    pub summaries_present: usize,
    /// Events appended across all tasks.
    pub events_written: u64,
    /// Failed event appends across all tasks.
    pub events_write_failures: u64,
    /// Malformed `events.ndjson` lines across all tasks.
    pub malformed_event_lines: u64,
    /// Prune attribution keyed by trigger.
    pub by_trigger: BTreeMap<String, PruneAgg>,
    /// Prune attribution keyed by per-call reason.
    pub by_reason: BTreeMap<String, PruneAgg>,
}

/// Reads every task directory under `base`.
///
/// A directory without a readable `manifest.json` is not a trace and
/// is skipped; one whose manifest will not parse yields a corrupt row.
/// Fails only when `base` itself cannot be listed.
pub fn collect(base: &Path) -> io::Result<Vec<TaskRow>> {
    let mut rows = Vec::new();
    let entries = std::fs::read_dir(base).map_err(|error| {
        io::Error::new(error.kind(), format!("reading {}: {error}", base.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            io::Error::new(error.kind(), format!("listing {}: {error}", base.display()))
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest = match std::fs::read_to_string(path.join(MANIFEST_FILE_NAME)) {
            Ok(text) => text,
            Err(_) => continue, // not a trace directory
        };
        let manifest: Value = match serde_json::from_str(&manifest) {
            Ok(value) => value,
            Err(_) => {
                let mut row = TaskRow::corrupt();
                row.dir_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                rows.push(row);
                continue;
            }
        };
        let prunes = prune_stats(&path);
        rows.push(TaskRow {
            dir_name: entry.file_name().to_string_lossy().into_owned(),
            corrupt: false,
            task_id: manifest["task_id"].as_str().map(str::to_owned),
            provider: manifest["harness"]["provider"].as_str().map(str::to_owned),
            model: manifest["harness"]["model"].as_str().map(str::to_owned),
            status: manifest["outcome"]["status"].as_str().map(str::to_owned),
            end_reason: manifest["outcome"]["end_reason"]
                .as_str()
                .map(str::to_owned),
            summary: manifest["summary"].as_object().cloned().map(Value::Object),
            events_written: manifest["events_written"].as_u64().unwrap_or(0),
            events_write_failures: manifest["events_write_failures"].as_u64().unwrap_or(0),
            prunes,
        });
    }
    rows.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    Ok(rows)
}

/// Extracts prune attribution from one task's `events.ndjson`.
fn prune_stats(task_dir: &Path) -> PruneStats {
    let mut stats = PruneStats::default();
    let Ok(text) = std::fs::read_to_string(task_dir.join(EVENTS_FILE_NAME)) else {
        return stats;
    };
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            stats.malformed_lines += 1;
            continue;
        };
        if event["type"] != "context_pruned" {
            continue;
        }
        let report = &event["report"];
        let trigger = report["trigger"].as_str().unwrap_or("unknown").to_owned();
        let agg = stats.by_trigger.entry(trigger.clone()).or_default();
        agg.events += 1;
        if let Some(calls) = report["calls"].as_array() {
            for call in calls {
                agg.calls += 1;
                let reclaimed = call["reclaimed_tokens"].as_u64().unwrap_or(0);
                agg.reclaimed_tokens += reclaimed;
                let reason = call["reason"].as_str().unwrap_or("unknown").to_owned();
                let reason_agg = stats.by_reason.entry(reason).or_default();
                reason_agg.calls += 1;
                reason_agg.reclaimed_tokens += reclaimed;
            }
        }
        // The report's own total outranks the per-call sum when present.
        if let Some(total) = report["reclaimed_tokens"].as_u64() {
            agg.reclaimed_tokens = agg.reclaimed_tokens.max(total);
        }
    }
    stats
}

/// Sums every row into one [`Totals`] value.
pub fn fold_totals(rows: &[TaskRow]) -> Totals {
    let mut totals = Totals::default();
    for row in rows {
        totals.tasks += 1;
        if row.corrupt {
            totals.corrupt += 1;
            continue;
        }
        if let Some(status) = &row.status {
            *totals.by_status.entry(status.clone()).or_default() += 1;
        }
        if let Some(summary) = &row.summary {
            totals.summaries_present += 1;
            totals.turns += u64_of(summary, "turns");
            totals.model_requests += u64_of(summary, "model_requests");
            totals.input_tokens += u64_of(summary, "input_tokens");
            totals.output_tokens += u64_of(summary, "output_tokens");
            totals.cached_input_tokens += u64_of(summary, "cached_input_tokens");
            totals.thinking_tokens += u64_of(summary, "thinking_tokens");
            totals.cache_creation_input_tokens += u64_of(summary, "cache_creation_input_tokens");
            totals.wall_time_millis += u64_of(summary, "wall_time_millis");
        }
        totals.events_written += row.events_written;
        totals.events_write_failures += row.events_write_failures;
        totals.malformed_event_lines += row.prunes.malformed_lines;
        merge_aggmaps(&mut totals.by_trigger, &row.prunes.by_trigger);
        merge_aggmaps(&mut totals.by_reason, &row.prunes.by_reason);
    }
    totals
}

/// Reads a u64 field from a JSON object, defaulting to 0.
fn u64_of(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

/// Adds `from`'s counters into `into` (summing per-call reclaims only;
/// the event maximum is per-task and does not aggregate).
fn merge_aggmaps(into: &mut BTreeMap<String, PruneAgg>, from: &BTreeMap<String, PruneAgg>) {
    for (key, agg) in from {
        let slot = into.entry(key.clone()).or_default();
        slot.events += agg.events;
        slot.calls += agg.calls;
        slot.reclaimed_tokens += agg.reclaimed_tokens;
    }
}

/// The JSON block for one task.
pub fn task_json(row: &TaskRow) -> Value {
    serde_json::json!({
        "dir": row.dir_name,
        "corrupt": row.corrupt,
        "task_id": row.task_id,
        "provider": row.provider,
        "model": row.model,
        "status": row.status,
        "end_reason": row.end_reason,
        "events_written": row.events_written,
        "events_write_failures": row.events_write_failures,
        "summary": row.summary,
        "prunes": {
            "by_trigger": row.prunes.by_trigger.iter()
                .map(|(k, v)| (k.clone(), agg_json(v)))
                .collect::<serde_json::Map<String, Value>>(),
            "by_reason": row.prunes.by_reason.iter()
                .map(|(k, v)| (k.clone(), agg_json(v)))
                .collect::<serde_json::Map<String, Value>>(),
            "malformed_lines": row.prunes.malformed_lines,
        },
    })
}

/// One aggregate block.
fn agg_json(agg: &PruneAgg) -> Value {
    serde_json::json!({
        "events": agg.events,
        "calls": agg.calls,
        "reclaimed_tokens": agg.reclaimed_tokens,
    })
}

/// The JSON totals block.
pub fn totals_json(totals: &Totals) -> Value {
    serde_json::json!({
        "tasks": totals.tasks,
        "corrupt": totals.corrupt,
        "by_status": totals.by_status,
        "turns": totals.turns,
        "model_requests": totals.model_requests,
        "input_tokens": totals.input_tokens,
        "output_tokens": totals.output_tokens,
        "cached_input_tokens": totals.cached_input_tokens,
        "thinking_tokens": totals.thinking_tokens,
        "cache_creation_input_tokens": totals.cache_creation_input_tokens,
        "wall_time_millis": totals.wall_time_millis,
        "summaries_present": totals.summaries_present,
        "events_written": totals.events_written,
        "events_write_failures": totals.events_write_failures,
        "malformed_event_lines": totals.malformed_event_lines,
        "prunes_by_trigger": totals.by_trigger.iter()
            .map(|(k, v)| (k.clone(), agg_json(v)))
            .collect::<serde_json::Map<String, Value>>(),
        "prunes_by_reason": totals.by_reason.iter()
            .map(|(k, v)| (k.clone(), agg_json(v)))
            .collect::<serde_json::Map<String, Value>>(),
    })
}

/// One human-readable task line.
pub fn human_task(row: &TaskRow) -> String {
    let label = row.task_id.clone().unwrap_or_else(|| row.dir_name.clone());
    let harness = match (&row.provider, &row.model) {
        (Some(provider), Some(model)) => format!("{model} via {provider}"),
        _ => "unknown harness".to_owned(),
    };
    let outcome = match (&row.status, &row.end_reason) {
        (Some(status), Some(reason)) => format!("{status} ({reason})"),
        (Some(status), None) => status.clone(),
        _ => "unknown outcome".to_owned(),
    };
    match &row.summary {
        Some(summary) => format!(
            "{label}  {harness}, {outcome}, {} turns, {} in / {} out / {} cached, {} events",
            u64_of(summary, "turns"),
            u64_of(summary, "input_tokens"),
            u64_of(summary, "output_tokens"),
            u64_of(summary, "cached_input_tokens"),
            row.events_written,
        ),
        None => format!(
            "{label}  {harness}, {outcome}, no summary, {} events",
            row.events_written,
        ),
    }
}

/// One human-readable totals line.
pub fn human_totals(totals: &Totals) -> String {
    let statuses: Vec<String> = totals
        .by_status
        .iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect();
    let statuses = if statuses.is_empty() {
        "no outcomes".to_owned()
    } else {
        statuses.join(", ")
    };
    format!(
        "{} tasks ({}), {} turns, {} in / {} out / {} cached / {} thinking, {} events ({} malformed lines, {} write failures)",
        totals.tasks,
        statuses,
        totals.turns,
        totals.input_tokens,
        totals.output_tokens,
        totals.cached_input_tokens,
        totals.thinking_tokens,
        totals.events_written,
        totals.malformed_event_lines,
        totals.events_write_failures,
    )
}

/// The human attribution summary: triggers first, then reasons.
pub fn attribution_line(totals: &Totals) -> String {
    let mut parts: Vec<String> = totals
        .by_trigger
        .iter()
        .map(|(trigger, agg)| {
            format!(
                "{trigger}: {} events, {} calls, ~{} tok",
                agg.events, agg.calls, agg.reclaimed_tokens
            )
        })
        .collect();
    parts.extend(totals.by_reason.iter().map(|(reason, agg)| {
        format!(
            "{reason}: {} calls, ~{} tok",
            agg.calls, agg.reclaimed_tokens
        )
    }));
    if parts.is_empty() {
        "no prune events".to_owned()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temp_base(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-trace-stats-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_task(base: &Path, dir: &str, manifest: serde_json::Value, events: &str) {
        let task = base.join(dir);
        fs::create_dir_all(&task).unwrap();
        fs::write(
            task.join(MANIFEST_FILE_NAME),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(task.join(EVENTS_FILE_NAME), events).unwrap();
    }

    fn completed_manifest() -> serde_json::Value {
        json!({
            "loom_version": "0.1.0",
            "event_schema_version": 3,
            "exported_at_millis": 0,
            "session_id": "s1",
            "task_id": "task-a",
            "harness": {
                "provider": "deepseek",
                "model": "deepseek-v4-pro",
                "reasoning_effort": "high",
                "max_turns": 40,
                "persisted": true,
            },
            "outcome": {"status": "completed", "end_reason": "end_turn"},
            "events_written": 5,
            "events_write_failures": 1,
            "summary": {
                "turns": 3,
                "model_requests": 3,
                "input_tokens": 100,
                "output_tokens": 50,
                "cached_input_tokens": 40,
                "thinking_tokens": 20,
                "cache_creation_input_tokens": 10,
                "wall_time_millis": 1000,
                "events_dropped": 0,
            },
        })
    }

    fn cancelled_manifest() -> serde_json::Value {
        json!({
            "loom_version": "0.1.0",
            "event_schema_version": 3,
            "exported_at_millis": 0,
            "session_id": "s2",
            "task_id": "task-b",
            "harness": {
                "provider": "nvidia",
                "model": "z-ai/glm-5.3",
                "reasoning_effort": "high",
                "max_turns": 40,
                "persisted": false,
            },
            "outcome": {"status": "cancelled", "end_reason": "user_cancelled"},
            "events_written": 2,
            "events_write_failures": 0,
        })
    }

    const TASK_A_EVENTS: &str = concat!(
        r#"{"schema_version":3,"sequence":0,"task_id":"task-a","type":"task_started"}"#,
        "\n",
        r#"{"schema_version":3,"sequence":1,"task_id":"task-a","type":"context_pruned","report":{"trigger":"age_mask","calls":[{"call_id":"c1","reason":"TurnAge","reclaimed_tokens":120},{"call_id":"c2","reason":"TurnAge","reclaimed_tokens":80}],"reclaimed_tokens":200}}"#,
        "\n",
        "not json at all\n",
        r#"{"schema_version":3,"sequence":2,"task_id":"task-a","type":"text_delta","text":"hi"}"#,
        "\n",
    );

    const TASK_B_EVENTS: &str = concat!(
        r#"{"schema_version":3,"sequence":0,"task_id":"task-b","type":"context_pruned","report":{"trigger":"window","calls":[{"call_id":"c1","reason":"SizeLimit","reclaimed_tokens":300}],"reclaimed_tokens":300}}"#,
        "\n",
    );

    fn fixture(name: &str) -> PathBuf {
        let base = temp_base(name);
        write_task(&base, "task-a", completed_manifest(), TASK_A_EVENTS);
        write_task(&base, "task-b", cancelled_manifest(), TASK_B_EVENTS);
        // A directory without a manifest is not a trace and is skipped.
        fs::create_dir_all(base.join("not-a-trace")).unwrap();
        // A manifest that will not parse counts as corrupt.
        let corrupt = base.join("corrupt-one");
        fs::create_dir_all(&corrupt).unwrap();
        fs::write(corrupt.join(MANIFEST_FILE_NAME), "{ not json").unwrap();
        base
    }

    #[test]
    fn aggregates_outcomes_usage_and_prune_attribution() {
        let base = fixture("aggregate");
        let rows = collect(&base).unwrap();
        assert_eq!(rows.len(), 3, "non-trace dirs skipped");
        assert!(
            rows.iter()
                .all(|row| !row.corrupt || row.dir_name == "corrupt-one")
        );

        let totals = fold_totals(&rows);
        assert_eq!(totals.tasks, 3);
        assert_eq!(totals.corrupt, 1);
        assert_eq!(totals.by_status.get("completed"), Some(&1));
        assert_eq!(totals.by_status.get("cancelled"), Some(&1));
        assert_eq!(totals.summaries_present, 1);
        assert_eq!(totals.turns, 3);
        assert_eq!(totals.input_tokens, 100);
        assert_eq!(totals.output_tokens, 50);
        assert_eq!(totals.cached_input_tokens, 40);
        assert_eq!(totals.thinking_tokens, 20);
        assert_eq!(totals.cache_creation_input_tokens, 10);
        assert_eq!(totals.wall_time_millis, 1000);
        assert_eq!(totals.events_written, 7);
        assert_eq!(totals.events_write_failures, 1);
        assert_eq!(totals.malformed_event_lines, 1);

        let age_mask = &totals.by_trigger["age_mask"];
        assert_eq!(age_mask.events, 1);
        assert_eq!(age_mask.calls, 2);
        assert_eq!(age_mask.reclaimed_tokens, 200);
        let window = &totals.by_trigger["window"];
        assert_eq!(window.reclaimed_tokens, 300);
        let turn_age = &totals.by_reason["TurnAge"];
        assert_eq!(turn_age.calls, 2);
        assert_eq!(turn_age.reclaimed_tokens, 200);
        let size = &totals.by_reason["SizeLimit"];
        assert_eq!(size.reclaimed_tokens, 300);

        let report = totals_json(&totals);
        assert_eq!(report["tasks"], 3);
        assert_eq!(report["corrupt"], 1);
        assert_eq!(report["prunes_by_trigger"]["age_mask"]["calls"], 2);
        assert_eq!(
            report["prunes_by_reason"]["TurnAge"]["reclaimed_tokens"],
            200
        );

        let task_a = rows
            .iter()
            .find(|row| row.dir_name == "task-a")
            .expect("task-a present");
        let body = task_json(task_a);
        assert_eq!(body["task_id"], "task-a");
        assert_eq!(body["corrupt"], false);
        assert_eq!(body["prunes"]["by_trigger"]["age_mask"]["events"], 1);
        assert_eq!(body["prunes"]["malformed_lines"], 1);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rows_sort_by_directory_name() {
        let base = fixture("sort");
        let rows = collect(&base).unwrap();
        let names: Vec<_> = rows.iter().map(|row| row.dir_name.as_str()).collect();
        assert_eq!(names, ["corrupt-one", "task-a", "task-b"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_base_directory_is_an_error() {
        let base = temp_base("missing");
        let _ = fs::remove_dir_all(&base);
        assert!(collect(&base).is_err());
    }

    #[test]
    fn human_lines_mention_the_estimates() {
        let base = fixture("human");
        let rows = collect(&base).unwrap();
        let totals = fold_totals(&rows);
        assert!(attribution_line(&totals).contains("age_mask"));
        assert!(human_totals(&totals).contains("malformed lines"));
        let task_a = rows.iter().find(|row| row.dir_name == "task-a").unwrap();
        assert!(human_task(task_a).contains("deepseek-v4-pro via deepseek"));
        assert!(human_task(task_a).contains("completed (end_turn)"));
        let _ = fs::remove_dir_all(&base);
    }
}
