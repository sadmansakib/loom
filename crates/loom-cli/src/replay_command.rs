//! `loom replay-context`: offline counterfactual context replay
//! (`docs/replay-context-design.md`).
//!
//! Re-compiles a recorded session — from a `loom run --trace` directory
//! or straight out of the session store — through the real
//! [`PruningContextCompiler`] at every turn boundary, then again under
//! a `--set` overlay of prune knobs. Zero model calls: the projection
//! is a pure function of the transcript and the config, so token and
//! cache-prefix costs of a knob change are measured, not guessed.
//!
//! What it cannot do is printed on every run: the transcript is fixed,
//! so replay says nothing about task success. The validity check closes
//! the other direction — replaying a trace under its own baseline must
//! reproduce the recorded census totals and prune events, or the tool
//! says so and declares itself untrustworthy for that input.
//!
//! Like `loom sessions`/`loom rewind`, the store path reads SQLite
//! directly: no engine open, no provider, no credentials.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::engine::replay::{self, ReplayOptions, ReplaySummary, TraceInput, ValidityReport};
use loom_core::kernel::context::PruneReport;
use loom_core::kernel::event::EventKind;
use loom_core::kernel::id::SessionId;
use loom_core::kernel::session::{Session, TaskRecord};
use loom_core::kernel::store::{EventStore, SessionStore};

use crate::args::ReplayContextArgs;

/// Printed on every run, human and JSON alike: the one-line honesty
/// contract for this tool.
const LIMITATION: &str = "replay measures projected token and cache-prefix cost only — \
                          the transcript is fixed, so it says NOTHING about task success";

/// Where the input came from; the two kinds carry different ground
/// truth (traces record a census, the store does not).
#[derive(Debug)]
enum Source {
    /// A `loom run --trace` task directory.
    Trace(std::path::PathBuf),
    /// A session id resolved through `loom.db`.
    Session(SessionId),
}

/// Runs `loom replay-context`.
pub fn run(args: ReplayContextArgs) -> anyhow::Result<ExitCode> {
    let source = resolve_source(&args.source)?;
    let mut trace = match &source {
        Source::Trace(dir) => TraceInput::read(dir).map_err(anyhow::Error::msg)?,
        Source::Session(id) => session_trace(id, args.provider.as_deref())?,
    };
    // `--provider` overrides the manifest's provider (gateway runs whose
    // manifest provider is not the estimator family the operator wants).
    if let Some(provider) = &args.provider {
        trace.provider = provider.clone();
    }

    let mut options = ReplayOptions::new(&trace.provider);
    if let Some(window) = args.window {
        options.window_tokens = window;
    }
    if let Some(reserved) = args.reserved_output {
        options.reserved_output_tokens = reserved;
    }
    if let Some(prompt) = &args.system_prompt {
        options.system_prompt = prompt.clone();
    }

    // Baseline: the provider's base config, exactly as a live run with
    // no LOOM_PRUNE_* environment would compose it.
    let (baseline_config, baseline_warnings) =
        loom_core::engine::engine::prune_with_env(&trace.provider, |_| None);
    for warning in &baseline_warnings {
        eprintln!("warning: {warning}");
    }

    let baseline = replay::replay_session(&trace.session, &trace.task, &options, baseline_config)
        .map_err(anyhow::Error::msg)
        .context("replaying the baseline arm")?;
    let validity = ValidityReport::check(&trace, &baseline);

    let overlay = if args.set.is_empty() {
        None
    } else {
        let (config, applied) =
            replay::overlay_config(baseline_config, &args.set).map_err(anyhow::Error::msg)?;
        let summary = replay::replay_session(&trace.session, &trace.task, &options, config)
            .map_err(anyhow::Error::msg)
            .context("replaying the overlay arm")?;
        Some((applied, summary))
    };

    let source_desc = match &source {
        Source::Trace(dir) => format!("{} (trace dir)", dir.display()),
        Source::Session(id) => format!("{id} (stored session)"),
    };
    if args.json {
        print_json(
            &source_desc,
            &trace,
            &baseline,
            validity_valid(&validity),
            overlay
                .as_ref()
                .map(|(applied, summary)| (applied.as_slice(), summary)),
            &validity,
        );
    } else {
        print_human(
            &source_desc,
            &trace,
            &baseline,
            overlay
                .as_ref()
                .map(|(applied, summary)| (applied.as_slice(), summary)),
            &validity,
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `--source` is a trace directory when it holds `manifest.json`,
/// else it must parse as a stored session id.
fn resolve_source(raw: &str) -> anyhow::Result<Source> {
    let path = std::path::Path::new(raw);
    if path.is_dir() && path.join("manifest.json").is_file() {
        return Ok(Source::Trace(path.to_owned()));
    }
    let id: SessionId = raw.parse().with_context(|| {
        format!(
            "{raw:?} is neither a trace directory (no manifest.json beside it) \
             nor a session id from `loom sessions`"
        )
    })?;
    Ok(Source::Session(id))
}

/// Builds replay input from the store. Sessions do not record their
/// provider, so `--provider` is required on this path; prune events
/// come from the persisted event log when present.
fn session_trace(id: &SessionId, provider: Option<&str>) -> anyhow::Result<TraceInput> {
    let provider = provider.map(str::to_owned).with_context(|| {
        "replaying a stored session needs --provider: sessions do not record \
         which provider ran them (traces do)"
    })?;
    let store = loom_core::store::SqliteStore::open(loom_core::paths::db_path())
        .context("opening the session store")?;
    let session: Session = store
        .load(id)
        .context("loading the session")?
        .with_context(|| format!("unknown session {id} (see `loom sessions`)"))?;
    let task: TaskRecord = session
        .tasks
        .last()
        .cloned()
        .context("session has no task record to replay")?;
    // Schema skew detection for store input: the envelopes carry the
    // schema of the binary that wrote them (no events → nothing to
    // compare, so the current version is a harmless stand-in).
    let events = store.load_events(id).unwrap_or_default();
    let schema_version = events
        .iter()
        .map(|envelope| envelope.schema_version)
        .max()
        .unwrap_or(loom_core::kernel::event::EVENT_SCHEMA_VERSION);
    let prune_events: Vec<(u32, PruneReport)> = events
        .into_iter()
        .filter_map(|envelope| match envelope.kind {
            EventKind::ContextPruned { turn, report } => Some((turn, report)),
            _ => None,
        })
        .collect();
    Ok(TraceInput {
        session,
        task,
        provider,
        model: String::new(),
        schema_version,
        summary: None,
        prune_events,
    })
}

/// The overall verdict for JSON output (the report carries the detail).
fn validity_valid(report: &ValidityReport) -> bool {
    report.valid
}

/// Human output: header, per-turn tables, paired deltas, validity.
fn print_human(
    source_desc: &str,
    trace: &TraceInput,
    baseline: &ReplaySummary,
    overlay: Option<(&[String], &ReplaySummary)>,
    validity: &ValidityReport,
) {
    let model = if trace.model.is_empty() {
        "-"
    } else {
        &trace.model
    };
    println!("replay: {source_desc}");
    println!("provider: {}  model: {model}", trace.provider);
    println!("LIMITATION: {LIMITATION}");
    println!();
    println!("arm baseline (provider base config):");
    print_arm(baseline);
    if let Some((applied, summary)) = overlay {
        println!();
        println!("arm overlay ({}):", applied.join(", "));
        print_arm(summary);
        println!();
        print_paired(baseline, summary);
    }
    println!();
    print_validity(validity);
    if let Some(turn) = baseline.handoff_stopped_at {
        println!(
            "handoff: baseline arm stopped at turn {turn} — the live loop would \
             spend one model call on an LLM handoff, which replay cannot synthesize"
        );
    }
    if let Some((_, summary)) = overlay
        && let Some(turn) = summary.handoff_stopped_at
    {
        println!(
            "handoff: overlay arm stopped at turn {turn} — the live loop would \
             spend one model call on an LLM handoff, which replay cannot synthesize"
        );
    }
}

/// One arm's per-turn table, swebench-contrast style: fixed columns,
/// per-turn rows, deltas read off the paired table instead.
fn print_arm(summary: &ReplaySummary) {
    println!(
        "  {:>4}  {:>8}  {:>6}  {:>6}  {:>6}  {:>7}  PREFIX-STAB",
        "TURN", "INPUT", "USER", "ASST", "TOOL", "FROZE"
    );
    for turn in &summary.turns {
        let froze = turn.froze.as_ref().map_or_else(
            || "-".to_owned(),
            |report| format!("{} tok", report.reclaimed_tokens),
        );
        let stability = turn
            .prefix_stability
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.4}"));
        println!(
            "  {:>4}  {:>8}  {:>6}  {:>6}  {:>6}  {:>7}  {stability}",
            turn.turn,
            turn.input_tokens,
            turn.census.user_items,
            turn.census.assistant_history,
            turn.census.tool_results,
            froze,
        );
    }
    println!(
        "  totals: {} turn(s), {} input tok, {} reclaimed, {} freeze(s), \
         median input {:.0} tok",
        summary.turns.len(),
        summary.total_input_tokens,
        summary.reclaimed_tokens,
        summary.freezes,
        summary.median_input_tokens,
    );
}

/// Paired baseline-vs-overlay deltas.
fn print_paired(baseline: &ReplaySummary, overlay: &ReplaySummary) {
    println!("paired deltas (overlay vs baseline; input down is better):");
    println!(
        "  median input tokens:  {:>8.0} -> {:>8.0}  ({:+.1}%)",
        baseline.median_input_tokens,
        overlay.median_input_tokens,
        delta_pct(baseline.median_input_tokens, overlay.median_input_tokens),
    );
    println!(
        "  total input tokens:   {:>8} -> {:>8}  ({:+.1}%)",
        baseline.total_input_tokens,
        overlay.total_input_tokens,
        delta_pct(
            baseline.total_input_tokens as f64,
            overlay.total_input_tokens as f64
        ),
    );
    println!(
        "  reclaimed tokens:     {:>8} -> {:>8}",
        baseline.reclaimed_tokens, overlay.reclaimed_tokens,
    );
    println!(
        "  freezes:              {:>8} -> {:>8}",
        baseline.freezes, overlay.freezes,
    );
    match (baseline.min_prefix_stability, overlay.min_prefix_stability) {
        (Some(base), Some(over)) => {
            println!("  min prefix stability:  {base:>8.4} -> {over:>8.4}");
        }
        _ => {
            println!("  min prefix stability:  (an arm has no comparable pair)");
        }
    }
    println!(
        "  projected median input improvement: {:+.1}%  (projected estimate, not billed)",
        replay::projected_improvement_percent(baseline, overlay),
    );
}

/// Percentage change overlay vs baseline; `0.0` when baseline is zero
/// (nothing to improve on).
fn delta_pct(baseline: f64, overlay: f64) -> f64 {
    if baseline <= f64::EPSILON {
        0.0
    } else {
        (overlay - baseline) / baseline * 100.0
    }
}

/// The validity verdict with per-check detail; the limitation reminder
/// rides along so a copied block never loses it.
fn print_validity(report: &ValidityReport) {
    let show = |check: Option<bool>| match check {
        Some(true) => "yes".to_owned(),
        Some(false) => "NO".to_owned(),
        None => "n/a".to_owned(),
    };
    if let Some((trace_schema, binary_schema)) = report.schema_skew {
        println!(
            "validity vs recorded run: NOT ASSERTABLE — trace schema {trace_schema} \
             predates this binary's schema {binary_schema} (census and prune events \
             are not comparable across compiler generations; record a fresh trace \
             to validate; turns still compared: {})",
            show(report.turns_match),
        );
    } else {
        let verdict = if report.valid { "OK" } else { "FAILED" };
        println!(
            "validity vs recorded run: {verdict} (turns {}, census {}, prune events {})",
            show(report.turns_match),
            show(report.census_match),
            show(report.prunes_match),
        );
    }
    for note in &report.notes {
        println!("  - {note}");
    }
    if !report.valid {
        println!(
            "  VALIDITY FAILED: do not trust this replay — the re-compiled \
             projection disagrees with the recorded run"
        );
    }
}

/// JSON output: the same content, machine-readable.
fn print_json(
    source_desc: &str,
    trace: &TraceInput,
    baseline: &ReplaySummary,
    valid: bool,
    overlay: Option<(&[String], &ReplaySummary)>,
    validity: &ValidityReport,
) {
    let arm = |summary: &ReplaySummary| {
        serde_json::json!({
            "turns": summary.turns.iter().map(|turn| serde_json::json!({
                "turn": turn.turn,
                "input_tokens": turn.input_tokens,
                "census": {
                    "user_items": turn.census.user_items,
                    "assistant_history": turn.census.assistant_history,
                    "reasoning_replay": turn.census.reasoning_replay,
                    "tool_results": turn.census.tool_results,
                    "tool_specs": turn.census.tool_specs,
                    "total": turn.census.total,
                },
                "froze_reclaimed_tokens": turn.froze.as_ref()
                    .map(|report| report.reclaimed_tokens),
                "prefix_stability": turn.prefix_stability,
                "stubbed_calls": turn.stubbed_calls,
            })).collect::<Vec<_>>(),
            "total_input_tokens": summary.total_input_tokens,
            "reclaimed_tokens": summary.reclaimed_tokens,
            "freezes": summary.freezes,
            "median_input_tokens": summary.median_input_tokens,
            "min_prefix_stability": summary.min_prefix_stability,
            "handoff_stopped_at": summary.handoff_stopped_at,
        })
    };
    let mut report = serde_json::json!({
        "source": source_desc,
        "provider": trace.provider,
        "model": trace.model,
        "limitation": LIMITATION,
        "baseline": arm(baseline),
        "validity": {
            "valid": valid,
            "schema_skew": validity.schema_skew,
            "turns_match": validity.turns_match,
            "census_match": validity.census_match,
            "prunes_match": validity.prunes_match,
            "notes": validity.notes,
        },
    });
    if let Some((applied, summary)) = overlay {
        report["overlay"] = serde_json::json!({
            "applied": applied,
            "summary": arm(summary),
            "projected_median_input_improvement_pct":
                replay::projected_improvement_percent(baseline, summary),
        });
    }
    let rendered = serde_json::to_string_pretty(&report)
        .unwrap_or_else(|error| format!("{{\"render_error\": \"{error}\"}}"));
    println!("{rendered}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with a manifest is a trace; anything else must parse
    /// as a session id. Neither branch touches the store here.
    #[test]
    fn source_resolution_prefers_trace_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("task");
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("manifest.json"), "{}").expect("manifest");
        let Source::Trace(path) =
            resolve_source(base.to_str().expect("utf8")).expect("a manifest dir is a trace source")
        else {
            panic!("expected a trace source");
        };
        assert_eq!(path, base);

        assert!(
            resolve_source(dir.path().to_str().expect("utf8")).is_err(),
            "a directory without manifest.json must not resolve"
        );
    }

    #[test]
    fn paired_deltas_computed_from_summaries() {
        let baseline = ReplaySummary {
            turns: Vec::new(),
            total_input_tokens: 10_000,
            reclaimed_tokens: 0,
            freezes: 0,
            median_input_tokens: 1_000.0,
            min_prefix_stability: Some(0.99),
            handoff_stopped_at: None,
        };
        let overlay = ReplaySummary {
            turns: Vec::new(),
            total_input_tokens: 5_000,
            reclaimed_tokens: 6_000,
            freezes: 2,
            median_input_tokens: 500.0,
            min_prefix_stability: Some(0.95),
            handoff_stopped_at: None,
        };
        assert_eq!(delta_pct(1_000.0, 500.0), -50.0);
        assert_eq!(
            replay::projected_improvement_percent(&baseline, &overlay),
            50.0
        );
        assert_eq!(delta_pct(0.0, 5.0), 0.0, "a zero baseline has no delta");
    }
}
