use std::collections::HashMap;

use crate::model::StopReason;
use crate::session::Session;
use crate::tool::{PathAccess, ToolEffects, ToolRegistry};
use crate::transcript::{Item, ToolArguments, ToolCallId, ToolStatus};

use super::config::PruneConfig;
use super::elision::elide_text;
use crate::context::default::serialized_size;
use crate::context::{TokenEstimator, estimate_serialized};

/// Facts gathered from the transcript for the decision pass.
pub(super) struct ResultFacts {
    pub(super) index: usize,
    pub(super) turn: usize,
    pub(super) call_id: ToolCallId,
    pub(super) status: ToolStatus,
    /// Serialized byte size — ordering only (the age-out sort).
    pub(super) bytes: u64,
    /// Estimated tokens of the serialized item — the reclaim math.
    pub(super) tokens: u32,
    /// Tool name + canonical arguments, when the call is known and
    /// read-only; `None` otherwise (unknown or mutating — never a dedupe,
    /// supersede or age-out candidate). An index into
    /// [`TranscriptFacts::call_meta`] — the metadata is owned once per
    /// call, not cloned per result (review item 3.2).
    pub(super) read_call: Option<usize>,
}

/// Facts about one assistant tool call, for argument elision.
pub(super) struct CallFacts<'a> {
    /// Transcript index of the assistant item carrying the call.
    pub(super) index: usize,
    /// Assistant turn the call belongs to (1-based).
    pub(super) turn: usize,
    pub(super) call_id: &'a str,
    /// Borrow of the call's immutable transcript arguments: the
    /// transcript is append-only for this compile, and elision builds
    /// an owned replacement only when one is chosen.
    pub(super) arguments: &'a ToolArguments,
}

/// Facts about one assistant reasoning trace, for reasoning elision.
pub(super) struct ReasoningFacts {
    /// Transcript index of the assistant item carrying the trace.
    pub(super) index: usize,
    /// Assistant turn the item belongs to (1-based).
    pub(super) turn: usize,
    /// Ordinal among *non-`ContentFilter`* assistant items (0-based) —
    /// the key the checkpoint freezes; the projection drops filtered
    /// turns, so plain transcript order would misalign with request
    /// messages.
    pub(super) ordinal: usize,
    /// Estimated tokens of the serialized trace — the reclaim math.
    pub(super) tokens: u32,
}

/// Facts about one assistant text, for last-resort text elision. Same
/// shape and ordinality as [`ReasoningFacts`].
pub(super) struct AssistantTextFacts {
    /// Transcript index of the assistant item carrying the text.
    pub(super) index: usize,
    /// Assistant turn the item belongs to (1-based).
    pub(super) turn: usize,
    /// Ordinal among *non-`ContentFilter`* assistant items (0-based).
    pub(super) ordinal: usize,
    /// Character length of the text — the elision-candidate gate.
    pub(super) chars: usize,
    /// Estimated tokens of the elided signature — the reclaim math
    /// subtracts this from the full text's estimate.
    pub(super) signature_tokens: u32,
}

/// Per-call metadata gathered in scan 1 and referenced by index from
/// every result of that call — `name`, canonical `args`, and the
/// declared `reads` were previously cloned as a triple into each
/// `ResultFacts` (review item 3.2).
pub(super) struct CallMeta {
    pub(super) name: String,
    pub(super) args: String,
    pub(super) reads: Vec<String>,
}

/// Everything the strategy passes reason over, collected in two
/// transcript scans: per-result facts, per-call facts, assistant turn
/// boundaries, user-item ordinals, and the writes that can supersede
/// earlier reads.
pub(super) struct TranscriptFacts<'a> {
    pub(super) results: Vec<ResultFacts>,
    pub(super) calls: Vec<CallFacts<'a>>,
    /// One entry per tool call, referenced by `ResultFacts::read_call`.
    pub(super) call_meta: Vec<CallMeta>,
    pub(super) reasoning_at: Vec<ReasoningFacts>,
    pub(super) assistant_text_at: Vec<AssistantTextFacts>,
    /// Transcript index of every assistant item — the turn boundaries.
    pub(super) assistant_at: Vec<usize>,
    /// Transcript indexes of user items, in order — the position in
    /// this list *is* the ordinal the checkpoint freezes.
    pub(super) user_at: Vec<usize>,
    /// `(index, effects)` per tool result that wrote or deleted; the
    /// effects borrow the transcript, which is append-only for this
    /// compile (consumers read the paths, never own them).
    pub(super) writes: Vec<(usize, &'a ToolEffects)>,
    /// Assistant turn of the most recent write/delete, for writing-phase
    /// detection.
    pub(super) last_write_turn: Option<usize>,
}

impl<'a> TranscriptFacts<'a> {
    /// The two fact-gathering scans behind [`decide`](super::strategies::decide).
    // The two scans are one walk over the transcript in lockstep;
    // splitting them would clone the walk skeleton twice.
    #[allow(clippy::too_many_lines)]
    pub(super) fn collect(
        session: &'a Session,
        registry: &ToolRegistry,
        estimator: &dyn TokenEstimator,
    ) -> Self {
        let transcript: &[Item] = &session.transcript;

        // Scan 1 — call metadata (name, canonical args, declared reads)
        // from assistant tool_calls, keyed by call id for O(1) lookup.
        // Owned exactly once here; scan 2 stores only the index
        // (review item 3.2: no per-result triple clone).
        let mut call_meta: Vec<CallMeta> = Vec::new();
        let mut meta_by_id: HashMap<&str, usize> = HashMap::new();
        for item in transcript {
            if let Item::Assistant(assistant) = item {
                for call in &assistant.tool_calls {
                    let reads = read_paths(registry, &call.name, &call.arguments);
                    meta_by_id.insert(call.id.as_str(), call_meta.len());
                    call_meta.push(CallMeta {
                        name: call.name.clone(),
                        args: call.arguments.to_string(),
                        reads,
                    });
                }
            }
        }
        let is_read_only = |name: &str| {
            registry
                .get(name)
                .is_some_and(crate::tool::Tool::is_read_only)
        };

        // Scan 2 — tool-result and call facts, assistant turn
        // boundaries, user ordinals, later writes.
        let mut assistant_at: Vec<usize> = Vec::new();
        let mut results: Vec<ResultFacts> = Vec::new();
        let mut call_facts: Vec<CallFacts> = Vec::new();
        let mut reasoning_at: Vec<ReasoningFacts> = Vec::new();
        let mut assistant_text_at: Vec<AssistantTextFacts> = Vec::new();
        let mut user_at: Vec<usize> = Vec::new();
        let mut writes: Vec<(usize, &ToolEffects)> = Vec::new();
        let mut last_write_turn: Option<usize> = None;
        let mut turn = 0usize;
        let mut assistant_ordinal = 0usize;
        for (index, item) in transcript.iter().enumerate() {
            match item {
                Item::Assistant(assistant) => {
                    turn += 1;
                    assistant_at.push(index);
                    for call in &assistant.tool_calls {
                        call_facts.push(CallFacts {
                            index,
                            turn,
                            call_id: call.id.as_str(),
                            arguments: &call.arguments,
                        });
                    }
                    if assistant.stop_reason != StopReason::ContentFilter {
                        if let Some(trace) = &assistant.reasoning {
                            reasoning_at.push(ReasoningFacts {
                                index,
                                turn,
                                ordinal: assistant_ordinal,
                                tokens: estimate_serialized(estimator, trace),
                            });
                        }
                        if !assistant.content.is_empty() {
                            assistant_text_at.push(AssistantTextFacts {
                                index,
                                turn,
                                ordinal: assistant_ordinal,
                                chars: assistant.content.chars().count(),
                                signature_tokens: estimate_serialized(
                                    estimator,
                                    &elide_text(&assistant.content),
                                ),
                            });
                        }
                        assistant_ordinal += 1;
                    }
                }
                Item::ToolResult(result) => {
                    let read_call = meta_by_id
                        .get(result.call_id.as_str())
                        .copied()
                        .filter(|&meta| is_read_only(&call_meta[meta].name));
                    if !result.effects.files_written.is_empty()
                        || !result.effects.files_deleted.is_empty()
                    {
                        writes.push((index, &result.effects));
                        last_write_turn = Some(turn);
                    }
                    results.push(ResultFacts {
                        index,
                        turn,
                        call_id: result.call_id.clone(),
                        status: result.status,
                        bytes: serialized_size(item),
                        tokens: estimate_serialized(estimator, item),
                        read_call,
                    });
                }
                Item::User { .. } => user_at.push(index),
            }
        }
        Self {
            results,
            calls: call_facts,
            call_meta,
            reasoning_at,
            assistant_text_at,
            assistant_at,
            user_at,
            writes,
            last_write_turn,
        }
    }

    /// Resolves a `ResultFacts::read_call` index to its call metadata.
    pub(super) fn read_meta(&self, result: &ResultFacts) -> Option<&CallMeta> {
        result.read_call.map(|index| &self.call_meta[index])
    }

    /// Index of the first unprotected item: the protected tail reaches
    /// back over the last `protected_tail_turns` turns AND at least
    /// `protected_tail_tokens` estimated trailing tokens — whichever
    /// reaches further back.
    pub(super) fn protected_from(
        &self,
        transcript: &[Item],
        config: PruneConfig,
        estimator: &dyn TokenEstimator,
    ) -> usize {
        let turn_cut = self
            .assistant_at
            .len()
            .checked_sub(config.protected_tail_turns)
            .and_then(|from| self.assistant_at.get(from))
            .copied()
            .unwrap_or(0);
        let mut trailing = 0u64;
        let mut token_cut = transcript.len();
        for (index, item) in transcript.iter().enumerate().rev() {
            trailing += u64::from(estimate_serialized(estimator, item));
            if trailing >= u64::from(config.protected_tail_tokens) {
                token_cut = index;
                break;
            }
        }
        turn_cut.min(token_cut)
    }
}

/// The assistant turn a transcript index belongs to (the count of
/// assistant items at or before it).
pub(super) fn turn_of(facts: &TranscriptFacts, index: usize) -> usize {
    facts.assistant_at.partition_point(|&at| at <= index)
}

/// The declared read paths of a call, when the tool is registered,
/// read-only, and describes its paths.
fn read_paths(registry: &ToolRegistry, name: &str, arguments: &serde_json::Value) -> Vec<String> {
    let Some(tool) = registry.get(name) else {
        return Vec::new();
    };
    tool.touched_paths(arguments)
        .unwrap_or_default()
        .into_iter()
        .filter(|touch| touch.access == PathAccess::Read)
        .map(|touch| touch.path)
        .collect()
}
