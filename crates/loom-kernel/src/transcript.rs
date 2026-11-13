//! The durable conversation transcript.
//!
//! The transcript is an append-only list of [`Item`]s and the **source of
//! truth** for session state (ADR-004): rebuilding a session means replaying
//! its items, never replaying the event stream. Items are addressed by
//! [`ItemIndex`]; because the list only grows, an index is stable for the
//! life of the session and doubles as the persistence key.
//!
//! Two invariants from the architecture govern this module:
//!
//! 1. **Only committed model output enters the transcript.** Streaming
//!    deltas are provisional; exactly one authoritative
//!    [`AssistantMessage`] per model response is appended, after the
//!    provider reports completion.
//! 2. **Turn commit is atomic (§6.1).** A turn's assistant item and one
//!    result per tool call are appended together or not at all — a turn
//!    containing tool calls is committed only after all tool results exist,
//!    in original tool-call order (§10.1).
//!
//! [`Item`] is intentionally *not* a model `Message`: future items
//! (compaction summaries, task markers, artifacts) will not necessarily be
//! model messages. The `ContextCompiler` owns the projection from items to
//! provider-neutral messages.

use crate::model::StopReason;
#[cfg(test)]
use crate::tool_call::MALFORMED_ARGUMENTS_EXCERPT;
pub use crate::tool_call::{
    MALFORMED_ARGUMENTS_KEY, ToolArguments, ToolArgumentsError, ToolCall, ToolCallId,
    ToolCallIdError, ToolFailure, ToolResult, ToolStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Position of an [`Item`] inside the session transcript.
///
/// Indices are assigned by append order (`0, 1, 2, …`), never reused, and
/// stable for the life of the session — which makes them safe to use as
/// persistence keys and pruning checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemIndex(u32);

impl ItemIndex {
    /// Returns the index as a `u32` (the persistence `idx` column).
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl From<u32> for ItemIndex {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ItemIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One durable entry in the conversation transcript.
///
/// The set of kinds is deliberately small for iteration 1. The enum is
/// `#[non_exhaustive]` so adding entries later (compaction summaries, task
/// markers, artifacts) stays source-compatible for downstream `match`es.
///
/// Stored-transcript compatibility is **forward-only**, matching the
/// store's migration policy: rows written by older code load in newer code,
/// but a newer kind in an older binary fails deserialization loudly rather
/// than being silently dropped.
///
/// Serialized with an internal `kind` tag matching the persistence `kind`
/// column (`user | assistant | tool_result`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// Input from the user (or a resumed conversation's stored input).
    User {
        /// The raw user text, exactly as received.
        text: String,
    },
    /// A complete, authoritative assistant response.
    ///
    /// Only committed model output becomes an `Assistant` item — streaming
    /// deltas are provisional and never persisted.
    Assistant(AssistantMessage),
    /// The outcome of one tool call, model-visible and persisted even on
    /// failure.
    ToolResult(ToolResult),
}

/// A complete assistant response as committed to the transcript.
///
/// Built by the provider adapter from a finished model response; nothing in
/// here is provisional.
///
/// Part of the event wire format (via `ModelCompleted`): adding a field
/// bumps `EVENT_SCHEMA_VERSION` in `crate::event`. The event form is
/// full-fidelity — including reasoning `provider_data` — so traces carry
/// the same data as the saved session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// The assistant's visible text output.
    pub content: String,
    /// Chain-of-thought text, if the provider emitted any.
    ///
    /// Stored byte-exact as received (no trimming or whitespace
    /// normalisation): replaying reasoning verbatim is a provider contract
    /// once tools are involved, not an optimisation.
    pub reasoning: Option<ReasoningTrace>,
    /// Tool executions the model requested, in the order the model issued
    /// them. Results come back as separate [`Item::ToolResult`] items, one
    /// per call, in the same order.
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped generating. Persisted here (not re-derived)
    /// because the context compiler projects on it — e.g.
    /// [`StopReason::ContentFilter`] output stays inspectable in the
    /// transcript but is excluded from later requests.
    pub stop_reason: StopReason,
}

/// Provider-neutral reasoning ("thinking") trace attached to an assistant
/// message.
///
/// `text` is human-readable reasoning stored byte-exact. `provider_data`
/// carries opaque provider payloads that must be returned verbatim on
/// replay (e.g. signed thinking blocks); core never interprets it — the
/// context compiler only replays it to the provider that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTrace {
    /// The reasoning text, exactly as received — no trimming, no
    /// normalisation.
    pub text: String,
    /// Opaque round-trip payload owned by the producing provider.
    pub provider_data: Option<ProviderData>,
}

/// The provider-owned half of a [`ReasoningTrace`].
///
/// `data` is opaque JSON: preserving *value* equality is required,
/// byte-identical re-serialization is not (JSON object key order may
/// differ). Providers that need stricter fidelity must encode it inside
/// the value themselves.
///
/// Part of the event wire format (via `ModelCompleted`): adding a field
/// bumps `EVENT_SCHEMA_VERSION` in `crate::event`. Appears verbatim in
/// `--json` traces — treat shared traces with the same care as session
/// files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderData {
    /// Identifies the provider that owns `data` (e.g. `"deepseek"`).
    pub provider: String,
    /// The opaque payload, replayed verbatim to the same provider.
    pub data: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{CommandEffect, ToolEffects};

    /// Builds a representative assistant message exercising every nested
    /// type at once.
    fn sample_assistant() -> AssistantMessage {
        AssistantMessage {
            content: "Reading the file now.".into(),
            reasoning: Some(ReasoningTrace {
                text: "  user wants config …\n\todd whitespace + ünicode ✓ ".into(),
                provider_data: Some(ProviderData {
                    provider: "deepseek".into(),
                    data: serde_json::json!({"signature": "abc123", "nested": {"b": 1, "a": 2}}),
                }),
            }),
            tool_calls: vec![ToolCall {
                id: ToolCallId::try_from("call_1".to_string()).unwrap(),
                name: "read_file".into(),
                arguments: ToolArguments::try_from(serde_json::json!({"path": "Cargo.toml"}))
                    .unwrap(),
            }],
            stop_reason: StopReason::ToolCalls,
        }
    }

    #[test]
    fn assistant_item_round_trips_losslessly() {
        let item = Item::Assistant(sample_assistant());
        let json = serde_json::to_string(&item).unwrap();
        let back: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
    }

    #[test]
    fn reasoning_text_survives_byte_exact() {
        // §4.2: no trimming, no normalisation — odd whitespace and
        // unicode must survive transcript -> serde -> transcript
        // byte-identical.
        let text = " \t leading … trailing \n\r\n\u{00a0}ünïcode ✓ ";
        let trace = ReasoningTrace {
            text: text.into(),
            provider_data: None,
        };
        let json = serde_json::to_string(&trace).unwrap();
        let back: ReasoningTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text, text);
        assert_eq!(back.text.len(), text.len());
    }

    #[test]
    fn item_serializes_with_kind_tag() {
        // The `kind` tag must line up with the persistence `kind` column.
        let user = serde_json::to_value(Item::User { text: "hi".into() }).unwrap();
        assert_eq!(user["kind"], "user");
        assert_eq!(user["text"], "hi");

        let assistant = serde_json::to_value(Item::Assistant(sample_assistant())).unwrap();
        assert_eq!(assistant["kind"], "assistant");

        let result = serde_json::to_value(Item::ToolResult(ToolResult::new(
            ToolCallId::try_from("call_1".to_string()).unwrap(),
            ToolStatus::Success,
            "file contents",
            false,
        )))
        .unwrap();
        assert_eq!(result["kind"], "tool_result");
        assert_eq!(result["call_id"], "call_1");
    }

    #[test]
    fn malformed_arguments_excerpt_never_splits_a_multibyte_char() {
        // 399 ASCII bytes then a multi-byte char: the 400-byte cut
        // lands mid-character. The naive byte slice panics here.
        let raw = format!("{}é…", "a".repeat(399));
        assert!(raw.len() > MALFORMED_ARGUMENTS_EXCERPT);
        let args = ToolArguments::malformed(&raw);
        let excerpt = args[MALFORMED_ARGUMENTS_KEY].as_str().unwrap();
        let head = excerpt.strip_suffix("[truncated]").unwrap_or(excerpt);
        assert!(raw.starts_with(head));
        assert!(head.len() <= MALFORMED_ARGUMENTS_EXCERPT);

        // Same shape with an emoji straddling the cut.
        let emoji = format!("{}😀", "b".repeat(398));
        let args = ToolArguments::malformed(&emoji);
        let excerpt = args[MALFORMED_ARGUMENTS_KEY].as_str().unwrap();
        assert!(excerpt.ends_with("[truncated]"));
        let head = excerpt.strip_suffix("[truncated]").unwrap();
        assert!(emoji.starts_with(head));
        assert!(head.len() <= MALFORMED_ARGUMENTS_EXCERPT);
    }

    #[test]
    fn tool_call_persists_provider_neutral_shape() {
        // Canonical persisted form: bare id/name/object-arguments — no
        // OpenAI-style {"type":"function","function":{…}} wrapper.
        let call = ToolCall {
            id: ToolCallId::try_from("call_123".to_string()).unwrap(),
            name: "read_file".into(),
            arguments: ToolArguments::try_from(serde_json::json!({"path": "src/main.rs"})).unwrap(),
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["id"], "call_123");
        assert_eq!(json["name"], "read_file");
        assert_eq!(json["arguments"]["path"], "src/main.rs");
        assert!(json.get("type").is_none());
        assert!(json.get("function").is_none());
        assert!(json.get("index").is_none());
    }

    #[test]
    fn tool_call_round_trips_value_arguments() {
        // ToolArguments wraps a serde_json::Value and guarantees it is an
        // object; deep/nested structures survive serialization by value
        // equality (formatting need not survive).
        let call = ToolCall {
            id: ToolCallId::try_from("c9".to_string()).unwrap(),
            name: "search".into(),
            arguments: ToolArguments::try_from(serde_json::json!({
                "query": "ünicode ✓",
                "opts": {"case_sensitive": false, "depth": 3},
            }))
            .unwrap(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call, back);
    }

    #[test]
    fn tool_result_takes_call_id_directly() {
        // Strong correlation: call.id flows into result.call_id with no
        // ambiguous string handling.
        let call = ToolCall {
            id: ToolCallId::try_from("call_77".to_string()).unwrap(),
            name: "read_file".into(),
            arguments: ToolArguments::try_from(serde_json::json!({"path": "x"})).unwrap(),
        };
        let result = ToolResult::new(call.id.clone(), ToolStatus::Success, "ok", false);
        assert_eq!(result.call_id, call.id);
        assert_eq!(result.call_id.as_str(), "call_77");
    }

    #[test]
    fn duplicate_calls_stay_distinguishable() {
        // Same tool, identical arguments, different ids — both are valid
        // distinct invocations.
        let make = |id: &str| ToolCall {
            id: ToolCallId::try_from(id.to_string()).unwrap(),
            name: "search".into(),
            arguments: ToolArguments::try_from(serde_json::json!({"q": "rust"})).unwrap(),
        };
        let a = make("call_1");
        let b = make("call_2");
        assert_ne!(a, b, "identical calls must differ by id");
        assert_eq!(a.name, b.name);
        assert_eq!(a.arguments, b.arguments);
    }

    #[test]
    fn tool_call_id_rejects_empty_on_every_path() {
        // Construction path.
        assert!(matches!(
            ToolCallId::try_from(String::new()),
            Err(ToolCallIdError::Empty)
        ));
        // Load path: a stored empty id fails deserialization loudly.
        assert!(serde_json::from_str::<ToolCallId>("\"\"").is_err());
        // Non-empty ids pass through unchanged, whatever they contain.
        let id = ToolCallId::try_from("call_…✓".to_string()).unwrap();
        assert_eq!(id.as_str(), "call_…✓");
    }

    #[test]
    fn tool_arguments_reject_non_objects_on_every_path() {
        // Construction path: every non-object JSON kind is rejected.
        for bad in [
            Value::Null,
            Value::Bool(true),
            serde_json::json!(3),
            serde_json::json!("str"),
            serde_json::json!([1, 2]),
        ] {
            assert!(
                ToolArguments::try_from(bad).is_err(),
                "non-object arguments must be rejected"
            );
        }
        // Load path: a corrupted row (array arguments) fails at
        // deserialization, before any tool code indexes into it.
        let json = r#"{"id":"c1","name":"t","arguments":[1,2]}"#;
        assert!(serde_json::from_str::<ToolCall>(json).is_err());
        // Objects are accepted and deref gives key access.
        let args = ToolArguments::try_from(serde_json::json!({"path": "x"})).unwrap();
        assert_eq!(args["path"], "x");
    }

    #[test]
    fn tool_status_is_copy_eq_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let hash = |s: ToolStatus| {
            let mut h = DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(ToolStatus::Success), hash(ToolStatus::Success));

        // Copy: `b` is a bitwise duplicate, `a` stays usable.
        let a = ToolStatus::Error(ToolFailure::Timeout);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn transcript_replay_preserves_tool_calls() {
        // Full replay path: committed assistant message -> serde -> back,
        // every call keeps id, name, arguments.
        let original = Item::Assistant(AssistantMessage {
            content: "done".into(),
            reasoning: None,
            tool_calls: vec![
                ToolCall {
                    id: ToolCallId::try_from("call_a".to_string()).unwrap(),
                    name: "read_file".into(),
                    arguments: ToolArguments::try_from(serde_json::json!({"path": "a.rs"}))
                        .unwrap(),
                },
                ToolCall {
                    id: ToolCallId::try_from("call_b".to_string()).unwrap(),
                    name: "read_file".into(),
                    arguments: ToolArguments::try_from(serde_json::json!({"path": "b.rs"}))
                        .unwrap(),
                },
            ],
            stop_reason: StopReason::ToolCalls,
        });

        let json = serde_json::to_string(&original).unwrap();
        let replayed: Item = serde_json::from_str(&json).unwrap();

        let Item::Assistant(msg) = replayed else {
            panic!("expected assistant item");
        };
        assert_eq!(msg.tool_calls.len(), 2);
        assert_eq!(msg.tool_calls[0].id.as_str(), "call_a");
        assert_eq!(msg.tool_calls[0].name, "read_file");
        assert_eq!(msg.tool_calls[0].arguments["path"], "a.rs");
        assert_eq!(msg.tool_calls[1].id.as_str(), "call_b");
        assert_eq!(msg.tool_calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn every_tool_status_round_trips() {
        let statuses = [
            ToolStatus::Success,
            ToolStatus::Error(ToolFailure::UnknownTool),
            ToolStatus::Error(ToolFailure::NotExposed),
            ToolStatus::Error(ToolFailure::InvalidInput),
            ToolStatus::Error(ToolFailure::Execution),
            ToolStatus::Error(ToolFailure::Timeout),
            ToolStatus::Cancelled { started: true },
            ToolStatus::Cancelled { started: false },
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let back: ToolStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "round trip failed for {json}");
        }
    }

    #[test]
    fn cancelled_status_serializes_started_flag() {
        let json = serde_json::to_value(ToolStatus::Cancelled { started: false }).unwrap();
        assert_eq!(json["cancelled"]["started"], false);
    }

    #[test]
    fn item_index_transparent_and_ordered() {
        let a = ItemIndex::from(41u32);
        let b = ItemIndex::from(42u32);
        assert!(a < b);
        assert_eq!(a.as_u32(), 41);
        assert_eq!(a.to_string(), "41");
        assert_eq!(serde_json::to_string(&a).unwrap(), "41");
        let back: ItemIndex = serde_json::from_str("41").unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn tool_result_effects_round_trip() {
        let effects = ToolEffects::none()
            .with_file_written("src/lib.rs")
            .with_file_deleted("src/old.rs")
            .with_command(CommandEffect::new(
                "cargo",
                vec!["test".to_owned()],
                Some(0),
            ));
        let mut result = ToolResult::new(
            ToolCallId::try_from("call_1".to_string()).unwrap(),
            ToolStatus::Success,
            "wrote src/lib.rs",
            false,
        );
        result.effects = effects.clone();
        let json = serde_json::to_string(&result).unwrap();
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.effects, effects);
    }

    #[test]
    fn effects_default_to_empty_when_a_record_predates_them() {
        let legacy = serde_json::json!({
            "call_id": "call_1",
            "status": "success",
            "content": "ok",
            "truncated": false,
        });
        let result: ToolResult = serde_json::from_value(legacy).unwrap();
        assert!(result.effects.is_empty());
    }
}
