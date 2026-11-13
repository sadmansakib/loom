//! Provider-neutral tool calls, results, and their validated value wrappers.

use crate::artifact::ArtifactRef;
use crate::tool::ToolEffects;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    #[must_use]
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

/// The reserved key an adapter uses to salvage a tool call whose raw
/// arguments did not parse as a JSON object ([`ToolArguments::malformed`]).
///
/// A salvaged call is committed to the transcript verbatim (history stays
/// append-only, provider prefix caches stay valid) but is never executed:
/// the engine sees the key and answers the call with an `InvalidInput`
/// error result telling the model to reissue it with well-formed JSON —
/// the same recovery the OpenAI/Anthropic loops use — instead of failing
/// the whole task on a protocol error.
pub const MALFORMED_ARGUMENTS_KEY: &str = "_loom_malformed_arguments";

/// Longest raw-arguments excerpt kept in a salvaged call's placeholder.
pub(crate) const MALFORMED_ARGUMENTS_EXCERPT: usize = 400;

impl ToolArguments {
    /// Builds the placeholder object for arguments that did not parse:
    /// `{ MALFORMED_ARGUMENTS_KEY: "<raw excerpt>" }`. The excerpt is
    /// bounded so a runaway fragment cannot bloat the transcript, and
    /// the cut backs off to a UTF-8 char boundary — the raw text is
    /// model-controlled, so a naive byte slice could panic.
    #[must_use]
    pub fn malformed(raw: &str) -> Self {
        let head = crate::event::head_bytes(raw, MALFORMED_ARGUMENTS_EXCERPT);
        let excerpt = if head.len() < raw.len() {
            format!("{head}[truncated]")
        } else {
            raw.to_owned()
        };
        Self(json!({ MALFORMED_ARGUMENTS_KEY: excerpt }))
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
/// [`crate::transcript::AssistantMessage`].
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
    /// they parse before constructing the committed `ToolCall`; max-token
    /// truncation discards incomplete calls outright. Value equality is the
    /// fidelity contract — raw byte formatting need not survive.
    ///
    /// One exception: arguments that parse to nothing usable are salvaged
    /// as a placeholder object carrying the raw text under
    /// [`MALFORMED_ARGUMENTS_KEY`] (see [`ToolArguments::malformed`]). The
    /// turn commits — append-only history, valid prefix caches — but the
    /// engine never dispatches the call ([`Self::arguments_are_malformed`]);
    /// the model gets an error result and reissues.
    pub arguments: ToolArguments,
}

impl ToolCall {
    /// Whether this call's arguments are the malformed-arguments
    /// placeholder ([`MALFORMED_ARGUMENTS_KEY`]) rather than real,
    /// model-intended arguments. Such calls are committed for history
    /// fidelity but never executed.
    #[must_use]
    pub fn arguments_are_malformed(&self) -> bool {
        self.arguments.get(MALFORMED_ARGUMENTS_KEY).is_some()
    }
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
