//! The replay pre-probe: pricing a projection-affecting overlay
//! offline through engine replay before docker probes are spent.

use super::compare::{ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT, median};
use super::knobs::Overlay;

/// The verdict of replaying one proposal offline before any docker
/// probes are spent on it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreProbeOutcome {
    /// `false` means the proposal is rejected without probing.
    pub kept: bool,
    /// The projected median input-token improvement across usable
    /// traces, in percent — `None` when replay could not price the
    /// overlay at all.
    pub projected_improvement_pct: Option<f64>,
    /// Why: always printed by the binary, kept in sweep reports.
    pub note: String,
}

/// Offline pre-probe (replay-context integration): replays each trace
/// directory under the provider's baseline config and under the
/// proposal overlay, and rejects the proposal when the projected median
/// input-token improvement stays below the adoption gate
/// ([`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`]).
///
/// Contract:
///
/// - Overlays that cannot change the projection (budget-only knobs —
///   the recorded transcript is fixed) are kept with a note; replay
///   cannot price them, so the docker probe decides.
/// - A trace whose baseline arm overruns past rescue is skipped and
///   named (its overlay comparison would not be apples-to-apples).
/// - A proposal with no usable traces is kept: the pre-probe tightens
///   on evidence, it never overrides the loop on none.
/// - Same gate constant as the docker probe, so a pre-probe reject can
///   never disagree with what the probe would have required.
#[must_use]
pub fn pre_probe_proposal(
    overlay: &Overlay,
    traces: &[std::path::PathBuf],
    window_tokens: u32,
    reserved_output_tokens: u32,
) -> PreProbeOutcome {
    let pairs: Vec<String> = overlay
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !loom_core::engine::replay::overlay_affects_projection(&pairs) {
        return PreProbeOutcome {
            kept: true,
            projected_improvement_pct: None,
            note: "overlay does not affect the projection (budget-only \
                   knobs); replay cannot price it, the docker probe decides"
                .to_owned(),
        };
    }

    let mut improvements: Vec<f64> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for trace_dir in traces {
        let trace = match loom_core::engine::replay::TraceInput::read(trace_dir) {
            Ok(trace) => trace,
            Err(error) => {
                skipped.push(format!("{} (unreadable: {error})", trace_dir.display()));
                continue;
            }
        };
        let (base_config, _) = loom_core::engine::engine::prune_with_env(&trace.provider, |_| None);
        let mut options = loom_core::engine::replay::ReplayOptions::new(&trace.provider);
        options.window_tokens = window_tokens;
        options.reserved_output_tokens = reserved_output_tokens;
        let Ok(baseline) = loom_core::engine::replay::replay_session(
            &trace.session,
            &trace.task,
            &options,
            base_config,
        ) else {
            skipped.push(format!("{} (baseline replay failed)", trace_dir.display()));
            continue;
        };
        if baseline.handoff_stopped_at.is_some() {
            skipped.push(format!(
                "{} (baseline overran past rescue at turn {})",
                trace_dir.display(),
                baseline.handoff_stopped_at.unwrap_or_default(),
            ));
            continue;
        }
        let Ok((config, _)) = loom_core::engine::replay::overlay_config(base_config, &pairs) else {
            skipped.push(format!("{} (overlay rejected)", trace_dir.display()));
            continue;
        };
        let Ok(overlay_replay) = loom_core::engine::replay::replay_session(
            &trace.session,
            &trace.task,
            &options,
            config,
        ) else {
            skipped.push(format!("{} (overlay replay failed)", trace_dir.display()));
            continue;
        };
        improvements.push(loom_core::engine::replay::projected_improvement_percent(
            &baseline,
            &overlay_replay,
        ));
    }

    if improvements.is_empty() {
        let reason = if skipped.is_empty() {
            "no traces supplied".to_owned()
        } else {
            format!("no usable traces: {}", skipped.join("; "))
        };
        return PreProbeOutcome {
            kept: true,
            projected_improvement_pct: None,
            note: format!("kept without pre-probe evidence ({reason})"),
        };
    }
    let improvement = median(&improvements);
    PreProbeOutcome {
        kept: improvement >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT,
        projected_improvement_pct: Some(improvement),
        note: format!(
            "projected median input improvement {improvement:+.1}% across {} \
             trace(s){} (gate {ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT}%){}",
            improvements.len(),
            if skipped.is_empty() {
                String::new()
            } else {
                format!("; skipped {}", skipped.len())
            },
            if improvement >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT {
                ""
            } else {
                " — rejected before docker"
            },
        ),
    }
}
