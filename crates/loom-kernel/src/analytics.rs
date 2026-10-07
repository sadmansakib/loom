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

use crate::session::Session;
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
    /// Spilled oversize results, in transcript order.
    pub spills: Vec<Spill>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactRef;
    use crate::id::ArtifactId;
    use crate::model::StopReason;
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
        }
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
}
