//! Trigger-gated deterministic context pruning (2L, ADR-011).
//!
//! [`PruningContextCompiler`] wraps another compiler (the shipped
//! [`DefaultContextCompiler`](super::DefaultContextCompiler)) and, once the
//! projected input crosses an absolute threshold, replaces *older* tool
//! results with informative stubs so the model keeps working inside its
//! budget. The transcript is never mutated — only this request's projection
//! changes, and every pruned result keeps its `call_id` plus a recovery
//! instruction, so the model can re-run the call if it needs the content
//! again.
//!
//! Determinism and cache behavior:
//!
//! * Decisions are computed only when the trigger fires, then **frozen** in
//!   a [`PruneCheckpoint`](super::PruneCheckpoint) that the caller passes back in on later turns.
//!   Between triggers the projection is byte-stable, so the provider's
//!   prefix cache holds; each trigger costs one cache miss (ADR-011).
//! * Everything is derived from the session transcript with fixed rules —
//!   no I/O, no clocks, no model calls.
//!
//! Never pruned by the result-stubbing strategies: user items,
//! assistant messages, and the protected tail — the last
//! `protected_tail_turns` turns *and* at least
//! `protected_tail_tokens` estimated tokens of trailing items
//! (whichever reaches further back). History elision (below) touches
//! old call arguments, non-first user items, and — when
//! `elide_reasoning` is enabled — old assistant reasoning traces; it
//! never touches assistant text.
//!
//! # History elision (roadmap item 12, deterministic half)
//!
//! Result stubbing alone leaves two buckets stacking verbatim every
//! turn: tool-call *arguments* (an edit call carries the whole new file
//! content — 28 % of context tokens on CliffCompaction's Terminal-Bench
//! unpacking) and old non-first *user items*. When `elide_turns` is
//! set (default 8), the same episodic machinery reduces them below the
//! window trigger:
//!
//! * old calls keep their signature — name, id, short strings (paths,
//!   flags) — while every string longer than `elide_arg_chars` in the
//!   arguments becomes `[pruned N chars]`; the frozen checkpoint stores
//!   the elided arguments verbatim so re-application is byte-stable.
//! * user items *superseded by a later instruction* — never ordinal 0
//!   (the session brief) and never the last user item (the live task's
//!   brief under follow-up tasks) — become a one-line stub once they
//!   age past the window.
//! * assistant reasoning traces older than `elide_reasoning` turns are
//!   dropped from the projection (`reasoning = None`). Off by default:
//!   replaying reasoning verbatim is a provider contract on the OpenAI
//!   wire (DeepSeek rejects a tool-bearing request whose prior
//!   assistant turns lack `reasoning_content`), so the engine enables
//!   it only where omission is provably safe — GLM (forfeits only the
//!   preserved-thinking cache benefit) and Anthropic (never forwards
//!   reasoning on replay anyway).
//!
//! All are deterministic — no model call, no I/O — and run through the
//! same reclaim gate and checkpoint freeze, so each batch costs exactly
//! one prefix-cache miss. Assistant text is still never reduced; the
//! LLM-handoff remains the last resort for it
//!
//! Budget-exceeded rescue: when the inner compiler's estimate already
//! overruns the budget — the trigger never got the chance to fire below
//! the wall — the wrapper decides stubs immediately and recompiles a
//! stubbed view of the same transcript (same rules, same estimator, same
//! protected tail). A rescue with nothing prunable, or one that is still
//! over budget once fully pruned, fails closed with the original
//! [`ContextError::BudgetExceeded`](super::ContextError::BudgetExceeded). The rescue freezes a checkpoint
//! like any other trigger, so later turns stay byte-stable and the
//! rescue costs exactly one cache miss.

mod compiler;
mod config;
mod elision;
mod facts;
mod projection;
mod reclaim;
mod strategies;
mod types;

pub use compiler::PruningContextCompiler;
pub use config::PruneConfig;
pub use types::{ArgElision, PruneReport, PruneTrigger, PrunedCall, StubDecision, StubReason};

#[cfg(test)]
mod tests;
