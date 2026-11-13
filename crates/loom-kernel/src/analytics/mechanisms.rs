//! Per-mechanism attribution (roadmap item 1's open tail): what the
//! context-saving mechanisms observed, split by what the durable data
//! can carry — measured estimator-side numbers versus plain occurrence
//! counts — plus the JSON and human-readable renderers `loom stats`
//! prints.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{EventEnvelope, EventKind};
use crate::session::Session;
use crate::store::{EventStore, SessionStore, StoreError};
use crate::transcript::Item;

/// Per-mechanism attribution for one session (roadmap item 1's open
/// tail: "persist events alongside transcripts, so `loom stats` can
/// attribute savings per mechanism").
///
/// Semantics are labeled honestly and split by what the data can carry:
///
/// * **Measured (estimator-side)** — `reclaimed_tokens_estimate` comes
///   from `context_pruned` event payloads, the same estimator-side
///   numbers the pruning compiler reports. They were never billed, and
///   the field says so in its name.
/// * **Counted (occurrences)** — every other field is a plain count.
///   The mechanisms whose payloads carry no token delta (cache stubs,
///   omission markers, repairs) are reported as the occurrences they
///   are, never as fabricated token savings.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MechanismStats {
    /// `context_pruned` events (pruning + stub elision freezes).
    pub prune_events: u64,
    /// Tool calls stubbed by those events (sum of per-report calls).
    pub pruned_calls: u64,
    /// Estimator-side reclaimed tokens, summed over prune reports.
    /// Measured by the estimator, never billed.
    pub reclaimed_tokens_estimate: u64,
    /// `context_handoff` events (the LLM-handoff elision of last
    /// resort, roadmap item 12).
    pub handoffs: u64,
    /// Tool-cache stub hits: results served as
    /// `unchanged since call <id>` while the original stayed in the
    /// projection (roadmap item 6). Counted from the transcript.
    pub stub_hits: u64,
    /// Counted omission markers in tool results: the explicit
    /// `… [N bytes omitted]` / `[... N more X omitted]` family of
    /// evidence-shaping and output-filter reductions (ADR-014).
    pub omission_markers: u64,
    /// Calls the harness repaired before or during execution (the same
    /// effects-derived count as [`SessionStats::repairs`](super::SessionStats::repairs)).
    pub repairs: u64,
    /// `fast_path_hit` events: deterministic solves with zero model
    /// requests (roadmap item 3).
    pub fast_path_hits: u64,
    /// Files the fast paths rewrote across those solves.
    pub fast_path_files_changed: u64,
}

impl MechanismStats {
    /// Adds `other` into `self`, counter by counter.
    pub fn add(&mut self, other: &MechanismStats) {
        self.prune_events += other.prune_events;
        self.pruned_calls += other.pruned_calls;
        self.reclaimed_tokens_estimate += other.reclaimed_tokens_estimate;
        self.handoffs += other.handoffs;
        self.stub_hits += other.stub_hits;
        self.omission_markers += other.omission_markers;
        self.repairs += other.repairs;
        self.fast_path_hits += other.fast_path_hits;
        self.fast_path_files_changed += other.fast_path_files_changed;
    }

    /// True when every counter is zero — a session where no mechanism
    /// observed anything worth reporting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The cache-stub prefix the tool engine serves on a repeat read whose
/// original result is still projected unstubbed (roadmap item 6;
/// `agent::tools::cached_result` formats the full line as
/// `unchanged since call <id>`).
const STUB_PREFIX: &str = "unchanged since call ";

/// The omission-marker suffix shared by every member of the
/// explicit-omission family (ADR-014): the per-tool output budgets'
/// `... [N bytes omitted]`, the command streams' head+tail cuts, the
/// per-command reducers' `[... N more X omitted]`, and evidence
/// shaping's `... [N more … omitted]` caps. Mirrored here as a literal
/// because the kernel sits below `loom-tools`; when the marker family
/// grows a new shape, this moves with it.
const OMISSION_MARKER: &str = "omitted]";

/// Counts the omission markers inside one tool result's content.
fn count_omission_markers(content: &str) -> u64 {
    content.matches(OMISSION_MARKER).count() as u64
}

/// Computes [`MechanismStats`] for one session from its two durable
/// records: the transcript (stub hits, omission markers, repairs) and
/// the persisted event stream (prunes, handoffs, fast paths).
///
/// Pure: one pass over each input, no I/O, no clocks. An empty event
/// slice is legitimate — every session that predates event persistence,
/// or ran with it off — and only the transcript-derived counters move.
#[must_use]
pub fn session_mechanisms(session: &Session, events: &[EventEnvelope]) -> MechanismStats {
    let mut stats = MechanismStats::default();

    for item in &session.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        if result.content.starts_with(STUB_PREFIX) {
            stats.stub_hits += 1;
        }
        stats.omission_markers += count_omission_markers(&result.content);
        stats.repairs += result.effects.repairs.len() as u64;
    }

    for envelope in events {
        // Delta/prune events are stats-neutral, and unknown future
        // events must not silently move aggregate counters either.
        #[allow(clippy::wildcard_enum_match_arm)]
        match &envelope.kind {
            EventKind::ContextPruned { report, .. } => {
                stats.prune_events += 1;
                stats.pruned_calls += report.calls.len() as u64;
                stats.reclaimed_tokens_estimate += report.reclaimed_tokens;
            }
            EventKind::ContextHandoff { .. } => stats.handoffs += 1,
            EventKind::FastPathHit { files_changed, .. } => {
                stats.fast_path_hits += 1;
                stats.fast_path_files_changed += u64::from(*files_changed);
            }
            _ => {}
        }
    }

    stats
}

/// Folds mechanism attribution across every session in the store,
/// optionally keeping only the ones whose id contains `filter` — the
/// same id set and ordering policy as
/// [`sessions_overview`](super::sessions_overview), so the two
/// reports always describe the same rows.
///
/// Sessions and events are separate seams (a store may implement
/// either); both come from the same database in every current frontend.
///
/// # Errors
///
/// Returns [`StoreError`] when either store cannot enumerate sessions
/// or read a session's event stream.
pub fn mechanisms_overview(
    sessions: &dyn SessionStore,
    events: &dyn EventStore,
    filter: Option<&str>,
) -> Result<MechanismStats, StoreError> {
    let mut ids = sessions.session_ids()?;
    ids.sort();

    let mut totals = MechanismStats::default();
    for id in ids {
        if let Some(filter) = filter
            && !id.to_string().contains(filter)
        {
            continue;
        }
        // A session that fails to load fails the pass, exactly like
        // `sessions_overview`: silently skipping would undercount.
        let Some(session) = sessions.load(&id)? else {
            continue;
        };
        let stream = events.load_events(&id)?;
        totals.add(&session_mechanisms(&session, &stream));
    }
    Ok(totals)
}

/// The JSON block for a mechanisms report, with the semantics note that
/// keeps counted fields from being read as token deltas.
#[must_use]
pub fn mechanisms_json(stats: &MechanismStats) -> Value {
    serde_json::json!({
        "prune_events": stats.prune_events,
        "pruned_calls": stats.pruned_calls,
        "reclaimed_tokens_estimate": stats.reclaimed_tokens_estimate,
        "handoffs": stats.handoffs,
        "stub_hits": stats.stub_hits,
        "omission_markers": stats.omission_markers,
        "repairs": stats.repairs,
        "fast_path_hits": stats.fast_path_hits,
        "fast_path_files_changed": stats.fast_path_files_changed,
        "note": "reclaimed_tokens_estimate is estimator-side (never billed); \
                 every other field is an occurrence count",
    })
}

/// One human-readable mechanisms line: only the mechanisms that fired,
/// each labeled by what it actually measured.
#[must_use]
pub fn mechanisms_line(stats: &MechanismStats) -> String {
    let mut parts = Vec::new();
    if stats.fast_path_hits > 0 {
        parts.push(format!(
            "{} fast-path solve{} ({} file{}, zero model calls)",
            stats.fast_path_hits,
            if stats.fast_path_hits == 1 { "" } else { "s" },
            stats.fast_path_files_changed,
            if stats.fast_path_files_changed == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    if stats.prune_events > 0 {
        parts.push(format!(
            "{} prune{} ({} call{} stubbed, ~{} tok estimator-side)",
            stats.prune_events,
            if stats.prune_events == 1 { "" } else { "s" },
            stats.pruned_calls,
            if stats.pruned_calls == 1 { "" } else { "s" },
            stats.reclaimed_tokens_estimate,
        ));
    }
    if stats.handoffs > 0 {
        parts.push(format!(
            "{} handoff{}",
            stats.handoffs,
            if stats.handoffs == 1 { "" } else { "s" },
        ));
    }
    if stats.stub_hits > 0 {
        parts.push(format!(
            "{} cache stub hit{} (\"unchanged since call\")",
            stats.stub_hits,
            if stats.stub_hits == 1 { "" } else { "s" },
        ));
    }
    if stats.omission_markers > 0 {
        parts.push(format!(
            "{} omission marker{}",
            stats.omission_markers,
            if stats.omission_markers == 1 { "" } else { "s" }
        ));
    }
    if stats.repairs > 0 {
        parts.push(format!(
            "{} repair{}",
            stats.repairs,
            if stats.repairs == 1 { "" } else { "s" },
        ));
    }
    if parts.is_empty() {
        "none observed".to_owned()
    } else {
        parts.join(", ")
    }
}
