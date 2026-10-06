//! Real-tokenizer token estimation (o200k BPE).
//!
//! The kernel's [`TokenEstimator`] trait keeps estimation injectable and
//! the kernel tokenizer-free; this module is the engine's injection: the
//! o200k vocabulary shared by the current generation of OpenAI-family
//! models. It approximates DeepSeek's own tokenizer far better than the
//! bytes ÷ 3 heuristic (±30 % → single-digit error on code-shaped JSON)
//! and is a drop-in swap per provider family if billing ever demands
//! exact counts.

use loom_kernel::context::TokenEstimator;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_shaped_json_estimates_below_the_heuristic() {
        // The heuristic bills ~3 bytes per token; o200k runs coarser on
        // JSON-heavy payloads like a serialized projection.
        let payload = r#"{"text":"You are Loom, a coding agent.","path":"src/lib.rs"}"#;
        let heuristic = loom_kernel::context::HeuristicEstimator.estimate(payload);
        let o200k = O200kEstimator.estimate(payload);
        assert!(o200k < heuristic, "o200k {o200k} vs heuristic {heuristic}");
        assert!(o200k > 0);
    }

    #[test]
    fn estimates_are_deterministic() {
        let text = "estimate me twice";
        assert_eq!(O200kEstimator.estimate(text), O200kEstimator.estimate(text));
    }
}
