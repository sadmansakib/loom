//! `loom stats`: transcript analytics over stored sessions (roadmap:
//! efficiency eval phase 2 — the `loom stats` mechanism-savings surface).
//!
//! The data step — listing, filtering, and folding every stored session —
//! is [`sessions_overview`](loom_core::kernel::analytics::sessions_overview)
//! in the kernel, so a stored session and its eval report always agree and
//! every frontend computes identical numbers. This module owns only the
//! presentation. What the transcript does not carry (billed usage, cache
//! hit/miss, prune decisions) is not estimated here; those numbers exist
//! only in live-run reports, and printing them from a transcript would be
//! dishonest accounting.
//!
//! `--trace DIR` switches the source to exported traces instead: there
//! the manifests carry the provider's own billed counts and the event
//! streams carry `context_pruned` reports, so the attribution the
//! transcript cannot offer is reported directly. The aggregation is
//! [`loom_core::engine::trace_stats`]; this module renders it.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::engine::trace_stats::{
    attribution_line, collect, fold_totals, human_task, human_totals, task_json, totals_json,
};
use loom_core::kernel::analytics::{SessionStats, SessionsOverview, Spill, sessions_overview};
use loom_core::store::SqliteStore;

use crate::args::StatsArgs;

/// Runs `loom stats`.
pub fn run(args: StatsArgs) -> anyhow::Result<ExitCode> {
    if let Some(base) = &args.trace {
        return trace_report(base, args.json);
    }
    let path = loom_core::paths::db_path();
    let store = SqliteStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let overview = sessions_overview(&store, args.session.as_deref())?;

    if overview.rows.is_empty() && !args.json {
        eprintln!(
            "no stored sessions{} in {} — run `loom run` first",
            args.session
                .as_deref()
                .map(|f| format!(" matching `{f}`"))
                .unwrap_or_default(),
            path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    if args.json {
        let report = serde_json::json!({
            "db": path,
            "sessions": overview
                .rows
                .iter()
                .map(|row| serde_json::json!({
                    "id": row.id,
                    "stats": row.stats,
                }))
                .collect::<Vec<_>>(),
            "totals": summarize(&overview),
            "note": "transcript-derived; billed usage and cache ratios live in live-run reports",
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("sessions in {}:", path.display());
        for row in &overview.rows {
            println!(
                "  {}  {} turns, {} calls ({} failed, {} malformed), {} repeats, {} spills ({})",
                row.id,
                row.stats.turns,
                row.stats.totals.calls,
                row.stats.totals.failed,
                row.stats.malformed_calls,
                row.stats.identical_repeats,
                row.stats.spills.len(),
                saved_summary(&row.stats),
            );
        }
        println!(
            "totals: {} sessions, {} turns, {} calls, {} failed, {} malformed, {} spills ({})",
            overview.rows.len(),
            overview.totals.turns,
            overview.totals.totals.calls,
            overview.totals.totals.failed,
            overview.totals.malformed_calls,
            overview.totals.spills.len(),
            saved_summary(&overview.totals),
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Renders the `--trace` report: aggregation from the engine, printing
/// here.
fn trace_report(base: &std::path::Path, json: bool) -> anyhow::Result<ExitCode> {
    let rows = collect(base)?;
    if rows.is_empty() && !json {
        eprintln!(
            "no task traces in {} — run `loom run --trace {}` first",
            base.display(),
            base.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let totals = fold_totals(&rows);
    if json {
        let report = serde_json::json!({
            "trace_dir": base.display().to_string(),
            "tasks": rows.iter().map(task_json).collect::<Vec<_>>(),
            "totals": totals_json(&totals),
            "note": "reclaimed tokens are estimator-side estimates; usage fields are the provider's billed counts",
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("traces in {}:", base.display());
        for row in &rows {
            println!("  {}", human_task(row));
            if row.prunes.by_trigger.is_empty() {
                continue;
            }
            let parts: Vec<String> = row
                .prunes
                .by_trigger
                .iter()
                .map(|(trigger, agg)| {
                    format!(
                        "{trigger}×{} ({} calls, ~{} tok)",
                        agg.events, agg.calls, agg.reclaimed_tokens
                    )
                })
                .collect();
            println!("    pruned: {}", parts.join(", "));
        }
        println!("totals: {}", human_totals(&totals));
        println!(
            "attribution: {}  (estimator-side estimates)",
            attribution_line(&totals)
        );
        if totals.corrupt > 0 {
            println!("skipped {} unreadable task directories", totals.corrupt);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Human-readable spill savings for one row.
fn saved_summary(row: &SessionStats) -> String {
    let saved: u64 = row.spills.iter().map(Spill::saved_bytes).sum();
    format!("{saved} bytes kept out of context")
}

/// The JSON totals block: counts plus the derived rates.
fn summarize(overview: &SessionsOverview) -> serde_json::Value {
    let rate = |n: u64, d: u64| -> serde_json::Value {
        if d == 0 {
            serde_json::json!(null)
        } else {
            serde_json::json!((n as f64 / d as f64 * 1e4).round() / 1e4)
        }
    };
    let stats = &overview.totals;
    let saved: u64 = stats.spills.iter().map(Spill::saved_bytes).sum();
    serde_json::json!({
        "sessions": overview.rows.len(),
        "turns": stats.turns,
        "calls": stats.totals.calls,
        "failed": stats.totals.failed,
        "malformed_calls": stats.malformed_calls,
        "tool_error_rate": rate(stats.totals.failed, stats.totals.calls),
        "malformed_rate": rate(stats.malformed_calls, stats.totals.calls),
        "identical_repeats": stats.identical_repeats,
        "repairs": stats.repairs,
        "edit_apply": stats.edit_apply,
        "spills": stats.spills.len(),
        "spill_saved_bytes": saved,
    })
}
