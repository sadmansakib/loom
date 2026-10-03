//! Provider-neutral model abstractions.
//!
//! This module owns the types that describe a conversation with a language
//! model: requests, streaming events, the final response, and errors. It is
//! grown incrementally; this file currently defines only [`StopReason`],
//! which the transcript needs to persist (§4 of the architecture).

use serde::{Deserialize, Serialize};

/// Why the model stopped generating in a response that completed
/// successfully.
///
/// A stop reason describes *only* the model's own stopping condition. It is
/// deliberately distinct from failure modes that are not the model's
/// decision at all:
///
/// - cancellation is signalled by a `CancellationToken`, not a stop reason;
/// - protocol / transport failures are `ModelError`s (e.g. GLM's
///   `finish_reason: "network_error"` is a transport error, not a stop
///   reason).
///
/// Semantics (iteration 1):
///
/// | Variant | Loop behaviour |
/// |---|---|
/// | [`StopReason::EndTurn`] | terminal success |
/// | [`StopReason::ToolCalls`] | continue the loop, execute the calls |
/// | [`StopReason::MaxTokens`] | task failure (continuation recovery later) |
/// | [`StopReason::ContextLimit`] | task failure (compaction recovery later) |
/// | [`StopReason::ContentFilter`] | task failure |
///
/// Normalization from raw wire `finish_reason` values (e.g. `"stop"` emitted
/// alongside tool calls) is the **adapter's** duty; core only ever sees
/// structurally consistent values: `ToolCalls` without calls or `EndTurn`
/// with calls is a protocol error.
///
/// "No tool calls" is *not* synonymous with success — completion is decided
/// from this value and task state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer; no further work was requested.
    EndTurn,
    /// The model requested tool executions before continuing.
    ToolCalls,
    /// Generation hit the output token limit.
    ///
    /// Partial tool-call arguments are discarded by the adapter and never
    /// executed when this is the stop reason.
    MaxTokens,
    /// The request exceeded the model's context window.
    ContextLimit,
    /// The model's output was blocked by a content filter.
    ///
    /// Persisted in the transcript (inspectable) but excluded from context
    /// compilation for later requests.
    ContentFilter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_uses_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolCalls).unwrap(),
            "\"tool_calls\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ContentFilter).unwrap(),
            "\"content_filter\""
        );
    }

    #[test]
    fn round_trips_every_variant() {
        let all = [
            StopReason::EndTurn,
            StopReason::ToolCalls,
            StopReason::MaxTokens,
            StopReason::ContextLimit,
            StopReason::ContentFilter,
        ];
        for reason in all {
            let json = serde_json::to_string(&reason).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, back);
        }
    }
}
