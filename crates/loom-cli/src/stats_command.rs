//! `loom stats`: transcript analytics over stored sessions (roadmap:
//! efficiency eval phase 2 — the `loom stats` mechanism-savings surface).
//!
//! Reads the durable store the engine already writes (`paths::db_path`,
//! overridable with `LOOM_DB`) and runs the same
//! [`session_stats`](loom_core::kernel::analytics::session_stats) the
//! eval harness embeds per run — so a stored session and its eval report
//! always agree. What the transcript does not carry (billed usage, cache
//! hit/miss, prune decisions) is not estimated here; those numbers exist
//! only in live-run reports, and printing them from a transcript would
//! be dishonest accounting.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::kernel::analytics::{SessionStats, ToolCounts, session_stats};
use loom_core::kernel::store::SessionStore;
use loom_store::SqliteStore;

use crate::args::StatsArgs;

/// One analyzed session, label attached.
struct Row {
    id: String,
    stats: SessionStats,
}

/// Runs `loom stats`.
pub fn run(args: StatsArgs) -> anyhow::Result<ExitCode> {
    let path = crate::paths::db_path();
    let store = SqliteStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut ids = store.session_ids()?;
    // v7 ids order by creation time; make the listing deterministic
    // regardless of the store's iteration order.
    ids.sort();

    let filter = args.session.as_deref();
    let mut rows = Vec::new();
    for id in ids {
        let Some(session) = store.load(&id).context("loading session")? else {
            continue;
        };
        let label = id.to_string();
        if let Some(filter) = filter
            && !label.contains(filter)
        {
            continue;
        }
        rows.push(Row {
            id: label,
            stats: session_stats(&session),
        });
    }

    if rows.is_empty() && !args.json {
        eprintln!(
            "no stored sessions{} in {} — run `loom run` first",
            filter
                .map(|f| format!(" matching `{f}`"))
                .unwrap_or_default(),
            path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let totals = fold_totals(&rows);
    if args.json {
        let report = serde_json::json!({
            "db": path,
            "sessions": rows
                .iter()
                .map(|row| serde_json::json!({
                    "id": row.id,
                    "stats": row.stats,
                }))
                .collect::<Vec<_>>(),
            "totals": summarize(&totals, &rows),
            "note": "transcript-derived; billed usage and cache ratios live in live-run reports",
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("sessions in {}:", path.display());
        for row in &rows {
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
            rows.len(),
            totals.turns,
            totals.totals.calls,
            totals.totals.failed,
            totals.malformed_calls,
            totals.spills.len(),
            saved_summary(&totals),
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Human-readable spill savings for one row.
fn saved_summary(row: &SessionStats) -> String {
    let saved: u64 = row
        .spills
        .iter()
        .map(loom_core::kernel::analytics::Spill::saved_bytes)
        .sum();
    format!("{} bytes kept out of context", saved)
}

/// Sums rows into one stats value (turns/calls/counts add; spills concat).
fn fold_totals(rows: &[Row]) -> SessionStats {
    let mut totals = SessionStats::default();
    for row in rows {
        totals.turns += row.stats.turns;
        totals.identical_repeats += row.stats.identical_repeats;
        totals.edit_apply.attempts += row.stats.edit_apply.attempts;
        totals.edit_apply.applied += row.stats.edit_apply.applied;
        totals.malformed_calls += row.stats.malformed_calls;
        for (name, counts) in &row.stats.tools {
            merge(totals.tools.entry(name.clone()).or_default(), counts);
        }
        merge(&mut totals.totals, &row.stats.totals);
        totals.spills.extend(row.stats.spills.iter().cloned());
    }
    totals
}

/// Adds `from` into `into`, counter by counter.
fn merge(into: &mut ToolCounts, from: &ToolCounts) {
    into.calls += from.calls;
    into.success += from.success;
    into.invalid_input += from.invalid_input;
    into.execution += from.execution;
    into.timeout += from.timeout;
    into.repeated += from.repeated;
    into.unknown_tool += from.unknown_tool;
    into.not_exposed += from.not_exposed;
    into.cancelled += from.cancelled;
    into.failed += from.failed;
}

/// The JSON totals block: counts plus the derived rates.
fn summarize(stats: &SessionStats, rows: &[Row]) -> serde_json::Value {
    let rate = |n: u64, d: u64| -> serde_json::Value {
        if d == 0 {
            serde_json::json!(null)
        } else {
            serde_json::json!((n as f64 / d as f64 * 1e4).round() / 1e4)
        }
    };
    let saved: u64 = stats
        .spills
        .iter()
        .map(loom_core::kernel::analytics::Spill::saved_bytes)
        .sum();
    serde_json::json!({
        "sessions": rows.len(),
        "turns": stats.turns,
        "calls": stats.totals.calls,
        "failed": stats.totals.failed,
        "malformed_calls": stats.malformed_calls,
        "tool_error_rate": rate(stats.totals.failed, stats.totals.calls),
        "malformed_rate": rate(stats.malformed_calls, stats.totals.calls),
        "identical_repeats": stats.identical_repeats,
        "edit_apply": stats.edit_apply,
        "spills": stats.spills.len(),
        "spill_saved_bytes": saved,
    })
}
