//! What a tool call returns: the successful [`ToolOutput`], harness
//! [`ToolNotice`]s, and the tool-side [`ToolError`] variants.

use serde::{Deserialize, Serialize};

use super::effects::ToolEffects;
use crate::artifact::ArtifactRef;

/// What a successful tool call produced.
///
/// The engine stores `content` as the model-visible
/// [`ToolResult::content`](crate::transcript::ToolResult::content);
/// `truncated` flows to
/// [`ToolResult::truncated`](crate::transcript::ToolResult::truncated)
/// and `effects` to
/// [`ToolResult::effects`](crate::transcript::ToolResult::effects).
///
/// `#[non_exhaustive]`: construct with [`ToolOutput::new`] and the
/// builder methods so later iterations can carry more metadata (ADR-012)
/// without another breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Model-visible text. Concise by convention — the model reads this,
    /// not a human.
    pub content: String,
    /// Whether `content` was cut short (e.g. a byte cap); orthogonal to
    /// success/failure.
    pub truncated: bool,
    /// What the call changed outside Loom's own state.
    pub effects: ToolEffects,
    /// Set when the full output was stored out of band (ADR-012 decision 3,
    /// ADR-014): the transcript then carries this compact handle and
    /// `content` says how to refetch it with `artifact_read`. `None`
    /// for an ordinary inline result.
    pub artifact: Option<ArtifactRef>,
    /// Harness-level observations for the trace, not the model: a
    /// sandbox that passed a command through, a stream a leaked
    /// descendant truncated. The context compiler never projects these
    /// into the model's view — they ride the result into
    /// `events.ndjson` as [`crate::event::EventKind::HarnessNotice`]s.
    pub notices: Vec<ToolNotice>,
}

/// One surfaced harness observation attached to a tool output.
///
/// Categories are stable dotted strings (`"sandbox.passthrough"`,
/// `"stream.truncated"`); consumers must tolerate unknown categories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolNotice {
    /// Dotted category for filtering and attribution (stable string).
    pub category: String,
    /// One-line human-readable detail.
    pub detail: String,
}

impl ToolNotice {
    /// Builds a notice from category and detail.
    pub fn new(category: &'static str, detail: impl Into<String>) -> Self {
        Self {
            category: category.to_owned(),
            detail: detail.into(),
        }
    }
}

impl ToolOutput {
    /// Full (untruncated) output with no recorded effects.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            effects: ToolEffects::none(),
            artifact: None,
            notices: Vec::new(),
        }
    }

    /// Marks the output as truncated (builder style).
    #[must_use]
    pub fn with_truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    /// Attaches a harness observation for the trace (builder style).
    #[must_use]
    pub fn with_notice(mut self, notice: ToolNotice) -> Self {
        self.notices.push(notice);
        self
    }

    /// Attaches the call's recorded side effects (builder style).
    #[must_use]
    pub fn with_effects(mut self, effects: ToolEffects) -> Self {
        self.effects = effects;
        self
    }

    /// Attaches a handle to the out-of-band body this result collapsed to
    /// (builder style).
    #[must_use]
    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }

    /// The call's recorded side effects.
    #[must_use]
    pub fn effects(&self) -> &ToolEffects {
        &self.effects
    }
}

/// Why a [`Tool`](super::Tool)`::call` failed.
///
/// Only the two tool-side failure modes exist here. The engine maps them
/// to `ToolStatus::Error(InvalidInput)` and `ToolStatus::Error(Execution)`
/// respectively; every other status — `UnknownTool`, `NotExposed`,
/// `Timeout`, `Cancelled` — is *engine-detected* and never crosses this
/// seam (§9.3).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ToolError {
    /// The tool rejected the invocation itself: arguments failed to
    /// parse into the tool's input type (bad JSON shape, wrong types),
    /// or parsed but are semantically wrong (bad path shape, unknown
    /// option, missing prerequisite). No side effects happened.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The tool ran (or began running) and failed — an environment fault
    /// (I/O error, missing file, command failure). Side effects are
    /// possible.
    #[error("execution failed: {0}")]
    Execution(String),
}
