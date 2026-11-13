//! Provider-neutral request and capability types.

use crate::tool::ToolSpec;
use crate::transcript::{AssistantMessage, ToolResult};
use serde::{Deserialize, Serialize};

/// One provider-neutral message in a [`ModelRequest`].
///
/// This is the *model-visible* projection of a conversation — built by the
/// context compiler from durable transcript [`Item`](crate::transcript::Item)s,
/// never persisted directly. Unlike an `Item`, a message can be a
/// [`Message::System`] prompt (which the transcript does not store), and the
/// projection may legitimately differ from the transcript (e.g. refused
/// `ContentFilter` turns are never replayed, §8.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Stable instruction prefix; cached by providers, so it should be
    /// deterministic for a given task.
    System {
        /// The system prompt text.
        text: String,
    },
    /// User input.
    User {
        /// The user's text, exactly as received.
        text: String,
    },
    /// An assistant turn, projected verbatim from the transcript (including
    /// stored reasoning when the compiler chooses to replay it).
    Assistant(AssistantMessage),
    /// The outcome of one tool call, projected verbatim from the
    /// transcript. Order within the request follows original tool-call
    /// order so transcript construction stays deterministic (§10.1).
    ToolResult(ToolResult),
}

impl Message {
    /// Returns the message's role name as used in the serde `role` tag
    /// (`"system"`, `"user"`, `"assistant"`, `"tool_result"`).
    #[must_use]
    pub fn role(&self) -> &'static str {
        match self {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "tool_result",
        }
    }
}

/// Provider-agnostic reasoning-effort intent for one request (roadmap
/// item 9). The kernel resolves a level per turn (a schedule); adapters
/// translate it into their wire dialect or ignore the whole hint — an
/// adapter that cannot steer effort sends nothing, exactly like
/// [`ContextEditingHint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Low effort: mid-task turns between the opening and verification.
    Low,
    /// Medium effort (dialects without it snap to the nearest side).
    Medium,
    /// High effort: opening turns and post-verification decisions.
    High,
}

impl ReasoningEffort {
    /// Wire string for the OpenAI-compatible `reasoning_effort` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

/// A single request to a model, produced by the context compiler.
///
/// All fields are provider-neutral; the adapter translates them into its
/// wire format. The compiler owns which transcript items and tools are
/// included (§8) — the agent loop never builds a `ModelRequest` itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Provider-specific model identifier (e.g. `"deepseek-flash"`). The
    /// provider resolves it to concrete [`ModelCapabilities`] (§6.4).
    pub model: String,
    /// The full projected conversation: system prompt first, then turns in
    /// transcript order.
    pub messages: Vec<Message>,
    /// Tool definitions exposed for this request. Deliberately explicit per
    /// request (§8.2): the compiler decides the set once per task so the
    /// cached prompt prefix stays stable.
    pub tools: Vec<ToolSpec>,
    /// Hard cap on generated tokens, if the caller sets one.
    pub max_tokens: Option<u32>,
    /// Sampling temperature, if the caller overrides the provider default.
    pub temperature: Option<f32>,
    /// Provider-agnostic cache hint: the number of leading `messages`
    /// the compiler guarantees byte-identical in every subsequent
    /// request of the same task (a lower bound on the stable prefix).
    ///
    /// The kernel stays provider-blind — adapters **may** use this to
    /// place provider cache breakpoints (e.g. Anthropic `cache_control`);
    /// `None` makes no promise. Old serialized requests default to it.
    #[serde(default)]
    pub stable_prefix_messages: Option<usize>,
    /// Server-side context editing hint (roadmap item 12): past
    /// `trigger_tokens` input tokens, adapters **may** ask the provider
    /// to clear stale tool results server-side, keeping the
    /// [`keep_tool_uses`](ContextEditingHint::keep_tool_uses) most
    /// recent uses. `None` (the default) sends nothing — the kernel's
    /// own projection and compaction stay authoritative. Additive and
    /// skipped when off, so serialized requests are byte-identical
    /// until a frontend opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_editing: Option<ContextEditingHint>,
    /// Per-turn reasoning-effort intent (roadmap item 9): the kernel's
    /// schedule resolved this request's level. `None` (the default)
    /// sends nothing; adapters that cannot steer effort ignore it. The
    /// level rides the request, never the message list, so the
    /// stable-prefix and prompt-cache decisions are blind to it.
    /// Additive and skipped when off, so serialized requests stay
    /// byte-identical until a schedule opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Provider-agnostic intent for server-side clearing of stale tool
/// results (roadmap item 12).
///
/// Names carry *intent*, never provider wire details — the Anthropic
/// adapter is the only place these become
/// `context_management.edits[clear_tool_uses_20250919]` (plus its beta
/// header). Every knob maps one-to-one onto that strategy today, which
/// is why the struct is provider-agnostic without being vague: an
/// adapter that cannot honor a knob ignores the whole hint rather than
/// half-applying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEditingHint {
    /// Clear only once the request's input tokens exceed this
    /// threshold (Anthropic requires >= `50_000` for its compaction
    /// trigger; this field imposes no such floor — validation is the
    /// provider's, not the kernel's).
    pub trigger_tokens: u32,
    /// Keep this many most-recent tool uses uncleared; older ones
    /// become placeholders when the trigger fires.
    pub keep_tool_uses: u32,
    /// When set, clear at least this many tokens per activation —
    /// clearing invalidates the cached prompt prefix, so small
    /// clearings cost more in cache misses than they save.
    pub clear_at_least_tokens: Option<u32>,
    /// Tool names whose results are never cleared.
    pub exclude_tools: Vec<String>,
}

/// What a model is capable of — descriptive facts, not policy (§3).
///
/// Resolved **per model** via [`ModelProvider::capabilities`]; the core uses
/// these to avoid sending unsupported requests (e.g. tools to a model
/// without `tool_calls`) and to shape future context decisions. It never
/// encodes provider *behaviour* — no retry quirks, no wire details.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Maximum tokens the model accepts as request context.
    pub max_context_tokens: u32,
    /// Maximum tokens the model can generate in one response, if known.
    pub max_output_tokens: Option<u32>,
    /// Whether the model supports tool calls at all.
    pub tool_calls: bool,
    /// Whether the model may be asked for several tool calls in one turn.
    pub parallel_tool_calls: bool,
    /// Whether the model emits reasoning traces.
    ///
    /// A `bool` for now; grows into a mode enum only when the compiler must
    /// choose between reasoning modes (§6.4).
    pub reasoning: bool,
}

impl ModelCapabilities {
    /// Creates a capabilities value with conservative defaults: no known
    /// output limit and every optional feature off.
    ///
    /// `#[non_exhaustive]` means callers outside this crate cannot use a
    /// struct literal, so provider adapters build values through this
    /// constructor and the `with_*` methods.
    #[must_use]
    pub const fn new(max_context_tokens: u32) -> Self {
        Self {
            max_context_tokens,
            max_output_tokens: None,
            tool_calls: false,
            parallel_tool_calls: false,
            reasoning: false,
        }
    }

    /// Sets the maximum output tokens (builder style).
    #[must_use]
    pub const fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = Some(max);
        self
    }

    /// Sets whether the model supports tool calls (builder style).
    #[must_use]
    pub const fn with_tool_calls(mut self, enabled: bool) -> Self {
        self.tool_calls = enabled;
        self
    }

    /// Sets whether the model may be asked for several tool calls in one
    /// turn (builder style).
    #[must_use]
    pub const fn with_parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = enabled;
        self
    }

    /// Sets whether the model emits reasoning traces (builder style).
    #[must_use]
    pub const fn with_reasoning(mut self, enabled: bool) -> Self {
        self.reasoning = enabled;
        self
    }
}
