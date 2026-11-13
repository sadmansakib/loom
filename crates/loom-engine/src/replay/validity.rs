//! The self-validity check: replaying recorded ground truth under its
//! own config must reproduce it, or the tool says so.

use loom_kernel::context::PruneReport;

use super::options::ReplaySummary;
use super::trace::TraceInput;

/// The self-validity check: replaying recorded ground truth under its own
/// original config must reproduce it, or the tool says so.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidityReport {
    /// Set when the trace's event schema predates this binary: the
    /// recorded census came from an older compiler generation, so the
    /// census and prune comparisons are not assertable (turn structure
    /// still is — the transcript shape is stable across schemas).
    pub schema_skew: Option<(u16, u16)>,
    /// `None` when no ground truth was available to compare.
    pub turns_match: Option<bool>,
    /// Per-source census comparison (exact equality required on
    /// transcript-derived sources).
    pub census_match: Option<bool>,
    /// Prune-event comparison (freeze turns, triggers, reclaimed totals).
    pub prunes_match: Option<bool>,
    /// Overall verdict; `false` makes every downstream number untrusted.
    pub valid: bool,
    /// Human-readable deltas for anything that did not match.
    pub notes: Vec<String>,
}

impl ValidityReport {
    /// Compares a replay arm against a [`TraceInput`]'s ground truth.
    ///
    /// Transcript-derived census sources must match exactly; `total` and
    /// the reconstructed prefix sources (system prompt, facts, tool
    /// specs) are informational because traces do not record them.
    #[must_use]
    pub fn check(trace: &TraceInput, replay: &ReplaySummary) -> Self {
        let mut notes = Vec::new();
        let binary_schema = loom_kernel::event::EVENT_SCHEMA_VERSION;
        let schema_skew = if trace.schema_version == binary_schema {
            None
        } else {
            notes.push(format!(
                "trace recorded under event schema {} but this binary is {}: the \
                 recorded census came from an older compiler generation, so census \
                 and prune comparisons are not assertable (record a fresh trace to \
                 validate); turn structure is still compared",
                trace.schema_version, binary_schema
            ));
            Some((trace.schema_version, binary_schema))
        };
        let turns_match = trace.summary.as_ref().map(|summary| {
            #[allow(clippy::cast_possible_truncation)] // recorded values are loom-written counts
            let recorded = summary["turns"].as_u64().unwrap_or_default() as u32;
            #[allow(clippy::cast_possible_truncation)] // turn counts stay far below u32::MAX
            let ok = replay.turns.len() as u32 == recorded;
            if !ok {
                notes.push(format!(
                    "turns: replay {} vs recorded {recorded}",
                    replay.turns.len()
                ));
            }
            ok
        });
        let census_match = if schema_skew.is_some() {
            None
        } else {
            trace.summary.as_ref().map(|summary| {
                let recorded = &summary["last_context_census"];
                let Some(last) = replay.turns.last() else {
                    notes.push("census: replay produced no turns".into());
                    return false;
                };
                let fields = [
                    ("user_items", last.census.user_items),
                    ("assistant_history", last.census.assistant_history),
                    ("reasoning_replay", last.census.reasoning_replay),
                    ("tool_results", last.census.tool_results),
                ];
                let mut ok = true;
                for (name, value) in fields {
                    #[allow(clippy::cast_possible_truncation)]
                    // recorded values are loom-written counts
                    let recorded_value = recorded[name].as_u64().unwrap_or_default() as u32;
                    if recorded_value != value {
                        ok = false;
                        notes.push(format!(
                            "census.{name}: replay {value} vs recorded {recorded_value}"
                        ));
                    }
                }
                ok
            })
        };
        let prunes_match =
            if trace.prune_events.is_empty() && replay.freezes == 0 || schema_skew.is_some() {
                None
            } else {
                Some(compare_prunes(trace, replay, &mut notes))
            };
        let valid = turns_match.unwrap_or(true)
            && census_match.unwrap_or(true)
            && prunes_match.unwrap_or(true);
        Self {
            schema_skew,
            turns_match,
            census_match,
            prunes_match,
            valid,
            notes,
        }
    }
}

/// Compares the replay arm's prune freezes against the trace's
/// recorded prune events: counts must match, then each freeze's turn,
/// trigger, and reclaimed tokens.
fn compare_prunes(trace: &TraceInput, replay: &ReplaySummary, notes: &mut Vec<String>) -> bool {
    let freezes: Vec<(u32, &PruneReport)> = replay
        .turns
        .iter()
        .filter_map(|turn| turn.froze.as_ref().map(|report| (turn.turn, report)))
        .collect();
    if freezes.len() != trace.prune_events.len() {
        notes.push(format!(
            "prune events: replay {} vs recorded {}",
            freezes.len(),
            trace.prune_events.len()
        ));
        return false;
    }
    let mut ok = true;
    for ((replay_turn, replay_report), (event_turn, event_report)) in
        freezes.iter().zip(&trace.prune_events)
    {
        if replay_turn != event_turn
            || replay_report.trigger != event_report.trigger
            || replay_report.reclaimed_tokens != event_report.reclaimed_tokens
        {
            ok = false;
            notes.push(format!(
                "prune at turn {event_turn}: replay (turn {replay_turn}, {:?}, \
                 {} tok) vs recorded ({:?}, {} tok)",
                replay_report.trigger,
                replay_report.reclaimed_tokens,
                event_report.trigger,
                event_report.reclaimed_tokens
            ));
        }
    }
    ok
}
