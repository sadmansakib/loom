//! Transcript analytics: per-tool call quality from a persisted
//! [`Session`] (roadmap: efficiency eval phase 2, micro-evals).
//!
//! Everything here is derived from the durable transcript alone — the
//! same data any replay sees — so the numbers are reproducible offline
//! and identical whether computed live, from a stored session, or from
//! an eval recording. What the transcript does not carry (usage, cache
//! hit/miss, prune decisions) is deliberately *not* estimated here:
//! those live in the live-run reports only, and claiming them from a
//! transcript would be dishonest accounting.
//!
//! The split the stats encode:
//!
//! * **Model-side faults** (`unknown_tool`, `not_exposed`,
//!   `invalid_input`): the model emitted a call the harness could not
//!   honor as-is — a malformed-signal rate, the number a call-repair
//!   layer (roadmap) would have to fix.
//! * **Execution faults** (`execution`, `timeout`): the program ran and
//!   failed — legitimate signal for the model, but a high rate still
//!   marks wasted turns.
//! * **`repeated`**: the identical-call ledger refused a re-dispatch —
//!   pure waste the ledger already prevented; also counted in
//!   `identical_repeats`, which additionally counts *executed*
//!   identical calls the ledger cannot see (same name + canonical
//!   arguments, but not back-to-back).
//! * **`edit_apply`**: attempts vs applied for the edit/write family —
//!   the model-side patch acceptance rate.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{EventEnvelope, EventKind};
use crate::id::SessionId;
use crate::session::Session;
use crate::store::{EventStore, SessionStore, StoreError};
use crate::transcript::{Item, ToolFailure, ToolStatus};

/// Per-tool counters derived from one transcript.
///
/// Every counter is a plain count; rates are computed by the caller so
/// the JSON stays lossless.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCounts {
    /// Total results recorded for this tool (every status).
    pub calls: u64,
    /// `Success` results.
    pub success: u64,
    /// `InvalidInput` results (model-side fault).
    pub invalid_input: u64,
    /// `Execution` results.
    pub execution: u64,
    /// `Timeout` results.
    pub timeout: u64,
    /// `RepeatedCall` results (ledger refusal).
    pub repeated: u64,
    /// `UnknownTool` results (model-side fault).
    pub unknown_tool: u64,
    /// `NotExposed` results (model-side fault).
    pub not_exposed: u64,
    /// `Cancelled` results.
    pub cancelled: u64,
    /// Sum of every non-success, non-cancelled status.
    pub failed: u64,
}

impl ToolCounts {
    /// Counts one status into the map.
    fn record(&mut self, status: &ToolStatus) {
        self.calls += 1;
        match status {
            ToolStatus::Success => self.success += 1,
            ToolStatus::Cancelled { .. } => self.cancelled += 1,
            ToolStatus::Error(failure) => {
                self.failed += 1;
                match failure {
                    ToolFailure::UnknownTool => self.unknown_tool += 1,
                    ToolFailure::NotExposed => self.not_exposed += 1,
                    ToolFailure::InvalidInput => self.invalid_input += 1,
                    ToolFailure::Execution => self.execution += 1,
                    ToolFailure::Timeout => self.timeout += 1,
                    ToolFailure::RepeatedCall => self.repeated += 1,
                }
            }
        }
    }

    /// Model-side fault count: calls the harness could not honor as
    /// emitted (unknown, not exposed, malformed input).
    #[must_use]
    pub fn malformed(&self) -> u64 {
        self.unknown_tool + self.not_exposed + self.invalid_input
    }

    /// Adds `other` into `self`, counter by counter (store-wide totals).
    pub fn add(&mut self, other: &ToolCounts) {
        self.calls += other.calls;
        self.success += other.success;
        self.invalid_input += other.invalid_input;
        self.execution += other.execution;
        self.timeout += other.timeout;
        self.repeated += other.repeated;
        self.unknown_tool += other.unknown_tool;
        self.not_exposed += other.not_exposed;
        self.cancelled += other.cancelled;
        self.failed += other.failed;
    }
}

/// Edit-family acceptance: how many edit/write calls actually applied.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditApply {
    /// Calls to `edit_file` or `write_file`.
    pub attempts: u64,
    /// Of those, results with `Success` status.
    pub applied: u64,
}

/// One oversized result that spilled to the artifact store.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spill {
    /// The spilled call's id.
    pub call_id: String,
    /// Tool that produced the oversize output.
    pub tool: String,
    /// Bytes the model actually received (the stub's length).
    pub delivered_bytes: u64,
    /// Bytes in the stored artifact body (`ArtifactRef::bytes`).
    pub artifact_bytes: u64,
    /// Store key of the body.
    pub artifact_id: String,
}

impl Spill {
    /// Bytes kept out of the context window by the spill.
    #[must_use]
    pub fn saved_bytes(&self) -> u64 {
        self.artifact_bytes.saturating_sub(self.delivered_bytes)
    }
}

/// Whole-transcript analytics (see the module docs for the fault
/// taxonomy).
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// Assistant items in the transcript (one model response each).
    pub turns: u64,
    /// Per-tool counters, keyed by tool name, in call order.
    pub tools: BTreeMap<String, ToolCounts>,
    /// Sum over all tools.
    pub totals: ToolCounts,
    /// Model-side faults across all tools.
    pub malformed_calls: u64,
    /// Edit-family acceptance.
    pub edit_apply: EditApply,
    /// Identical calls (same name + canonical arguments) beyond the
    /// first — includes both executed repeats and ledger refusals.
    pub identical_repeats: u64,
    /// Calls Loom repaired before or during execution (sum of the
    /// per-effect repair notes, e.g. whitespace-tolerant edit matches).
    pub repairs: u64,
    /// Spilled oversize results, in transcript order.
    pub spills: Vec<Spill>,
}

impl SessionStats {
    /// Adds `other` into `self`: counts sum, per-tool maps merge, spills
    /// concatenate in order. The store-wide totals of a
    /// [`sessions_overview`] are folded with this.
    pub fn add(&mut self, other: &SessionStats) {
        self.turns += other.turns;
        self.identical_repeats += other.identical_repeats;
        self.repairs += other.repairs;
        self.edit_apply.attempts += other.edit_apply.attempts;
        self.edit_apply.applied += other.edit_apply.applied;
        self.malformed_calls += other.malformed_calls;
        for (name, counts) in &other.tools {
            self.tools.entry(name.clone()).or_default().add(counts);
        }
        self.totals.add(&other.totals);
        self.spills.extend(other.spills.iter().cloned());
    }
}

/// Computes [`SessionStats`] from a persisted session.
///
/// Pure: one pass over the transcript, no I/O, no clocks. Assistant
/// tool-call metadata (name, canonical arguments) is joined to each
/// result by call id, exactly as the projection does.
#[must_use]
pub fn session_stats(session: &Session) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut calls: HashMap<&str, (&str, String)> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for item in &session.transcript {
        match item {
            Item::User { .. } => {}
            Item::Assistant(assistant) => {
                stats.turns += 1;
                for call in &assistant.tool_calls {
                    // Canonical key: serialized arguments (BTreeMap
                    // ordering), the same string the identical-call
                    // ledger keys on.
                    let canonical = serde_json::to_string(&Value::from(call.arguments.clone()))
                        .unwrap_or_default();
                    if !seen.insert((call.name.clone(), canonical.clone())) {
                        stats.identical_repeats += 1;
                    }
                    calls.insert(call.id.as_str(), (&call.name, canonical));
                }
            }
            Item::ToolResult(result) => {
                static UNKNOWN: (&str, String) = ("?", String::new());
                let (name, _canonical) = calls.get(result.call_id.as_str()).unwrap_or(&UNKNOWN);
                let name = (*name).to_owned();

                let counts = stats.tools.entry(name.clone()).or_default();
                counts.record(&result.status);
                stats.totals.record(&result.status);
                stats.repairs += result.effects.repairs.len() as u64;

                if name == "edit_file" || name == "write_file" {
                    stats.edit_apply.attempts += 1;
                    if matches!(result.status, ToolStatus::Success) {
                        stats.edit_apply.applied += 1;
                    }
                }

                if let Some(reference) = &result.artifact {
                    stats.spills.push(Spill {
                        call_id: result.call_id.as_str().to_owned(),
                        tool: name,
                        delivered_bytes: result.content.len() as u64,
                        artifact_bytes: reference.bytes,
                        artifact_id: reference.id.to_string(),
                    });
                }
            }
        }
    }

    stats.malformed_calls = stats.totals.malformed();
    stats
}

/// One analyzed session from a [`sessions_overview`] pass.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOverview {
    /// The session's id (v7, creation-ordered; also the label a
    /// frontend filters on).
    pub id: SessionId,
    /// Transcript-derived statistics for the session.
    pub stats: SessionStats,
}

/// Store-wide transcript analytics: every analyzed session plus the
/// totals folded across them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionsOverview {
    /// One row per analyzed session, oldest creation order.
    pub rows: Vec<SessionOverview>,
    /// Every row folded into one value ([`SessionStats::add`]).
    pub totals: SessionStats,
}

/// Analyzes every session in `store`, optionally keeping only the ones
/// whose id contains `filter`.
///
/// This is the whole of `loom stats`' data step — the iteration,
/// filtering, and folding are policy, not presentation, so every
/// frontend lists sessions identically. Derives each row with
/// [`session_stats`]; an unreadable session fails the pass (the store
/// is the source of truth, and silently skipping a row would
/// undercount).
pub fn sessions_overview(
    store: &dyn SessionStore,
    filter: Option<&str>,
) -> Result<SessionsOverview, StoreError> {
    // v7 ids order by creation time; sort regardless so the listing is
    // deterministic whatever the store's iteration order.
    let mut ids = store.session_ids()?;
    ids.sort();

    let mut overview = SessionsOverview::default();
    for id in ids {
        let Some(session) = store.load(&id)? else {
            continue;
        };
        if let Some(filter) = filter
            && !id.to_string().contains(filter)
        {
            continue;
        }
        let stats = session_stats(&session);
        overview.totals.add(&stats);
        overview.rows.push(SessionOverview { id, stats });
    }
    Ok(overview)
}

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
    /// effects-derived count as [`SessionStats::repairs`]).
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
/// same id set and ordering policy as [`sessions_overview`], so the two
/// reports always describe the same rows.
///
/// Sessions and events are separate seams (a store may implement
/// either); both come from the same database in every current frontend.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactRef;
    use crate::id::ArtifactId;
    use crate::model::StopReason;
    use crate::tool::ToolEffects;
    use crate::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolResult};
    use serde_json::json;

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
            name: name.to_owned(),
            arguments: ToolArguments::try_from(args).expect("arguments"),
        }
    }

    fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
        AssistantMessage {
            content: text.to_owned(),
            reasoning: None,
            tool_calls: calls,
            stop_reason: StopReason::ToolCalls,
        }
    }

    fn ok_result(id: &str, content: &str) -> ToolResult {
        ToolResult::new(
            crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
            ToolStatus::Success,
            content,
            false,
        )
    }

    fn failed_result(id: &str, failure: ToolFailure) -> ToolResult {
        ToolResult::new(
            crate::transcript::ToolCallId::try_from(id.to_owned()).expect("call id"),
            ToolStatus::Error(failure),
            "boom",
            false,
        )
    }

    fn session(items: Vec<Item>) -> Session {
        Session {
            id: crate::id::SessionId::new(),
            transcript: items,
            tasks: Vec::new(),
            todos: crate::session::TodoList::default(),
        }
    }

    #[test]
    fn repairs_are_counted_from_effects() {
        let mut repaired = ok_result("a", "edited f.rs");
        repaired.effects = ToolEffects::none().with_repair("old_string matched after trimming");
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![call("a", "edit_file", json!({"path": "f.rs"}))],
            )),
            Item::ToolResult(repaired),
        ];
        let stats = session_stats(&session(items));
        assert_eq!(stats.repairs, 1);
    }

    #[test]
    fn counts_statuses_per_tool_and_in_total() {
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![
                    call("a", "read_file", json!({"path": "x"})),
                    call("b", "edit_file", json!({"path": "y"})),
                    call("c", "run_command", json!({"program": "cargo"})),
                ],
            )),
            Item::ToolResult(ok_result("a", "contents")),
            Item::ToolResult(failed_result("b", ToolFailure::InvalidInput)),
            Item::ToolResult(failed_result("c", ToolFailure::Execution)),
            Item::Assistant(assistant(
                "again",
                vec![call("d", "read_file", json!({"path": "z"}))],
            )),
            Item::ToolResult(failed_result("d", ToolFailure::RepeatedCall)),
        ];
        let stats = session_stats(&session(items));

        assert_eq!(stats.turns, 2);
        let reads = &stats.tools["read_file"];
        assert_eq!((reads.calls, reads.success, reads.repeated), (2, 1, 1));
        assert_eq!(stats.tools["edit_file"].invalid_input, 1);
        assert_eq!(stats.tools["run_command"].execution, 1);
        assert_eq!(stats.totals.calls, 4);
        assert_eq!(stats.totals.failed, 3);
        assert_eq!(stats.malformed_calls, 1);
    }

    #[test]
    fn identical_repeats_count_executed_duplicates_too() {
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![call("a", "read_file", json!({"path": "x"}))],
            )),
            Item::ToolResult(ok_result("a", "contents")),
            Item::Assistant(assistant(
                "other",
                vec![call("b", "edit_file", json!({"path": "y"}))],
            )),
            Item::ToolResult(ok_result("b", "done")),
            // Same tool AND same canonical arguments as call a, but not
            // adjacent: the ledger lets it through, the stats do not.
            Item::Assistant(assistant(
                "again",
                vec![call("c", "read_file", json!({"path": "x"}))],
            )),
            Item::ToolResult(ok_result("c", "contents")),
        ];
        let stats = session_stats(&session(items));
        assert_eq!(stats.identical_repeats, 1);
        // Canonical keying: same path, different key order in the JSON
        // literal, still identical once serialized.
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![call("a", "read_file", json!({"path": "x", "offset": 0}))],
            )),
            Item::ToolResult(ok_result("a", "contents")),
            Item::Assistant(assistant(
                "again",
                vec![call("b", "read_file", json!({"offset": 0, "path": "x"}))],
            )),
            Item::ToolResult(ok_result("b", "contents")),
        ];
        let stats = session_stats(&session(items));
        assert_eq!(stats.identical_repeats, 1);
    }

    #[test]
    fn edit_acceptance_and_spill_savings_are_derived() {
        let mut spilled = ok_result("a", "[artifact: 64 KiB body omitted]");
        spilled.artifact = Some(ArtifactRef::new(
            ArtifactId::new(),
            "tool_output",
            65_536,
            "deadbeef",
        ));
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![
                    call("a", "run_command", json!({"program": "cargo"})),
                    call("b", "write_file", json!({"path": "y"})),
                    call("c", "edit_file", json!({"path": "z"})),
                ],
            )),
            Item::ToolResult(spilled),
            Item::ToolResult(ok_result("b", "written")),
            Item::ToolResult(failed_result("c", ToolFailure::Execution)),
        ];
        let stats = session_stats(&session(items));

        assert_eq!(stats.edit_apply.attempts, 2);
        assert_eq!(stats.edit_apply.applied, 1);
        assert_eq!(stats.spills.len(), 1);
        let spill = &stats.spills[0];
        assert_eq!(spill.tool, "run_command");
        assert_eq!(spill.saved_bytes(), 65_536 - spilled_content_len(spill));
    }

    /// The stub length the test above planted, for the savings assert.
    fn spilled_content_len(spill: &Spill) -> u64 {
        spill.delivered_bytes
    }

    #[test]
    fn an_empty_transcript_yields_zeroed_stats() {
        let stats = session_stats(&session(Vec::new()));
        assert_eq!(stats.turns, 0);
        assert_eq!(stats.totals.calls, 0);
        assert!(stats.tools.is_empty());
        assert!(stats.spills.is_empty());
    }

    #[test]
    fn sessions_overview_folds_totals_across_the_store() {
        use crate::store::MemorySessionStore;

        // Seeds one session through the store's own seams: plan the turn
        // against a local mirror, persist, then apply — the same dance
        // the loop performs per committed turn.
        fn seed(
            store: &MemorySessionStore,
            calls: Vec<ToolCall>,
            results: Vec<ToolResult>,
        ) -> SessionId {
            let mut session = Session::new(SessionId::new());
            store.ensure_session(&session.id).unwrap();
            let user = Item::User {
                text: "task".into(),
            };
            let index = session.push_item(user.clone()).unwrap();
            store.append_item(&session.id, index, &user).unwrap();

            let planned = session.plan_turn(assistant("go", calls), results).unwrap();
            store.append_turn(&session.id, &planned).unwrap();
            session.apply_planned_turn(planned);
            session.id
        }

        let store = MemorySessionStore::new();
        let first = seed(
            &store,
            vec![call("a", "read_file", json!({"path": "x"}))],
            vec![ok_result("a", "contents")],
        );
        let second = seed(
            &store,
            vec![call("b", "run_command", json!({"program": "cargo"}))],
            vec![failed_result("b", ToolFailure::Execution)],
        );

        let overview = sessions_overview(&store, None).unwrap();
        assert_eq!(overview.rows.len(), 2);
        assert_eq!(overview.totals.turns, 2);
        assert_eq!(overview.totals.totals.calls, 2);
        assert_eq!(overview.totals.totals.failed, 1);
        let listed: HashSet<SessionId> = overview.rows.iter().map(|row| row.id).collect();
        assert_eq!(listed, HashSet::from([first, second]));
    }

    #[test]
    fn sessions_overview_filters_by_id_substring() {
        use crate::store::MemorySessionStore;

        let only = session(vec![Item::Assistant(assistant("go", Vec::new()))]);
        let store = MemorySessionStore::with_session(only.clone());

        let overview = sessions_overview(&store, Some(&only.id.to_string())).unwrap();
        assert_eq!(overview.rows.len(), 1);
        assert_eq!(overview.rows[0].id, only.id);

        let none = sessions_overview(&store, Some("no-such-id")).unwrap();
        assert!(none.rows.is_empty());
        assert_eq!(none.totals.turns, 0);
    }

    #[test]
    fn sessions_overview_folds_repairs_into_totals() {
        use crate::store::MemorySessionStore;

        let mut repaired = ok_result("a", "edited f.rs");
        repaired.effects = ToolEffects::none().with_repair("old_string matched after trimming");
        let repaired_session = session(vec![
            Item::Assistant(assistant(
                "go",
                vec![call("a", "edit_file", json!({"path": "f.rs"}))],
            )),
            Item::ToolResult(repaired),
        ]);
        let store = MemorySessionStore::with_session(repaired_session);

        let overview = sessions_overview(&store, None).unwrap();
        assert_eq!(overview.rows[0].stats.repairs, 1);
        // The fold must carry repairs too, or store-wide totals would
        // silently report zero.
        assert_eq!(overview.totals.repairs, 1);
    }

    /// A synthetic event stream exercising every mechanism-bearing
    /// kind — built in-test, no network, no provider.
    fn mechanism_events() -> Vec<EventEnvelope> {
        use crate::context::{PruneReport, PruneTrigger, PrunedCall, StubReason};
        use crate::event::EVENT_SCHEMA_VERSION;
        use crate::id::TaskId;

        let task = TaskId::new();
        let envelope = |sequence, kind| EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            sequence,
            task_id: task,
            kind,
        };
        vec![
            envelope(0, EventKind::TaskStarted),
            envelope(
                1,
                EventKind::ContextPruned {
                    turn: 4,
                    report: PruneReport {
                        trigger: PruneTrigger::AgeMask,
                        calls: vec![
                            PrunedCall {
                                call_id: "c1".to_owned(),
                                reason: StubReason::TurnAge,
                                reclaimed_tokens: 120,
                            },
                            PrunedCall {
                                call_id: "c2".to_owned(),
                                reason: StubReason::TurnAge,
                                reclaimed_tokens: 80,
                            },
                        ],
                        reclaimed_tokens: 200,
                    },
                },
            ),
            envelope(
                2,
                EventKind::FastPathHit {
                    kind: "rename".to_owned(),
                    files_changed: 2,
                },
            ),
            envelope(
                3,
                EventKind::ContextHandoff {
                    turn: 9,
                    chars: 4_000,
                },
            ),
            // Kinds with no mechanism attribution are ignored.
            envelope(4, EventKind::TaskCompleted),
        ]
    }

    #[test]
    fn session_mechanisms_counts_each_mechanism_honestly() {
        let mut stubbed = ok_result("b", "unchanged since call a");
        stubbed.effects = ToolEffects::none().with_repair("matched after trimming");
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![
                    call("a", "read_file", json!({"path": "x"})),
                    call("b", "read_file", json!({"path": "x"})),
                    call("c", "run_command", json!({"program": "git"})),
                ],
            )),
            Item::ToolResult(ok_result("a", "contents")),
            Item::ToolResult(stubbed),
            // One result carrying two markers: a byte cut and a
            // reducer's counted cap.
            Item::ToolResult(ok_result(
                "c",
                "commit 1\n[... 5 more commits omitted]\n... [300 bytes omitted] ...\ncommit 7",
            )),
        ];
        let stats = session_mechanisms(&session(items), &mechanism_events());

        // Event-derived: prunes with estimator-side tokens, handoff,
        // fast path.
        assert_eq!(stats.prune_events, 1);
        assert_eq!(stats.pruned_calls, 2);
        assert_eq!(stats.reclaimed_tokens_estimate, 200);
        assert_eq!(stats.handoffs, 1);
        assert_eq!(stats.fast_path_hits, 1);
        assert_eq!(stats.fast_path_files_changed, 2);
        // Transcript-derived: one stub hit, two markers, one repair.
        assert_eq!(stats.stub_hits, 1);
        assert_eq!(stats.omission_markers, 2);
        assert_eq!(stats.repairs, 1);
    }

    #[test]
    fn a_session_without_events_still_counts_transcript_mechanisms() {
        // Every session predating event persistence loads like this:
        // no stream, but the transcript's mechanisms still attribute.
        let items = vec![
            Item::Assistant(assistant(
                "go",
                vec![call("b", "read_file", json!({"path": "x"}))],
            )),
            Item::ToolResult(ok_result("b", "unchanged since call a")),
        ];
        let stats = session_mechanisms(&session(items), &[]);
        assert_eq!(stats.stub_hits, 1);
        assert_eq!(stats.omission_markers, 0);
        assert!(
            !stats.is_empty(),
            "the transcript mechanisms still attribute"
        );
    }

    #[test]
    fn mechanisms_overview_folds_across_the_store_and_filters() {
        use crate::store::{MemoryEventStore, MemorySessionStore};

        let events = MemoryEventStore::new();
        let store = MemorySessionStore::new();

        // Seed two sessions through the store's own seams, one with a
        // persisted stream and one without.
        let with_stream = SessionId::new();
        store.ensure_session(&with_stream).unwrap();
        events.ensure_session(&with_stream);
        for envelope in mechanism_events() {
            events.append_event(&with_stream, &envelope).unwrap();
        }
        let mut plain_session = Session::new(SessionId::new());
        store.ensure_session(&plain_session.id).unwrap();
        let planned = plain_session
            .plan_turn(
                assistant("go", vec![call("b", "read_file", json!({"path": "x"}))]),
                vec![ok_result("b", "unchanged since call a")],
            )
            .unwrap();
        store.append_turn(&plain_session.id, &planned).unwrap();
        plain_session.apply_planned_turn(planned);
        let plain = plain_session.id;

        let totals = mechanisms_overview(&store, &events, None).unwrap();
        assert_eq!(totals.prune_events, 1);
        assert_eq!(totals.reclaimed_tokens_estimate, 200);
        assert_eq!(totals.fast_path_hits, 1);
        // The stub hit from the event-less session folded in too.
        assert_eq!(totals.stub_hits, 1);

        // The filter narrows to the same id set `sessions_overview`
        // would keep.
        let only_plain = mechanisms_overview(&store, &events, Some(&plain.to_string())).unwrap();
        assert_eq!(only_plain.prune_events, 0);
        assert_eq!(only_plain.stub_hits, 1);

        let none = mechanisms_overview(&store, &events, Some("no-such-id")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn mechanisms_json_and_line_label_their_semantics() {
        let stats = session_mechanisms(&session(Vec::new()), &mechanism_events());
        let report = mechanisms_json(&stats);
        assert_eq!(report["prune_events"], 1);
        assert_eq!(report["reclaimed_tokens_estimate"], 200);
        assert_eq!(report["stub_hits"], 0);
        assert!(
            report["note"]
                .as_str()
                .expect("note")
                .contains("occurrence count"),
            "counted fields must be labeled as counts, not tokens"
        );

        let line = mechanisms_line(&stats);
        assert!(line.contains("~200 tok estimator-side"), "{line}");
        assert!(line.contains("1 fast-path solve"), "{line}");
        assert!(!line.contains("stub hit"), "{line}");

        assert_eq!(mechanisms_line(&MechanismStats::default()), "none observed");
    }
}
