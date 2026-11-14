//! The counted data model of transcript analytics: per-tool counters,
//! edit acceptance, spill records, and the foldable [`SessionStats`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::skills::SkillStats;
use crate::transcript::{ToolFailure, ToolStatus};

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
    /// `Declined` results (operator refused at the approval gate).
    pub declined: u64,
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
    pub(crate) fn record(&mut self, status: ToolStatus) {
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
                    ToolFailure::Declined => self.declined += 1,
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
        self.declined += other.declined;
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
    /// Per-skill `load_skill` counters, keyed by menu name (skill
    /// telemetry). `serde(default)` keeps JSON from before this field
    /// loadable.
    #[serde(default)]
    pub skills: BTreeMap<String, SkillStats>,
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
    /// Adds `other` into `self`: counts sum, per-tool and per-skill
    /// maps merge, spills concatenate in order. The store-wide totals
    /// of a [`sessions_overview`](super::sessions_overview) are folded
    /// with this.
    pub fn add(&mut self, other: &SessionStats) {
        self.turns += other.turns;
        self.identical_repeats += other.identical_repeats;
        self.repairs += other.repairs;
        self.edit_apply.attempts += other.edit_apply.attempts;
        self.edit_apply.applied += other.edit_apply.applied;
        for (name, counts) in &other.skills {
            self.skills.entry(name.clone()).or_default().add(counts);
        }
        self.malformed_calls += other.malformed_calls;
        for (name, counts) in &other.tools {
            self.tools.entry(name.clone()).or_default().add(counts);
        }
        self.totals.add(&other.totals);
        self.spills.extend(other.spills.iter().cloned());
    }
}
