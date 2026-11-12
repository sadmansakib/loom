//! Turn budgets and the task resource envelope (§14, roadmap item 26):
//! the staged turn budget, the output-token reservation, and the
//! token/tool/wall-time ceilings checked once per turn.

use super::feedback::CompletionGate;
use super::summary::Accounting;
use crate::event::TurnBudgetReason;
use crate::model::ModelCapabilities;
use std::time::Duration;

/// Staged turn budget (roadmap item 26): `max_turns` stays the hard
/// ceiling; the loop starts with `initial_turns` (0 = the full ceiling,
/// the frozen default) and may extend by `extension_turns` when a turn
/// boundary shows deterministic progress evidence. Grants are capped by
/// `max_extensions`, so the policy spends *earlier*, never more than
/// the ceiling allows.
///
/// Inert by default (`0, 0, 0`): the loop runs `1..=max_turns` exactly
/// as before, byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnBudget {
    /// Turns granted up front; 0 = `max_turns` (the previous behavior).
    pub initial_turns: u32,
    /// Turns added per extension grant; 0 disables extensions.
    pub extension_turns: u32,
    /// Maximum extension grants per task.
    pub max_extensions: u32,
}

/// Whether the boundary before `turn` may extend the effective budget,
/// and on what evidence (roadmap item 26). `None` stops the run: no
/// grant room, or no progress signal. The evidence is state the loop
/// already keeps — a fresh mutation, or unverified work inside the
/// completion gate — never a model call.
pub(super) fn turn_extension_reason(
    budget: &TurnBudget,
    extensions: u32,
    effective: u32,
    hard: u32,
    completion: &CompletionGate,
    mutations_at_grant: u64,
) -> Option<TurnBudgetReason> {
    if budget.extension_turns == 0 || extensions >= budget.max_extensions || effective >= hard {
        return None;
    }
    if completion.mutations > mutations_at_grant {
        Some(TurnBudgetReason::MutationAtLimit)
    } else if completion.pending() {
        Some(TurnBudgetReason::UnverifiedWork)
    } else {
        None
    }
}

/// Default output-token reservation: the share of the context window held
/// back for the model's reply (§8). Used verbatim when the model reports
/// no output limit, and as the floor of the automatic derivation
/// ([`OutputReservation::Auto`]) otherwise. Configurable via
/// [`Agent::with_reserved_output_tokens`].
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 8_192;

/// Hard ceiling on a single model turn's output reservation, whatever the
/// model declares. Task budgets (§14) bound aggregate spend; this only
/// shapes individual turns, so a model advertising a 200K-token output
/// ceiling cannot dump it all in one go.
pub const MAX_RESERVED_OUTPUT_TOKENS: u32 = 32_768;

/// How the per-turn output reservation is chosen (§8).
///
/// `Auto` (the default) derives it from the model's capabilities: the
/// model's declared output maximum when it reports one, floored at
/// [`DEFAULT_RESERVED_OUTPUT_TOKENS`] and capped at
/// [`MAX_RESERVED_OUTPUT_TOKENS`], then clamped to half the context
/// window so the projection always keeps room for input. `Fixed` pins an
/// explicit value — still window-clamped, since a reservation that
/// starves input is a misconfiguration, not a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputReservation {
    /// Derive from model capabilities (default).
    Auto,
    /// Explicit reservation, clamped to half the context window.
    Fixed(u32),
}

/// Resolves the output reservation actually used for a task's requests.
///
/// The derived value feeds [`ContextBudget::from_window`] and bounds
/// `max_tokens` on each request (the compiler applies the model's own
/// output limit on top, so a smaller declared maximum always wins).
#[must_use]
pub fn derive_reserved_output_tokens(
    capabilities: &ModelCapabilities,
    reservation: OutputReservation,
) -> u32 {
    let window_half = capabilities.max_context_tokens / 2;
    match reservation {
        OutputReservation::Fixed(reserved) => reserved.min(window_half),
        OutputReservation::Auto => DEFAULT_RESERVED_OUTPUT_TOKENS
            .max(capabilities.max_output_tokens.unwrap_or(0))
            .min(MAX_RESERVED_OUTPUT_TOKENS)
            .min(window_half),
    }
}

/// A task's resource envelope (§14).
///
/// Every ceiling is optional: `None` means the resource is unbounded, which
/// is the default. The loop checks the envelope once per turn, before
/// compiling the next request, so a ceiling can be exceeded by at most the
/// unit of work in flight when it was crossed — one model response, or one
/// tool batch. A task that completes inside that turn still succeeds.
///
/// The turn ceiling is separate: it stays [`Agent::with_max_turns`], because
/// it is a loop-shape knob rather than a cost bound.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskBudget {
    /// Ceiling on prompt (input) tokens summed over the task.
    pub max_input_tokens: Option<u64>,
    /// Ceiling on generated (output) tokens summed over the task.
    pub max_output_tokens: Option<u64>,
    /// Ceiling on `input + output` tokens summed over the task.
    pub max_total_tokens: Option<u64>,
    /// Ceiling on tool calls the loop executes. Refused repeats of a failed
    /// call do not run and do not count.
    pub max_tool_calls: Option<u32>,
    /// Ceiling on the task's wall-clock duration.
    pub max_wall_time: Option<Duration>,
}

impl TaskBudget {
    /// An envelope with no ceilings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the input-token ceiling.
    #[must_use]
    pub fn with_max_input_tokens(mut self, max_input_tokens: u64) -> Self {
        self.max_input_tokens = Some(max_input_tokens);
        self
    }

    /// Sets the output-token ceiling.
    #[must_use]
    pub fn with_max_output_tokens(mut self, max_output_tokens: u64) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// Sets the total-token ceiling (`input + output`).
    #[must_use]
    pub fn with_max_total_tokens(mut self, max_total_tokens: u64) -> Self {
        self.max_total_tokens = Some(max_total_tokens);
        self
    }

    /// Sets the tool-call ceiling.
    #[must_use]
    pub fn with_max_tool_calls(mut self, max_tool_calls: u32) -> Self {
        self.max_tool_calls = Some(max_tool_calls);
        self
    }

    /// Sets the wall-clock ceiling.
    #[must_use]
    pub fn with_max_wall_time(mut self, max_wall_time: Duration) -> Self {
        self.max_wall_time = Some(max_wall_time);
        self
    }

    /// The name of the first exhausted limit, if any.
    ///
    /// Deterministic order so the reported [`TaskError::BudgetExhausted`] is
    /// stable when several limits are reached at once: total tokens, then
    /// input, then output, then tool calls, then wall time.
    pub(super) fn exhausted(
        &self,
        accounting: &Accounting,
        elapsed: Duration,
    ) -> Option<&'static str> {
        let limits = [
            (
                self.max_total_tokens,
                accounting
                    .usage
                    .input_tokens
                    .saturating_add(accounting.usage.output_tokens),
                "total_tokens",
            ),
            (
                self.max_input_tokens,
                accounting.usage.input_tokens,
                "input_tokens",
            ),
            (
                self.max_output_tokens,
                accounting.usage.output_tokens,
                "output_tokens",
            ),
            (
                self.max_tool_calls.map(u64::from),
                u64::from(accounting.tool_calls()),
                "tool_calls",
            ),
        ];
        for (max, used, name) in limits {
            if let Some(max) = max
                && used >= max
            {
                return Some(name);
            }
        }
        if let Some(max) = self.max_wall_time
            && elapsed >= max
        {
            return Some("wall_time");
        }
        None
    }

    /// Headroom left after `accounting` and `elapsed`.
    pub(super) fn remaining(&self, accounting: &Accounting, elapsed: Duration) -> RemainingBudget {
        let rem_u64 = |max: Option<u64>, used: u64| max.map(|max| max.saturating_sub(used));
        RemainingBudget {
            input_tokens: rem_u64(self.max_input_tokens, accounting.usage.input_tokens),
            output_tokens: rem_u64(self.max_output_tokens, accounting.usage.output_tokens),
            total_tokens: rem_u64(
                self.max_total_tokens,
                accounting
                    .usage
                    .input_tokens
                    .saturating_add(accounting.usage.output_tokens),
            ),
            tool_calls: self
                .max_tool_calls
                .map(|max| max.saturating_sub(accounting.tool_calls())),
            wall_time: self.max_wall_time.map(|max| max.saturating_sub(elapsed)),
        }
    }
}

/// Budget headroom when a task ended, surfaced in [`TaskSummary`].
///
/// Each field is `None` when the corresponding ceiling was not set, and
/// `Some(0)` when the ceiling was reached (or passed).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemainingBudget {
    /// Input tokens still allowed, when bounded.
    pub input_tokens: Option<u64>,
    /// Output tokens still allowed, when bounded.
    pub output_tokens: Option<u64>,
    /// Total tokens still allowed, when bounded.
    pub total_tokens: Option<u64>,
    /// Tool calls still allowed, when bounded.
    pub tool_calls: Option<u32>,
    /// Wall time still allowed, when bounded.
    pub wall_time: Option<Duration>,
}
