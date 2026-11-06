//! `loom stats`: transcript analytics over stored sessions (roadmap:
//! efficiency eval phase 2 — the `loom stats` mechanism-savings surface).
//!
//! The data step — listing, filtering, and folding every stored session —
//! is [`sessions_overview`](loom_core::kernel::analytics::sessions_overview)
//! in the kernel, so a stored session and its eval report always agree and
//! every frontend computes identical numbers. This module owns only the
//! presentation. What neither the transcript nor the persisted event
//! stream carries (billed usage, cache hit/miss) is not estimated here;
//! those numbers exist only in live-run reports, and printing them from
//! stored state would be dishonest accounting.
//!
//! The `mechanisms` section (roadmap item 1's open tail) attributes the
//! harness's context-saving levers per mechanism: prune reports and fast
//! paths from the persisted event stream, cache stub hits, omission
//! markers, and repairs from the transcript. Token numbers appear only
//! where an event payload carried them (estimator-side reclaim); every
//! other mechanism is reported as the occurrence count it is.
//!
//! `--trace DIR` switches the source to exported traces instead: there
//! the manifests carry the provider's own billed counts and the event
//! streams carry `context_pruned` reports, so the attribution the
//! transcript cannot offer is reported directly. The aggregation is
//! [`loom_core::engine::trace_stats`]; this module renders it.
//!
//! The skills section (roadmap gap: skill telemetry) extends the
//! per-tool accounting to `load_skill`: per-skill body loads,
//! bundled-resource reads, failures, and bytes delivered into
//! context — the usage-frequency and context-cost evidence the skill
//! lifecycle rule (retire low-value skills on evidence) needs.
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
use loom_core::kernel::analytics::{
    SessionStats, SessionsOverview, Spill, mechanisms_json, mechanisms_line, mechanisms_overview,
    sessions_overview,
};
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
    // Per-mechanism attribution (roadmap item 1's open tail): the
    // persisted event stream supplies what the transcript cannot —
    // prune reports and fast paths — while stub hits, omission markers,
    // and repairs come from the transcript. Same id set as the rows
    // above, so both sections describe the same sessions.
    let mechanisms = mechanisms_overview(&store, &store, args.session.as_deref())?;
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
            "mechanisms": mechanisms_json(&mechanisms),
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
        println!("mechanisms: {}", mechanisms_line(&mechanisms));
        if let Some(line) = skills_line(&overview.totals) {
            println!("skills: {line}");
        }
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
        if !totals.decisions.fast_path_declined.is_empty()
            || !totals.decisions.harness_notices.is_empty()
        {
            let mut parts: Vec<String> = Vec::new();
            if totals.decisions.capabilities_resolved > 0 {
                parts.push(format!(
                    "{} capability resolution{}",
                    totals.decisions.capabilities_resolved,
                    if totals.decisions.capabilities_resolved == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
            }
            for (reason, count) in &totals.decisions.fast_path_declined {
                parts.push(format!("fast path declined ({reason})×{count}"));
            }
            for (category, count) in &totals.decisions.harness_notices {
                parts.push(format!("{category}×{count}"));
            }
            println!("decisions: {}", parts.join(", "));
        }
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

/// `1 load` vs `2 loads`.
fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// Per-skill telemetry, human form: one summary per skill in name
/// order, semicolon-separated. Totals only — per-session detail is
/// the JSON's job. `None` when no `load_skill` call exists anywhere,
/// so skill-free stores keep their exact old output.
fn skills_line(stats: &SessionStats) -> Option<String> {
    if stats.skills.is_empty() {
        return None;
    }
    let parts: Vec<String> = stats
        .skills
        .iter()
        .map(|(name, s)| {
            let mut part = format!(
                "{name} {} {}, {} {}",
                s.loads,
                plural(s.loads, "load", "loads"),
                s.resources,
                plural(s.resources, "resource", "resources"),
            );
            if s.failures > 0 {
                part.push_str(&format!(
                    ", {} {}",
                    s.failures,
                    plural(s.failures, "failure", "failures")
                ));
            }
            part.push_str(&format!(", {} bytes delivered", s.delivered_bytes));
            part
        })
        .collect();
    Some(parts.join("; "))
}

/// The JSON form of the skills block: one row per skill, name order.
fn skills_json(stats: &SessionStats) -> serde_json::Value {
    stats
        .skills
        .iter()
        .map(|(name, s)| {
            serde_json::json!({
                "name": name,
                "loads": s.loads,
                "resources": s.resources,
                "failures": s.failures,
                "delivered_bytes": s.delivered_bytes,
            })
        })
        .collect::<Vec<_>>()
        .into()
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
        "skills": skills_json(stats),
        "spills": stats.spills.len(),
        "spill_saved_bytes": saved,
    })
}
