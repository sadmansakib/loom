//! Tool and turn budgets, pruning configuration, and their
//! environment-variable seams.

use loom_kernel::agent::TurnBudget;
use loom_kernel::context::PruneConfig;
use loom_tools::ToolBudgets;

/// Pruning config for a provider — the one place reasoning elision is
/// provider-gated (roadmap item 12, deterministic half).
///
/// Replaying stored reasoning verbatim is a provider contract, not an
/// optimisation, on the OpenAI wire: DeepSeek rejects a tool-bearing
/// request whose prior assistant turns lack `reasoning_content`
/// (live-verified 2026-10-03), so reasoning elision stays off for the
/// OpenAI-vocabulary backends. GLM's Preserved Thinking merely gets
/// *better* cache hits when the client echoes reasoning — dropping old
/// traces forfeits that benefit for the affected turns but never fails
/// the request — and Anthropic never forwards reasoning on replay at
/// all, so for both the elision is a pure context win behind the same
/// episodic checkpoint freeze (one cache miss per aging batch).
pub(crate) fn prune_config_for(provider: &str) -> PruneConfig {
    let mut config = PruneConfig::default();
    if provider == loom_providers::GLM_PROVIDER_NAME
        || provider == loom_providers::ANTHROPIC_PROVIDER_NAME
    {
        config.elide_reasoning = Some(8);
    }
    config
}

/// The `LOOM_*` budget knobs [`budgets_with_env`] reads, in struct
/// order. Exported so a frontend can echo which overrides a run
/// actually saw (sweep logs stay honest about what varied).
pub const BUDGET_ENV_VARS: [&str; 9] = [
    "LOOM_READ_HEAD_BYTES",
    "LOOM_COMMAND_HEAD_BYTES",
    "LOOM_COMMAND_TAIL_BYTES",
    "LOOM_EDIT_ECHO_MAX_BYTES",
    "LOOM_EDIT_ECHO_LINES",
    "LOOM_SYMBOLS_OVERVIEW_MAX_BYTES",
    "LOOM_SKILL_HEAD_BYTES",
    "LOOM_REPLACE_SYMBOL_HEAD_BYTES",
    "LOOM_MCP_IDLE_EVICT_TURNS",
];

/// The `LOOM_*` staged-turn-budget knobs [`turn_budget_with_env`] reads,
/// in struct order (roadmap item 26).
pub const TURN_BUDGET_ENV_VARS: [&str; 3] = [
    "LOOM_INITIAL_TURNS",
    "LOOM_EXTENSION_TURNS",
    "LOOM_MAX_TURN_EXTENSIONS",
];

/// Applies `LOOM_*` staged-turn-budget overrides (roadmap item 26) over
/// the inert default. Same purity and warning contract as
/// [`budgets_with_env`]. `LOOM_INITIAL_TURNS=0` is accepted — it means
/// "the full ceiling" — while zero values for the other two are
/// ignored, matching the never-disable-by-typo convention.
pub fn turn_budget_with_env(
    base: TurnBudget,
    get: impl Fn(&str) -> Option<String>,
) -> (TurnBudget, Vec<String>) {
    let mut budget = base;
    let mut warnings = Vec::new();
    apply_env_overrides(
        [
            (&mut budget.initial_turns, TURN_BUDGET_ENV_VARS[0], true),
            (&mut budget.extension_turns, TURN_BUDGET_ENV_VARS[1], false),
            (&mut budget.max_extensions, TURN_BUDGET_ENV_VARS[2], false),
        ],
        &get,
        &mut warnings,
    );
    (budget, warnings)
}

/// The `LOOM_PRUNE_*` knobs [`prune_with_env`] reads, in struct order.
pub const PRUNE_ENV_VARS: [&str; 5] = [
    "LOOM_PRUNE_TRIGGER_PERCENT",
    "LOOM_PRUNE_FLOOR_TOKENS",
    "LOOM_PRUNE_MIN_RECLAIM_TOKENS",
    "LOOM_PRUNE_TAIL_TOKENS",
    "LOOM_PHASE_MASK_TURNS",
];

/// Applies typed positive environment overrides; zero is allowed for
/// settings where it has an explicit meaning.
fn apply_env_overrides<T, const N: usize>(
    overrides: [(&mut T, &str, bool); N],
    get: &impl Fn(&str) -> Option<String>,
    warnings: &mut Vec<String>,
) where
    T: std::str::FromStr + PartialOrd + Default,
{
    for (slot, name, allow_zero) in overrides {
        let Some(raw) = get(name) else { continue };
        match raw.trim().parse::<T>() {
            Ok(value) if allow_zero || value > T::default() => *slot = value,
            _ => warnings.push(format!("ignoring invalid {name}={raw:?}")),
        }
    }
}

/// Applies `LOOM_*` per-tool budget overrides (roadmap item 20.2) over
/// `base`. Pure — the caller supplies the env reader — so the frozen
/// SWE-bench container script (which sets none of these) is unaffected,
/// and tests drive it without touching process env. Invalid or
/// zero-valued entries are ignored and reported as warning lines for the
/// frontend to print.
pub fn budgets_with_env(
    base: ToolBudgets,
    get: impl Fn(&str) -> Option<String>,
) -> (ToolBudgets, Vec<String>) {
    let mut budgets = base;
    let mut warnings = Vec::new();
    apply_env_overrides(
        [
            (&mut budgets.read_file_head_bytes, BUDGET_ENV_VARS[0], false),
            (&mut budgets.command_head_bytes, BUDGET_ENV_VARS[1], false),
            (&mut budgets.command_tail_bytes, BUDGET_ENV_VARS[2], false),
            (&mut budgets.edit_echo_max_bytes, BUDGET_ENV_VARS[3], false),
            (
                &mut budgets.edit_echo_context_lines,
                BUDGET_ENV_VARS[4],
                false,
            ),
            (
                &mut budgets.symbols_overview_max_bytes,
                BUDGET_ENV_VARS[5],
                false,
            ),
            (
                &mut budgets.skill_body_head_bytes,
                BUDGET_ENV_VARS[6],
                false,
            ),
            (
                &mut budgets.replace_symbol_head_bytes,
                BUDGET_ENV_VARS[7],
                false,
            ),
        ],
        &get,
        &mut warnings,
    );
    // `0` = "never evict" is a code-level policy, not an env knob, so
    // keep the ignore-invalid-and-zero convention for this u32 slot too.
    apply_env_overrides(
        [(&mut budgets.mcp_idle_evict_turns, BUDGET_ENV_VARS[8], false)],
        &get,
        &mut warnings,
    );
    (budgets, warnings)
}

/// Applies `LOOM_PRUNE_*` overrides (roadmap item 20.2) over the
/// per-provider base from [`prune_config_for`], preserving its
/// provider-gated reasoning elision. Same purity and warning contract
/// as [`budgets_with_env`].
pub fn prune_with_env(
    provider: &str,
    get: impl Fn(&str) -> Option<String>,
) -> (PruneConfig, Vec<String>) {
    let mut config = prune_config_for(provider);
    let mut warnings = Vec::new();
    apply_env_overrides(
        [
            (&mut config.trigger_percent, PRUNE_ENV_VARS[0], false),
            (&mut config.floor_tokens, PRUNE_ENV_VARS[1], false),
            (&mut config.min_reclaim_tokens, PRUNE_ENV_VARS[2], false),
            (&mut config.protected_tail_tokens, PRUNE_ENV_VARS[3], false),
        ],
        &get,
        &mut warnings,
    );
    // Option<usize> slot: any positive value enables phase-aware
    // retention with that window; 0/invalid warn and stay off.
    if let Some(raw) = get(PRUNE_ENV_VARS[4]) {
        match raw.trim().parse::<usize>() {
            Ok(value) if value > 0 => config.phase_mask_turns = Some(value),
            _ => warnings.push(format!("ignoring invalid {}={raw:?}", PRUNE_ENV_VARS[4])),
        }
    }
    (config, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_budget_with_env_layers_over_the_inert_default() {
        // Zero initial turns is the "full ceiling" default, not an error.
        let (budget, warnings) = turn_budget_with_env(TurnBudget::default(), |name| match name {
            "LOOM_INITIAL_TURNS" => Some("0".to_owned()),
            "LOOM_EXTENSION_TURNS" => Some("8".to_owned()),
            "LOOM_MAX_TURN_EXTENSIONS" => Some("2".to_owned()),
            _ => None,
        });
        assert_eq!(
            budget,
            TurnBudget {
                initial_turns: 0,
                extension_turns: 8,
                max_extensions: 2
            }
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // A zero extension would disable the policy silently — ignored.
        let (_, warnings) = turn_budget_with_env(TurnBudget::default(), |name| match name {
            "LOOM_EXTENSION_TURNS" => Some("0".to_owned()),
            _ => None,
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn budgets_with_env_applies_valid_and_warns_on_invalid() {
        let (budgets, warnings) = budgets_with_env(ToolBudgets::default(), |name| match name {
            "LOOM_READ_HEAD_BYTES" => Some("128".to_owned()),
            "LOOM_EDIT_ECHO_LINES" => Some(" 5 ".to_owned()),
            "LOOM_SKILL_HEAD_BYTES" => Some("zero".to_owned()),
            "LOOM_COMMAND_TAIL_BYTES" => Some("0".to_owned()),
            _ => None,
        });
        assert_eq!(budgets.read_file_head_bytes, 128);
        assert_eq!(budgets.edit_echo_context_lines, 5);
        assert_eq!(
            budgets.command_tail_bytes,
            ToolBudgets::default().command_tail_bytes
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("LOOM_SKILL_HEAD_BYTES")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("LOOM_COMMAND_TAIL_BYTES"))
        );
    }

    #[test]
    fn prune_with_env_layers_over_provider_base() {
        // GLM keeps its elide-reasoning default; only the swept knobs move.
        let (prune, warnings) =
            prune_with_env(loom_providers::GLM_PROVIDER_NAME, |name| match name {
                "LOOM_PRUNE_TRIGGER_PERCENT" => Some("50".to_owned()),
                "LOOM_PRUNE_TAIL_TOKENS" => Some("bad".to_owned()),
                _ => None,
            });
        assert_eq!(prune.trigger_percent, 50);
        assert_eq!(prune.elide_reasoning, Some(8));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
