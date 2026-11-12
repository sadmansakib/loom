//! Overlay-key parsing and the projection statistics the eval harness
//! diffs baseline and overlay replays with.

use loom_kernel::context::PruneConfig;

use super::options::ReplaySummary;

/// Parses `KEY=VAL` overlay pairs into a config on top of `base`.
///
/// Accepted keys are the five selfopt prune seams (by `LOOM_*` env name or
/// bare name) plus replay-only compiler knobs selfopt does not env-sweep
/// (`age_mask_turns`, `elide_turns`, `elide_reasoning`,
/// `elide_assistant_text`). Optional knobs take `off` (or `0`) to
/// disable. Unknown keys are an error — a silently ignored key would make
/// every delta a lie.
pub fn overlay_config(
    base: PruneConfig,
    pairs: &[String],
) -> Result<(PruneConfig, Vec<String>), String> {
    let mut config = base;
    let mut applied = Vec::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("overlay `{pair}` is not KEY=VAL"));
        };
        let key = key.trim();
        let value = value.trim();
        let bare = key.strip_prefix("LOOM_").unwrap_or(key).to_lowercase();
        match bare.as_str() {
            "prune_trigger_percent" => config.trigger_percent = positive(value, key)?,
            "prune_floor_tokens" => config.floor_tokens = positive(value, key)?,
            "prune_min_reclaim_tokens" => config.min_reclaim_tokens = positive(value, key)?,
            "prune_tail_tokens" => config.protected_tail_tokens = positive(value, key)?,
            "phase_mask_turns" => config.phase_mask_turns = optional(value, key)?,
            "age_mask_turns" => config.age_mask_turns = optional(value, key)?,
            "elide_turns" => config.elide_turns = optional(value, key)?,
            "elide_reasoning" => config.elide_reasoning = optional(value, key)?,
            "elide_assistant_text" => config.elide_assistant_text = optional(value, key)?,
            _ => {
                return Err(format!(
                    "unknown overlay key `{key}` (known: LOOM_PRUNE_TRIGGER_PERCENT, \
                     LOOM_PRUNE_FLOOR_TOKENS, LOOM_PRUNE_MIN_RECLAIM_TOKENS, \
                     LOOM_PRUNE_TAIL_TOKENS, LOOM_PHASE_MASK_TURNS, LOOM_AGE_MASK_TURNS, \
                     LOOM_ELIDE_TURNS, LOOM_ELIDE_REASONING, LOOM_ELIDE_ASSISTANT_TEXT)"
                ));
            }
        }
        applied.push(format!("{key}={value}"));
    }
    Ok((config, applied))
}

/// Whether an overlay touches any key that changes the compiled
/// projection (selfopt budget knobs do not — the recorded transcript is
/// fixed, so budgets cannot change a replay).
#[must_use]
pub fn overlay_affects_projection(pairs: &[String]) -> bool {
    pairs.iter().any(|pair| {
        let key = pair.split_once('=').map_or(pair.as_str(), |(key, _)| key);
        let bare = key
            .trim()
            .strip_prefix("LOOM_")
            .unwrap_or(key.trim())
            .to_lowercase();
        matches!(
            bare.as_str(),
            "prune_trigger_percent"
                | "prune_floor_tokens"
                | "prune_min_reclaim_tokens"
                | "prune_tail_tokens"
                | "phase_mask_turns"
                | "age_mask_turns"
                | "elide_turns"
                | "elide_reasoning"
                | "elide_assistant_text"
        )
    })
}

/// The projected median input-token improvement of an overlay over a
/// baseline, in percent — the same statistic the selfopt adoption gate
/// computes on billed usage. Positive means the overlay is cheaper.
#[must_use]
pub fn projected_improvement_percent(baseline: &ReplaySummary, overlay: &ReplaySummary) -> f64 {
    if baseline.median_input_tokens == 0.0 {
        return 0.0;
    }
    (baseline.median_input_tokens - overlay.median_input_tokens) / baseline.median_input_tokens
        * 100.0
}

/// Median of a slice (0.0 empty); mirrors the selfopt helper without
/// taking a loom-eval dependency (dependency direction is one-way).
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// Parses a strictly positive integer overlay value.
fn positive(value: &str, key: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|error| format!("overlay `{key}`: `{value}` is not a number ({error})"))
        .and_then(|parsed| {
            if parsed == 0 {
                Err(format!("overlay `{key}` must be > 0"))
            } else {
                Ok(parsed)
            }
        })
}

/// Parses an optional-window overlay value: a number turns the window on
/// at that size, `off` (or `0`) disables it.
fn optional(value: &str, key: &str) -> Result<Option<usize>, String> {
    if value.eq_ignore_ascii_case("off") || value == "0" {
        return Ok(None);
    }
    value.parse::<usize>().map(Some).map_err(|error| {
        format!("overlay `{key}`: `{value}` is neither a number nor `off` ({error})")
    })
}
