//! The model proposer: the system contract, the reduced prompt it
//! reads, and the tolerant parser for its JSON response.

use serde::Deserialize;

use super::baseline::Baseline;
use super::compare::InstanceStat;
use super::knobs::{KNOBS, Overlay, validate_overlay};
use super::ledger::Lesson;
use super::proposals::{Proposal, ProposalClass};

/// The model proposer's system contract: JSON in, JSON out, no
/// narration. Kept here so the prompt is reviewable and testable
/// alongside its parser.
pub const PROPOSER_SYSTEM_PROMPT: &str = "You are proposing harness-parameter changes for a coding agent you cannot see. You read reduced run statistics (tokens, events, turns, failures per instance) and retained lessons, then propose a small set of environment-variable overlays as your only output. Respond with a JSON array and nothing else. Each element: {\"overlay\": {\"LOOM_*\": value}, \"class\": \"invariant\"|\"domain\", \"hypothesis\": one sentence naming the mechanism you expect to change in the trace, \"rationale\": the statistic or lesson you read that supports it}. Rules: only keys from the provided knob table, values inside its bounds, integers; at most one overlay per knob; prefer few proposals with distinct mechanisms over many variations; never repeat an overlay the lessons already rejected.";

/// Builds the user prompt for the model proposer: the knob table, the
/// baseline aggregates, per-instance deltas worth seeing, and the
/// retained lessons — the full Actionable Side Information GEPA
/// prescribes, reduced to what the loop can defend.
pub fn build_proposal_prompt(
    baseline: &Baseline,
    stats: &[InstanceStat],
    lessons: &[Lesson],
) -> String {
    let knobs: Vec<serde_json::Value> = KNOBS
        .iter()
        .map(|knob| {
            serde_json::json!({
                "env": knob.env,
                "default": knob.default,
                "floor": knob.floor,
                "ceiling": knob.ceiling,
                "unit": knob.unit,
                "doc": knob.doc,
            })
        })
        .collect();
    let per_instance: Vec<serde_json::Value> = stats
        .iter()
        .map(|stat| {
            serde_json::json!({
                "instance": stat.instance,
                "median_input_tokens": stat.median_input_tokens,
                "median_events": stat.median_events,
                "median_turns": stat.median_turns,
                "failed_runs": stat.failed_runs,
                "runs": stat.runs,
            })
        })
        .collect();
    let lessons: Vec<serde_json::Value> = lessons
        .iter()
        .map(|lesson| {
            serde_json::json!({
                "statement": lesson.statement,
                "evidence": lesson.evidence,
            })
        })
        .collect();
    format!(
        "Baseline sweep: provider {} model {} arm {}.\n\nKnob table (the only keys you may set):\n{}\n\nPer-instance baseline medians:\n{}\n\nRetained lessons (contrast-supported only):\n{}\n\nPropose overlays as a JSON array.",
        baseline.provider,
        baseline.model,
        baseline.arm,
        serde_json::to_string_pretty(&knobs).unwrap_or_default(),
        serde_json::to_string_pretty(&per_instance).unwrap_or_default(),
        serde_json::to_string_pretty(&lessons).unwrap_or_default(),
    )
}

/// The model proposer's wire shape (raw JSON, before validation).
#[derive(Debug, Deserialize)]
struct RawProposal {
    overlay: Overlay,
    class: Option<String>,
    hypothesis: Option<String>,
    rationale: Option<String>,
}

/// Parses a model response into validated proposals: tolerant of code
/// fences and surrounding prose, dropping anything that fails
/// [`validate_overlay`] or duplicates an earlier overlay. Silent
/// drops are counted in the return so the binary can report drift.
pub fn parse_proposal_response(text: &str, max: usize) -> (Vec<Proposal>, usize) {
    let body = extract_json_array(text).unwrap_or_default();
    let Ok(raw) = serde_json::from_str::<Vec<RawProposal>>(&body) else {
        return (Vec::new(), 1);
    };
    let mut proposals = Vec::new();
    let mut dropped = 0usize;
    for item in raw {
        if proposals.len() >= max {
            dropped += 1;
            continue;
        }
        if validate_overlay(&item.overlay).is_err() {
            dropped += 1;
            continue;
        }
        if proposals
            .iter()
            .any(|p: &Proposal| p.overlay == item.overlay)
        {
            dropped += 1;
            continue;
        }
        let class = match item.class.as_deref() {
            Some("domain") => ProposalClass::Domain,
            _ => ProposalClass::Invariant,
        };
        let proposal = Proposal::labeled(
            item.overlay,
            class,
            item.hypothesis.unwrap_or_else(|| "unstated".to_owned()),
            item.rationale.unwrap_or_else(|| "unstated".to_owned()),
            "model",
        );
        proposals.push(proposal);
    }
    (proposals, dropped)
}

/// Finds the outermost JSON array in a response: direct parse first,
/// then fence-stripped, then the first `[` to the last `]`.
fn extract_json_array(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        return Some(trimmed.to_owned());
    }
    let unfenced = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.split_once('\n'))
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim);
    if let Some(body) = unfenced
        && body.starts_with('[')
    {
        return Some(body.to_owned());
    }
    let source = unfenced.unwrap_or(trimmed);
    let start = source.find('[')?;
    let end = source.rfind(']')?;
    (start < end).then(|| source[start..=end].to_owned())
}
