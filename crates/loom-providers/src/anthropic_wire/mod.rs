//! Anthropic Messages API wire translation — pure, no I/O.
//!
//! The mirror of [`crate::openai_wire`] for the Anthropic Messages
//! protocol: request-body construction, SSE event assembly, stop-reason
//! mapping, and error classification. Everything here is unit-testable
//! without a key (roadmap item 14: the adapter pressures the
//! `ModelProvider` boundary offline).
//!
//! Wire shape (Messages API, `anthropic-version: 2023-06-01`):
//!
//! - the system prompt is a top-level `system` parameter, not a message;
//!   a `System` message that appears **after** the conversation has
//!   started is *not* hoisted there — it stays in place as an in-turn
//!   `system`-role message, the Fable 5.1+ steering form (e.g. a
//!   per-turn `output_config.effort` note) that applies to the next
//!   assistant turn without invalidating the cached prefix;
//! - conversation turns are `user`/`assistant` messages whose `content`
//!   is an array of typed blocks (`text`, `tool_use`, `tool_result`);
//! - tool results travel in `user`-role `tool_result` blocks keyed by
//!   `tool_use_id`;
//! - streaming terminates with a dispatched `message_stop` event (no
//!   OpenAI-style `[DONE]` sentinel);
//! - prompt caching is requested via `cache_control` markers on content
//!   blocks, placed by this adapter from the kernel's
//!   [`ModelRequest::stable_prefix_messages`] hint (the kernel itself
//!   stays provider-blind).
//!
//! Deviations from the `OpenAI` wire that matter here:
//!
//! - `max_tokens` is **required** — defaulted when the request omits it;
//! - `temperature` is **never sent**: Claude 4.7 and later (the whole
//!   5.5 family, Fable 5.1 included) reject a non-default value with a
//!   400, and adaptive thinking makes it moot on the current lineup.
//!   Loom has no surface that sets one; see [`build_request_body`];
//! - assistant `reasoning` traces are not forwardable (Anthropic has no
//!   replay field for foreign reasoning), so they are dropped on
//!   translation — only reasoning *received* from Anthropic is replayed,
//!   via its own blocks in later requests' assistant messages;
//! - failures can arrive as `error` events *inside* an otherwise-200
//!   SSE stream, not only as HTTP statuses.
//!
//! Consequences of the 5.5-generation changes that are *already*
//! satisfied here, and must stay satisfied:
//!
//! - **preserved thinking** — thinking blocks are bound to the model and
//!   the conversation that produced them, and replaying hand-edited ones
//!   is an API error. Loom never re-emits thinking blocks (reasoning is
//!   replayed as plain text, if at all), so it cannot violate this;
//! - **thinking cannot be disabled** on Opus 5.5 / Fable 5.1 — nothing
//!   to do, since the adapter never sends a `thinking` parameter and
//!   the table reports `reasoning: true` for every model;
//! - **forced tool use is an error** — the adapter never sends
//!   `tool_choice`, so `tool_choice: any` is simply unavailable.

mod accumulate;
mod error;
mod request;
#[cfg(test)]
mod tests;

pub(crate) use accumulate::AnthropicAssembler;
pub(crate) use error::{classify_status, classify_stream_error};
pub(crate) use request::build_request_body;

/// Header value for the required `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta header required by server-side context editing. Sent only
/// when the request body carries `context_management` — a dated
/// constant; if Anthropic renames the strategy, this and the edit
/// type in `anthropic_wire` are the only places to touch.
pub(crate) const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// `max_tokens` fallback when the request carries none. Anthropic
/// requires the parameter; the kernel's compiler normally derives it
/// from capabilities, so this covers direct adapter use only.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8_192;
