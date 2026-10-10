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

use crate::artifact::ArtifactRef;
use crate::model::StopReason;
use crate::tool::ToolEffects;
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

/// Error produced when constructing a [`ToolCallId`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolCallIdError {
    /// Ids must be non-empty. An empty id means the adapter failed its duty
    /// to pass the provider's id through or synthesize one — both are bugs
    /// worth catching at the boundary, not storing.
    #[error("tool call id must not be empty")]
    Empty,
}

/// Opaque correlation id linking a [`ToolCall`] to its [`ToolResult`].
///
/// Rules (architecture §7 + tool-call invariants):
///
/// - **Opaque.** Core never interprets its contents — no turn numbers,
///   indexes, timestamps, or provider semantics encoded inside.
/// - **Preserved exactly** as the provider supplied it.
/// - **Never derived** from tool name + arguments: two identical calls
///   (`search({"q":"rust"})` twice) must stay distinguishable.
/// - **Synthesized by the adapter** (stable, unique) when the provider
///   supplies no id — before the `AssistantMessage` is committed.
/// - **Stable once committed** to the transcript.
///
/// Non-emptiness is enforced on every input path ([`TryFrom`],
/// `Deserialize`); anything else about the string is the provider's business.
///
/// Not used for execution ordering; ordering comes from transcript order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Returns the id as a string slice (tracing, persistence).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolCallId {
    type Error = ToolCallIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err(ToolCallIdError::Empty)
        } else {
            Ok(Self(value))
        }
    }
}

impl From<ToolCallId> for String {
    /// Extracts the id (used by serde `into`).
    fn from(value: ToolCallId) -> Self {
        value.0
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error produced when constructing [`ToolArguments`] from a JSON value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolArgumentsError {
    /// Tool arguments must be a JSON object; the payload was some other
    /// JSON kind (the message names it).
    #[error("tool arguments must be a JSON object, got {0}")]
    NotObject(&'static str),
}

/// A tool call's arguments, guaranteed to be a JSON **object**.
///
/// The guarantee is enforced on every input path ([`TryFrom<Value>`],
/// `Deserialize`), mirroring how `SessionId`/`TaskId` enforce their v7 rule:
/// a corrupted or hand-edited transcript row fails loudly at load time,
/// before the tool engine ever indexes into it. The adapter assembles
/// provider argument fragments and validates them before constructing the
/// committed call; malformed JSON never becomes a `ToolCall` at all.
///
/// Dereferences to the underlying [`Value`] for key access
/// (`args["path"]`); value equality is the fidelity contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub struct ToolArguments(Value);

impl TryFrom<Value> for ToolArguments {
    type Error = ToolArgumentsError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        if value.is_object() {
            Ok(Self(value))
        } else {
            Err(ToolArgumentsError::NotObject(json_kind(&value)))
        }
    }
}

impl From<ToolArguments> for Value {
    /// Extracts the arguments object (used by serde `into`).
    fn from(value: ToolArguments) -> Self {
        value.0
    }
}

impl std::ops::Deref for ToolArguments {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Names a JSON value's kind for error messages ("null", "array", …).
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A tool execution requested by the model, recorded inside an
/// [`AssistantMessage`].
///
/// This is the **canonical provider-neutral** representation of a
/// model-generated invocation — the boundary between provider wire format,
/// transcript, and tool execution. Adapters assemble their native fragments
/// (OpenAI-style `index`/`function.name`/`function.arguments`, Anthropic
/// `tool_use` blocks, …) into this shape before anything reaches the core;
/// provider wire structures must never leak in here.
///
/// An invocation only — no execution state (`status`, `duration`, retries,
/// read-only classification all live elsewhere: results, events, the
/// registered `Tool`, `ToolSpec`).
///
/// Part of the event wire format (via `ModelCompleted`): adding a field
/// bumps `EVENT_SCHEMA_VERSION` in `crate::event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Opaque correlation id; see [`ToolCallId`].
    pub id: ToolCallId,
    /// Canonical Loom tool name (the `ToolSpec` name the tool engine
    /// resolves). Never a wire-level name variant.
    pub name: String,
    /// The call's arguments as a validated JSON **object** ([`ToolArguments`]
    /// enforces this on every input path).
    ///
    /// The adapter assembles provider argument fragments and validates that
    /// they parse before constructing the committed `ToolCall`: malformed
    /// JSON is a protocol error and never becomes a `ToolCall`; max-token
    /// truncation discards incomplete calls outright. Value equality is the
    /// fidelity contract — raw byte formatting need not survive.
    pub arguments: ToolArguments,
}

/// The outcome of one tool call, persisted and shown to the model.
///
/// Failures become results too — a tool error never aborts the task; the
/// model sees the (concise) `content` and decides what to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Matches the [`ToolCall::id`] this result answers.
    pub call_id: ToolCallId,
    /// Structured outcome; persisted for tracing and future policy logic.
    pub status: ToolStatus,
    /// Model-visible text (concise). On failure this is what the model
    /// reads, e.g. `error: unknown tool "grep"; available: …`.
    pub content: String,
    /// Whether `content` was cut short. Orthogonal to [`ToolStatus`]: an
    /// error result may also be truncated.
    pub truncated: bool,
    /// What the call changed outside Loom, recorded at execution time
    /// (ADR-012). Empty for read-only calls and for every failure — a
    /// call that never started cannot have effects. Deserializes to the
    /// empty value for results persisted before Iteration 2 step 2B.
    #[serde(default)]
    pub effects: ToolEffects,
    /// Set when the full output was stored out of band and `content` only
    /// says how to refetch it (ADR-012 decision 3, ADR-014). Loom-side
    /// state: the context compiler strips it from the model projection,
    /// and it defaults to `None` for results persisted before step 2D.
    #[serde(default)]
    pub artifact: Option<ArtifactRef>,
    /// Harness observations carried out of the tools layer (v8):
    /// sandbox passthrough, truncated streams. Persisted for tracing,
    /// never projected into the model's view; defaults to empty for
    /// results persisted before the field existed.
    #[serde(default)]
    pub notices: Vec<crate::tool::ToolNotice>,
}

impl ToolResult {
    /// A result carrying no recorded effects and no out-of-band body.
    ///
    /// Used by the engine for failures and read-only calls, and by
    /// fixtures; the engine attaches a successful call's effects from
    /// the tool's [`crate::tool::ToolOutput`].
    pub fn new(
        call_id: ToolCallId,
        status: ToolStatus,
        content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        Self {
            call_id,
            status,
            content: content.into(),
            truncated,
            effects: ToolEffects::none(),
            artifact: None,
            notices: Vec::new(),
        }
    }

    /// Attaches a handle to the out-of-band body this result collapsed to
    /// (builder style).
    #[must_use]
    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }
}

/// Structured outcome of a tool call.
///
/// The distinction between variants matters to future policy: whether a
/// call *started* (side effects possible) and whether the fault lies with
/// the model (invalid invocation — an efficiency signal) or the
/// environment (execution/timeout — possibly retryable).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// The call ran and produced its result.
    Success,
    /// The call failed; the payload says why.
    Error(ToolFailure),
    /// The call was cancelled before completing. `started` records
    /// whether execution had begun — blocking work may still be running
    /// (§6.6), which future policy needs to know.
    Cancelled {
        /// Whether the tool had started executing.
        started: bool,
    },
}

/// Structured cause of a tool failure, kept internally even though the
/// model only sees [`ToolResult::content`].
///
/// The first three variants mean the tool never started (no side
/// effects); the last two mean it ran (or began running) and failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailure {
    /// The model named a tool that does not exist.
    UnknownTool,
    /// The tool exists but was not offered in this request.
    NotExposed,
    /// Arguments failed to parse or validate against the tool's schema.
    InvalidInput,
    /// The tool ran and returned an error.
    Execution,
    /// The agent stopped waiting; blocking work may still continue.
    Timeout,
    /// The loop refused to re-run an identical call that already failed,
    /// because no tool call has succeeded since (§13). Repeating a failed
    /// attempt unchanged is not permitted; the model must change its
    /// approach. The call never started, so no side effects occurred.
    RepeatedCall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::CommandEffect;

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
