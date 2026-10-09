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
//!
//! The artifacts section (roadmap 2.17) reports the body store's
//! lifecycle state: how many bodies exist, how many the stored
//! transcripts still reference, and how many bytes the GC's backstop
//! bounds would reclaim. It is always store-wide — a `--session` filter
//! narrows the rows above it, but references from other sessions keep
//! artifacts alive, so scoping the section to the filter would
//! overcount orphans.

use std::process::ExitCode;

use anyhow::Context;
use loom_core::engine::trace_stats::{
    attribution_line, collect, fold_totals, human_task, human_totals, task_json, totals_json,
};
use loom_core::kernel::analytics::{SessionStats, SessionsOverview, Spill, sessions_overview};
use loom_core::kernel::artifact_gc::{self, GcBounds};
use loom_core::kernel::store::ArtifactStore;
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
    let artifacts = artifacts_section(&store)?;

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
            "artifacts": artifacts_json(&artifacts),
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
        println!(
            "artifacts: {} bodies, {} bytes — {} referenced, {} orphaned ({} bytes, {} reclaimable by gc)",
            artifacts.bodies,
            artifacts.bytes,
            artifacts.referenced,
            artifacts.bodies - artifacts.referenced,
            artifacts.orphan_bytes,
            artifacts.reclaimable_bytes,
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

/// The artifact store's lifecycle numbers, as both output modes print
/// them (roadmap 2.17).
struct ArtifactsSection {
    bodies: u64,
    bytes: u64,
    referenced: u64,
    orphan_bytes: u64,
    reclaimable_bytes: u64,
}

/// Scans the artifact store: total bodies and bytes, the live count, and
/// the bytes a GC run would reclaim now — orphans past the backstop age
/// bound. Store-wide by contract (see the module doc).
fn artifacts_section(store: &SqliteStore) -> anyhow::Result<ArtifactsSection> {
    let referenced = artifact_gc::referenced_artifacts(store)?;
    let bounds = GcBounds::default();
    let now = artifact_gc::now_millis();
    let mut section = ArtifactsSection {
        bodies: 0,
        bytes: 0,
        referenced: 0,
        orphan_bytes: 0,
        reclaimable_bytes: 0,
    };
    for stat in store.list()? {
        section.bodies += 1;
        section.bytes += stat.bytes;
        if referenced.contains(&stat.id) {
            section.referenced += 1;
        } else {
            section.orphan_bytes += stat.bytes;
            if now.saturating_sub(stat.created_at) >= bounds.max_age_millis {
                section.reclaimable_bytes += stat.bytes;
            }
        }
    }
    Ok(section)
}

/// The JSON form of the lifecycle block.
fn artifacts_json(section: &ArtifactsSection) -> serde_json::Value {
    serde_json::json!({
        "bodies": section.bodies,
        "bytes": section.bytes,
        "referenced": section.referenced,
        "orphaned": section.bodies - section.referenced,
        "orphan_bytes": section.orphan_bytes,
        "reclaimable_bytes": section.reclaimable_bytes,
    })
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
