//! Projection rules (§8.3) — the only omissions a [`DefaultContextCompiler`]
//! makes:
//!
//! 1. **`ContentFilter` assistant turns are never replayed.** The item stays
//!    in the transcript (inspectable) but is omitted from every later
//!    projection, alone: the `Item::Assistant` with
//!    `stop_reason = ContentFilter` is dropped, and every other item —
//!    including any tool results that might follow it in the transcript —
//!    projects normally. In practice a refused turn carries no tool calls
//!    (§5.1 normalization), so nothing follows it.
//! 2. **`MaxTokens` assistant turns project verbatim, as text.** They never
//!    carry tool calls (§5.1 normalization), so the projection is exactly
//!    the stored message; the §8.3 rule is documented here because a future
//!    normalization change could otherwise re-introduce tool calls on a
//!    `MaxTokens` item, which must project as text only.
//!
//! Everything else — including all reasoning (§4.2) — is projected verbatim
//! in transcript order. Reasoning is replayed byte-exact because the
//! DeepSeek thinking-mode contract demands the complete, unmodified
//! reasoning of every prior turn once tools are sent (ADR-006).

mod census;
mod compiler;
mod effort;
mod projection;

#[cfg(test)]
mod tests;

pub use compiler::DefaultContextCompiler;

pub(super) use census::{census, serialized_size};
