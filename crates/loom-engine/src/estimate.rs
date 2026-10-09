//! Real-tokenizer token estimation, per provider family.
//!
//! The kernel's [`TokenEstimator`] trait keeps estimation injectable and
//! the kernel tokenizer-free; this module is the engine's injection.
//! Two implementations ship:
//!
//! - [`O200kEstimator`] — the o200k_base BPE shared by the current
//!   generation of OpenAI-family (and DeepSeek/OpenRouter) models. It
//!   approximates DeepSeek's own tokenizer far better than the bytes ÷ 3
//!   heuristic (±30 % → single-digit error on code-shaped JSON).
//! - [`AnthropicEstimator`] — the same BPE scaled up for Anthropic,
//!   whose tokenizer is not published as a local vocabulary. Anthropic's
//!   4.7-and-later tokenizer counts roughly 30 % more tokens for the
//!   same text as the previous generation, and o200k runs low against
//!   Anthropic's counting on top of that, so the estimate is scaled by
//!   [`ANTHROPIC_TOKEN_SCALE_PER_MILLE`] and rounded **up**.
//!
//! The direction of the error is the whole point. Under-estimating makes
//! Loom send a request the provider rejects (or truncates) and turns a
//! manageable prompt into a mid-task failure; over-estimating only makes
//! the pruning compiler reclaim a little sooner. With a 1M window the
//! cost of headroom is negligible, so the Anthropic estimate deliberately
//! errs high.
//!
//! [`estimator_for`] is the selection point: one estimator per engine,
//! shared by the census, the budget check, and the pruning math.

use loom_kernel::context::TokenEstimator;
use std::sync::Arc;

/// Multiplier applied to the o200k count for Anthropic models.
///
/// `1_250` = 1.25× — covers the o200k-vs-Anthropic delta plus the 4.7+
/// tokenizer drift, without giving away a third of a 1M window. Applied
/// in per-mille so the arithmetic stays integer-only.
pub const ANTHROPIC_TOKEN_SCALE_PER_MILLE: u32 = 1_250;

/// Estimates tokens with the o200k_base BPE (`tiktoken-rs` singleton).
#[derive(Debug, Clone, Copy, Default)]
pub struct O200kEstimator;

impl TokenEstimator for O200kEstimator {
    fn estimate(&self, text: &str) -> u32 {
        // `encode_ordinary` never inspects special-token boundaries —
        // projection payloads are JSON, so there are none.
        let tokens = tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len();
        u32::try_from(tokens).unwrap_or(u32::MAX)
    }
}

/// Estimates Anthropic tokens: o200k scaled by
/// [`ANTHROPIC_TOKEN_SCALE_PER_MILLE`], rounded up.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicEstimator;

impl AnthropicEstimator {
    /// The scaled count, exposed for tests and for callers that need the
    /// figure without the trait.
    #[must_use]
    pub fn scaled(text: &str) -> u32 {
        let base = O200kEstimator.estimate(text) as u64;
        let scaled = base.saturating_mul(u64::from(ANTHROPIC_TOKEN_SCALE_PER_MILLE));
        // Ceiling division: never under-report.
        let scaled = scaled.div_ceil(1_000);
        u32::try_from(scaled).unwrap_or(u32::MAX)
    }
}

impl TokenEstimator for AnthropicEstimator {
    fn estimate(&self, text: &str) -> u32 {
        Self::scaled(text)
    }
}

/// Estimates tokens with the vocabulary family the provider bills.
///
/// Anthropic models count text differently from everything else Loom
/// speaks, so they get [`AnthropicEstimator`]; every other provider
/// (DeepSeek, OpenRouter's OpenAI-family backends) uses
/// [`O200kEstimator`]. Called once per engine open, before the compilers
/// are built.
#[must_use]
pub fn estimator_for(provider: &str) -> Arc<dyn TokenEstimator> {
    if provider == loom_providers::ANTHROPIC_PROVIDER_NAME {
        Arc::new(AnthropicEstimator)
    } else {
        Arc::new(O200kEstimator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A projection-shaped payload: JSON, code identifiers, no prose.
    const PAYLOAD: &str = r#"{"text":"You are Loom, a coding agent.","path":"src/lib.rs","content":"fn main() { println!(\"hi\"); }"}"#;

    #[test]
    fn code_shaped_json_estimates_below_the_heuristic() {
        // The heuristic bills ~3 bytes per token; o200k runs coarser on
        // JSON-heavy payloads like a serialized projection.
        let heuristic = loom_kernel::context::HeuristicEstimator.estimate(PAYLOAD);
        let o200k = O200kEstimator.estimate(PAYLOAD);
        assert!(o200k < heuristic, "o200k {o200k} vs heuristic {heuristic}");
        assert!(o200k > 0);
    }

    #[test]
    fn estimates_are_deterministic() {
        let text = "estimate me twice";
        assert_eq!(O200kEstimator.estimate(text), O200kEstimator.estimate(text));
        assert_eq!(
            AnthropicEstimator.estimate(text),
            AnthropicEstimator.estimate(text)
        );
    }

    #[test]
    fn anthropic_never_estimates_below_the_base_vocabulary() {
        // The safety property: the scaled estimate is >= the o200k count
        // for every input, so an Anthropic budget is never more
        // optimistic than an OpenAI-family one.
        for text in ["", "hi", PAYLOAD, "word ".repeat(500).as_str()] {
            let base = O200kEstimator.estimate(text);
            let anthropic = AnthropicEstimator.estimate(text);
            assert!(anthropic >= base, "{anthropic} < {base} for {text:?}");
        }
    }

    #[test]
    fn anthropic_scaling_is_a_ceiling_of_the_ratio() {
        // 1.25x rounded up, i.e. ceil(n * 5 / 4) computed in integers.
        assert_eq!(AnthropicEstimator::scaled(""), 0);
        let one = O200kEstimator.estimate("a");
        assert!(one > 0);
        let expected = u32::try_from((u64::from(one) * 5).div_ceil(4)).expect("fits");
        assert_eq!(AnthropicEstimator::scaled("a"), expected);
    }

    #[test]
    fn estimator_for_selects_by_provider() {
        let anthropic = estimator_for("anthropic");
        let deepseek = estimator_for("deepseek");
        let openrouter = estimator_for("openrouter");
        assert_eq!(
            anthropic.estimate(PAYLOAD),
            AnthropicEstimator.estimate(PAYLOAD)
        );
        assert_eq!(deepseek.estimate(PAYLOAD), O200kEstimator.estimate(PAYLOAD));
        assert_eq!(
            openrouter.estimate(PAYLOAD),
            O200kEstimator.estimate(PAYLOAD)
        );
    }
}
