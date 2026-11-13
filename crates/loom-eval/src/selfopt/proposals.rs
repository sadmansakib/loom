//! Candidate proposals: the Harness² candidate classes, the
//! deterministic rule proposer that seeds the loop with signal.

use serde::{Deserialize, Serialize};

use super::knobs::{KNOBS, Overlay, overlay_label, validate_overlay};

/// Harness²'s two candidate classes: an `Invariant` proposal claims a
/// context-management improvement that should hold across domains; a
/// `Domain` proposal is specific to the task class being probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalClass {
    /// Cross-domain (planning/tool-use/context-management) change.
    Invariant,
    /// Task-class-specific change.
    Domain,
}

/// One candidate: an overlay plus the claim behind it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// Deterministic label (also the probe directory name).
    pub label: String,
    /// The env overlay the probe runs with.
    pub overlay: Overlay,
    /// Harness² candidate class.
    pub class: ProposalClass,
    /// One sentence of what the proposer expects to change, in the
    /// trace (mechanism, not just direction).
    pub hypothesis: String,
    /// Why the proposer believes it — the evidence it read.
    pub rationale: String,
    /// `rule` or `model`.
    pub origin: String,
}

impl Proposal {
    /// Builds a rule proposal's [`Proposal::label`] via
    /// [`overlay_label`]; model proposals get labels the same way
    /// after validation.
    #[must_use]
    pub fn labeled(
        overlay: Overlay,
        class: ProposalClass,
        hypothesis: String,
        rationale: String,
        origin: &str,
    ) -> Self {
        let label = overlay_label(&overlay);
        Self {
            label,
            overlay,
            class,
            hypothesis,
            rationale,
            origin: origin.to_owned(),
        }
    }
}

/// Deterministic first-pass proposer: for every knob, halve and double
/// the shipped default, clamped to the knob's bounds. No-ops (the
/// clamp rounds back to the baseline value) are skipped. This exists
/// so the loop has signal before the model proposer earns trust, and
/// so regressions in the model path never leave the loop empty.
#[must_use]
pub fn rule_proposals(baseline: &Overlay, max: usize) -> Vec<Proposal> {
    let mut out = Vec::new();
    for knob in KNOBS {
        let current: u64 = baseline
            .get(knob.env)
            .and_then(|value| value.parse().ok())
            .unwrap_or(knob.default);
        let candidates = [current / 2, current.saturating_mul(2)];
        for (raw, direction) in candidates.into_iter().zip(["halve", "double"]) {
            let candidate = raw.clamp(knob.floor, knob.ceiling);
            if candidate == current {
                continue;
            }
            let mut overlay = baseline.clone();
            overlay.insert(knob.env.to_owned(), candidate.to_string());
            if validate_overlay(&overlay).is_err()
                || out.iter().any(|p: &Proposal| p.overlay == overlay)
            {
                continue;
            }
            let hypothesis = format!("{} {} → {}: {}", knob.env, current, candidate, knob.doc);
            let rationale = format!(
                "deterministic {direction} of the shipped default ({} {}), clamped to bounds",
                knob.default, knob.unit
            );
            out.push(Proposal::labeled(
                overlay,
                ProposalClass::Invariant,
                hypothesis,
                rationale,
                "rule",
            ));
            if out.len() >= max {
                return out;
            }
        }
    }
    out
}
