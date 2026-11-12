use serde::{Deserialize, Serialize};

use crate::transcript::ToolArguments;

/// Why a tool result was stubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StubReason {
    /// A later identical read-only call supersedes this one.
    DuplicateRead,
    /// A later recorded write touched a path this call read.
    SupersededRead,
    /// An error result older than `stale_error_turns`.
    StaleError,
    /// A large read-only result aged out of the unprotected region.
    AgedOut,
    /// A read-only result older than `age_mask_turns` (turn-age
    /// masking, independent of the window trigger).
    TurnAge,
    /// A tool call's arguments were reduced to a signature — strings
    /// longer than `elide_arg_chars` replaced with `[pruned N chars]`
    /// (history elision). Never used for result bodies; `stub_text`
    /// exists for exhaustive matches and diagnostics.
    ElidedArgs,
    /// A user item after the task brief was replaced with a stub
    /// (history elision). The brief itself is never elided.
    ElidedUser,
    /// A read-only result masked by phase-aware retention: the loop
    /// entered the writing phase and this read is older than
    /// `phase_mask_turns`.
    WritingPhase,
    /// An assistant reasoning trace was dropped from the projection
    /// (history elision, provider-gated via
    /// [`PruneConfig::elide_reasoning`](crate::context::PruneConfig::elide_reasoning)). Assistant text is only
    /// elided by the last-resort window ([`StubReason::ElidedAssistantText`]).
    ElidedReasoning,
    /// An assistant text was reduced to its leading signature —
    /// the first 200 characters plus a `[pruned N chars]` marker
    /// (last-resort history elision, window/rescue passes only).
    ElidedAssistantText,
}

impl StubReason {
    /// The deterministic stub body replacing the pruned content.
    #[must_use]
    pub fn stub_text(self) -> &'static str {
        match self {
            Self::DuplicateRead => {
                "[pruned] duplicate of a later identical read-only call; re-run the tool to recover the content"
            }
            Self::WritingPhase => {
                "[pruned] exploration read masked in the writing phase; re-run the tool to recover the content"
            }
            Self::SupersededRead => {
                "[pruned] the file changed after this read; re-run the tool for the current content"
            }
            Self::StaleError => {
                "[pruned] stale error from an earlier turn; re-run the tool for a fresh result"
            }
            Self::AgedOut => {
                "[pruned] aged out by context pruning; re-run the tool to recover the content"
            }
            Self::TurnAge => {
                "[pruned] read-only result older than the kept turn window; re-run the tool to recover the content"
            }
            Self::ElidedArgs => {
                "[pruned] call arguments reduced to their signature; the result body carries the outcome"
            }
            Self::ElidedUser => {
                "[pruned] earlier user message elided by context pruning; the original task brief is preserved"
            }
            Self::ElidedReasoning => "[pruned] earlier reasoning elided by context pruning",
            Self::ElidedAssistantText => {
                "[pruned] earlier assistant text reduced to its signature by context pruning"
            }
        }
    }
}

/// One frozen stub decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubDecision {
    /// The call whose result is stubbed.
    pub call_id: String,
    /// Why — decides the stub text.
    pub reason: StubReason,
}

/// One frozen argument elision (history elision): the call keeps its
/// id, name and short argument strings, while every string longer than
/// `PruneConfig::elide_arg_chars` was replaced with `[pruned N chars]`.
///
/// The *elided* arguments are frozen verbatim (not just the call id) so
/// re-applying a checkpoint never depends on config drift between the
/// freeze and the re-apply — the projection stays byte-stable by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgElision {
    /// The call whose arguments are elided.
    pub call_id: String,
    /// The already-elided arguments, projected verbatim on re-apply.
    pub arguments: ToolArguments,
}

/// Which arm froze a [`PruneCheckpoint`](crate::context::PruneCheckpoint) (and produced its
/// [`PruneReport`]) — the attribution dimension for `loom stats` and
/// trace consumers (item 20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneTrigger {
    /// The estimated input crossed the window-relative trigger.
    Window,
    /// Turn-age masking decided below the window trigger (roadmap
    /// item 10).
    AgeMask,
    /// The budget-exceeded rescue: the unpruned projection could not
    /// fit, so the wrapper decided stubs ahead of any trigger.
    Rescue,
}

/// One stub decision paired with its estimated reclaim — the unit of
/// per-mechanism attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedCall {
    /// The call whose result was stubbed.
    pub call_id: String,
    /// Why — the mechanism that took it.
    pub reason: StubReason,
    /// Estimated tokens the stub reclaimed versus the full result.
    pub reclaimed_tokens: u64,
}

/// Attribution for one checkpoint freeze, reported on the event stream
/// as `ContextPruned` (event schema v3) exactly once — at the decision
/// moment. Later compiles that merely re-apply the frozen stubs report
/// nothing; summing `reclaimed_tokens` across `ContextPruned` events
/// (deduplicating by `call_id`) attributes savings per mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    /// The arm that fired.
    pub trigger: PruneTrigger,
    /// Per-call attribution, in transcript order.
    pub calls: Vec<PrunedCall>,
    /// Sum of `calls`' reclaims, precomputed for cheap consumers.
    pub reclaimed_tokens: u64,
}
