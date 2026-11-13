//! Run-artifact reduction: reads a run's JSONL event stream (outcome,
//! turns, token usage, integrity flags) and the official scorer's
//! report (per-instance resolved map).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Reads a run's JSONL event stream and reduces it to what the sweep
/// report needs: the event count and the terminal `task_failed` reason
/// (if any). The stream is line-delimited, so a whole-file
/// `from_str` would always fail; this walks events instead.
#[must_use]
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

/// Scans one run's JSONL trajectory for commands that could have
/// retrieved the fix (or a newer release of the package under test)
/// from outside the testbed — review F-05's enforcement half. The
/// container's git history is already destroyed before the agent
/// starts ([`crate::swebench::CONTAINER_SCRIPT`]); this catches the network routes:
/// `git fetch/clone/pull`, any `github.com` URL (curl/wget/pip from
/// git), and `pip install|download` of anything (the official
/// testbeds ship the dependency set the gold patch assumes, so any
/// install during a run is suspect).
///
/// Reads `tool_started` arguments (precisely: `program`/`args` parse
/// back out of the head-bounded JSON — schema 11 keeps the head, so
/// pathologically long argv can truncate; best effort) and
/// `tool_completed` content heads (command echoes). Returns one
/// human-readable flag per hit; empty means clean.
#[must_use]
pub fn integrity_flags(run_json: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(run_json) else {
        return Vec::new();
    };
    let mut flags = Vec::new();
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = &value["kind"];
        match kind["type"].as_str() {
            Some("tool_started") => {
                let arguments = kind["arguments"].as_str().unwrap_or_default();
                // Prefer the structured fields; fall back to substring
                // sweeps for shell-wrapped invocations (`bash -c …`).
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(arguments) {
                    let program = parsed["program"].as_str().unwrap_or_default();
                    let args: Vec<&str> = parsed["args"].as_array().map_or_default(|entries| {
                        entries.iter().filter_map(|entry| entry.as_str()).collect()
                    });
                    let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
                    let sub = *subcommand.unwrap_or(&"");
                    // Global flags (`-C dir`) shift the subcommand, so
                    // match the verb anywhere in argv — a path literally
                    // named `fetch` false-positives, but the flag is a
                    // human-reviewed pointer, not a verdict.
                    if program == "git"
                        && args
                            .iter()
                            .any(|arg| matches!(*arg, "fetch" | "clone" | "pull"))
                    {
                        flags.push("git fetch/clone/pull during run".to_string());
                    }
                    if matches!(program, "pip" | "pip3") && matches!(sub, "install" | "download") {
                        flags.push("pip install/download during run".to_string());
                    }
                }
                let lower = arguments.to_ascii_lowercase();
                if lower.contains("github.com/") {
                    flags.push("github.com URL in tool arguments".to_string());
                }
            }
            Some("tool_completed") => {
                let head = kind["content_head"].as_str().unwrap_or_default();
                if head.to_ascii_lowercase().contains("github.com/") {
                    flags.push("github.com URL in tool output".to_string());
                }
            }
            _ => {}
        }
    }
    flags.dedup();
    flags
}

/// Model turns in a run's JSONL event stream: the count of
/// `turn_started` events (the terminal `task_completed` event carries
/// no fields, so this is the only turn signal the artifacts hold).
#[must_use]
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
#[must_use]
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
///
/// # Errors
///
/// Returns the `std::io::Error` of [`std::fs::read_to_string`] when the
/// report file cannot be read, and of [`serde_json::from_str`] when the
/// report is not valid JSON.
pub fn scorer_outcomes(report_json: &Path) -> std::io::Result<Vec<(String, bool)>> {
    /// The scorer's id partitions; every other `*_ids` key (e.g.
    /// `unremoved_images`) is bookkeeping, not outcomes.
    fn partition(key: &str) -> Option<bool> {
        match key {
            "resolved_ids" | "resolved_instances" => Some(true),
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
            // `incomplete_*` ids were never submitted — they carry no
            // outcome and stay out of the map entirely, like every other
            // unrecognized `*_ids` key (e.g. bookkeeping
            // `unremoved_images`), so contrasts pair attempted work only.
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
    #[allow(clippy::cast_precision_loss)] // u64 token counts are far below f64's 2^53 mantissa
    pub fn hit_rate(&self) -> Option<f64> {
        self.cached_input_tokens
            .filter(|_| self.input_tokens > 0)
            .map(|cached| cached as f64 / self.input_tokens as f64)
    }
}

/// Reads a run's JSONL event stream and sums every model response's
/// usage into [`RunUsage`] (the per-request split providers report on
/// the authoritative `model_completed` event).
#[must_use]
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

// ------------------------------------------------------------ tests ---

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
fn integrity_flags_catch_retrieval_routes() {
    let dir = std::env::temp_dir().join(format!("loom-swe-integrity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.json");

    // Missing file: never started, nothing to flag.
    assert_eq!(integrity_flags(&path), [] as [std::string::String; 0]);

    // Clean trajectory: reads, tests, plain git status — no flags.
    std::fs::write(
        &path,
        "{\"kind\":{\"type\":\"tool_started\",\"name\":\"run_command\",\"arguments\":\"{\\\"program\\\":\\\"git\\\",\\\"args\\\":[\\\"status\\\"]}\"}}\n\
         {\"kind\":{\"type\":\"tool_started\",\"name\":\"run_command\",\"arguments\":\"{\\\"program\\\":\\\"python\\\",\\\"args\\\":[\\\"-m\\\",\\\"pytest\\\",\\\"-q\\\"]}\"}}\n",
    )
    .unwrap();
    assert!(integrity_flags(&path).is_empty(), "clean run flagged");

    // Every retrieval route F-05 names, one per line: git fetch
    // (including flag-prefixed argv), pip download, github.com in
    // arguments (curl) and in tool output (content_head).
    std::fs::write(
        &path,
        "{\"kind\":{\"type\":\"tool_started\",\"arguments\":\"{\\\"program\\\":\\\"git\\\",\\\"args\\\":[\\\"fetch\\\",\\\"origin\\\"]}\"}}\n\
         {\"kind\":{\"type\":\"tool_started\",\"arguments\":\"{\\\"program\\\":\\\"git\\\",\\\"args\\\":[\\\"-C\\\",\\\"/testbed\\\",\\\"clone\\\"]}\"}}\n\
         {\"kind\":{\"type\":\"tool_started\",\"arguments\":\"{\\\"program\\\":\\\"pip3\\\",\\\"args\\\":[\\\"install\\\",\\\"--upgrade\\\",\\\"sympy\\\"]}\"}}\n\
         {\"kind\":{\"type\":\"tool_started\",\"arguments\":\"{\\\"program\\\":\\\"curl\\\",\\\"args\\\":[\\\"-s\\\",\\\"https://github.com/django/django/commit/abc.patch\\\"]}\"}}\n\
         {\"kind\":{\"type\":\"tool_completed\",\"content_head\":\"Cloning into 'django' from github.com/django/django\\n\"}}\n",
    )
    .unwrap();
    let flags = integrity_flags(&path);
    assert!(
        flags
            .iter()
            .any(|flag| flag.contains("git fetch/clone/pull")),
        "{flags:?}"
    );
    assert!(
        flags
            .iter()
            .any(|flag| flag.contains("pip install/download")),
        "{flags:?}"
    );
    assert_eq!(
        flags
            .iter()
            .filter(|flag| flag.contains("github.com"))
            .count(),
        2,
        "{flags:?}"
    );

    // pip list / pip show stay legal: the review's own scans hit them
    // as benign version checks.
    std::fs::write(
        &path,
        "{\"kind\":{\"type\":\"tool_started\",\"arguments\":\"{\\\"program\\\":\\\"pip\\\",\\\"args\\\":[\\\"list\\\"]}\"}}\n",
    )
    .unwrap();
    assert_eq!(integrity_flags(&path), [] as [std::string::String; 0]);
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
