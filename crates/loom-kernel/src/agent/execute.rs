//! Tool-batch execution for one turn (§10): refusal fillers for
//! ledger repeats and malformed salvages, dispatch through the tool
//! engine, and start/completion event pairing.

use super::Agent;
use super::feedback::FailureLedger;
use super::summary::Accounting;
use super::tools::{MAX_TOOL_OUTPUT_BYTES, ToolBatch, ToolExposure};
use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::model::{Message, ModelRequest};
use crate::tool::{ToolContext, ToolRegistry};
use crate::transcript::{MALFORMED_ARGUMENTS_KEY, ToolCall, ToolFailure, ToolResult, ToolStatus};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

impl Agent {
    /// Runs `symbols_overview` on `path` without spending a model call
    /// (item 30 stall-breaker). `None` when the tool is not registered
    /// or declines the file; a failure stays silent — the injection is
    /// best-effort context, never a task error.
    pub(super) async fn symbol_outline(
        &self,
        turn_tools: &ToolRegistry,
        path: &str,
        cancellation: &CancellationToken,
    ) -> Option<String> {
        let tool = turn_tools.get("symbols_overview")?;
        let input = serde_json::json!({ "path": path });
        let context = ToolContext::new(
            self.config.workspace_root.clone(),
            cancellation.clone(),
            self.tool_timeout,
        );
        let output = tool.call(input, &context).await.ok()?;
        Some(output.content)
    }

    /// Executes one assistant turn's tool calls through the tool engine
    /// (§10), emitting start/complete events and counting outcomes.
    ///
    /// Runs are bounded and order-preserving inside [`ToolBatch`]; the
    /// per-call events are emitted in call order (all starts before the
    /// batch, completes after), which is exact for the sequential path and
    /// an Iteration 1 approximation under read-only concurrency. Only calls
    /// that actually run produce start/complete events: a refused repeat
    /// emits none, and its `RepeatedCall` result is committed to the
    /// transcript instead.
    ///
    /// The loop — not the model — owns the reaction to a failure (§13):
    /// `failures` refuses an exact repeat of a call that already failed
    /// (returning [`ToolFailure::RepeatedCall`] without invoking the tool)
    /// until some call succeeds, and every failure the model sees carries a
    /// deterministic next-step hint.
    #[allow(clippy::too_many_arguments)]
    // Batch execution interleaves per-call refusals, hints, and
    // accounting invariants; a split would scatter those invariants.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_batch(
        &self,
        calls: &[ToolCall],
        request: &ModelRequest,
        stubbed: &[String],
        task_id: TaskId,
        sink: &dyn EventSink,
        cancellation: &CancellationToken,
        accounting: &mut Accounting,
        failures: &mut FailureLedger,
        recent_changes: &[String],
        todos: Arc<Mutex<crate::session::TodoList>>,
        turn_tools: &ToolRegistry,
    ) -> Vec<ToolResult> {
        let exposure = ToolExposure::from_names(request.tools.iter().map(|spec| spec.name.clone()));

        // Partition into calls the loop refuses and calls that may run.
        // Refused: salvaged calls whose arguments carry the malformed
        // marker (never dispatched — the wire committed the model's raw
        // text as a placeholder), and, when loop shaping is on, an
        // exact repeat of a failure with no success since. A refused
        // call never reaches the tool engine, so it starts nothing and
        // emits no tool events; its filler result is committed instead.
        let mut runnable: Vec<ToolCall> = Vec::new();
        let mut runnable_indices: Vec<usize> = Vec::new();
        let mut is_runnable = vec![false; calls.len()];
        for (index, call) in calls.iter().enumerate() {
            let malformed = call.arguments_are_malformed();
            if !(malformed || (self.loop_shaping && failures.refuses(call))) {
                runnable_indices.push(index);
                runnable.push(call.clone());
                is_runnable[index] = true;
            }
        }
        for call in &runnable {
            sink.emit(
                task_id,
                EventKind::ToolStarted {
                    name: call.name.clone(),
                    call_id: call.id.clone(),
                    arguments: crate::event::text_head(
                        &serde_json::to_string(&call.arguments).unwrap_or_default(),
                        crate::event::ARGUMENTS_EVENT_HEAD,
                    ),
                },
            );
        }

        // Call ids whose results the model still sees in this request
        // (post-projection). The tool engine serves a cache hit for one of
        // these as a cheap `unchanged since call <id>` stub (roadmap
        // item 6): the full content is already in view. `visible_call_ids`
        // excludes any id whose body a pruning pass replaced with a stub.
        let projected_calls = visible_call_ids(request, stubbed);

        let context = ToolContext::new(
            self.config.workspace_root.clone(),
            cancellation.clone(),
            self.tool_timeout,
        )
        .with_artifacts(self.artifacts.clone())
        .with_recent_changes(if self.loop_shaping {
            recent_changes.to_vec()
        } else {
            Vec::new()
        })
        .with_cache(Some(self.cache.clone()))
        .with_projected_calls(projected_calls)
        .with_todos(Some(todos));
        let batch =
            ToolBatch::new(turn_tools, &exposure).with_max_concurrency(self.max_tool_concurrency);
        let ran = batch.execute(&runnable, &context).await;
        // Surface bookkeeping (slice D2): a dispatched surface tool was
        // used this turn, whatever the outcome — failures count as use
        // for idleness, since the model demonstrably still sees the
        // tool. Unknown names no-op inside the surface.
        if let Some(surface) = &self.surface {
            for call in runnable {
                surface.mark_used(&call.name);
            }
        }
        // Dedupe key for harness notices within this batch: one event per
        // distinct (category, detail), not one per parallel call. The key
        // borrows from `results`, which outlives the dedupe pass.
        let mut emitted_notices: HashSet<(&str, &str)> = HashSet::new();

        // Refused calls were never handed to the engine; reconstruct every
        // result in call order from the executed set plus refusal fillers
        // (malformed-salvage refusals and ledger refusals each get their
        // own filler).
        let mut executed: HashMap<usize, ToolResult> =
            runnable_indices.iter().copied().zip(ran).collect();
        let mut results: Vec<ToolResult> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                executed.remove(&index).unwrap_or_else(|| {
                    if call.arguments_are_malformed() {
                        malformed_arguments_result(call)
                    } else {
                        repeated_call_result(call)
                    }
                })
            })
            .collect();

        for (index, (call, result)) in calls.iter().zip(results.iter_mut()).enumerate() {
            match result.status {
                ToolStatus::Success => {
                    accounting.successful_tool_calls += 1;
                    // Progress: a previously failed call is a legitimate
                    // retry again once the workspace may have changed.
                    failures.clear();
                }
                ToolStatus::Error(kind) => {
                    accounting.failed_tool_calls += 1;
                    failures.record(call);
                    let tool_hint = self
                        .registry
                        .get(&call.name)
                        .and_then(|tool| tool.failure_hint(kind));
                    append_failure_hint(&mut result.content, kind, tool_hint.as_deref());
                }
                // A cancelled call is neither a success nor a failure of the
                // tool; the task outcome records the cancellation.
                ToolStatus::Cancelled { .. } => {}
            }
            // Pair each start with a completion: refused calls emitted no
            // start, so they emit no completion either.
            if is_runnable[index] {
                let status = result.status;
                let content_head =
                    crate::event::text_head(&result.content, crate::event::CONTENT_EVENT_HEAD);
                sink.emit(
                    task_id,
                    EventKind::ToolCompleted {
                        name: call.name.clone(),
                        call_id: call.id.clone(),
                        status,
                        content_head,
                    },
                );
                // A verify-call autofix reports as its own decision event
                // (schema 14), emitted right after the call it belongs to
                // — `kept: false` is a reverted attempt, still worth a
                // trace line.
                if let Some(autofix) = &result.effects.autofix {
                    sink.emit(
                        task_id,
                        EventKind::AutofixApplied {
                            edits: autofix.edits,
                            files: autofix.files,
                            diagnostics_before: autofix.diagnostics_before,
                            diagnostics_after: autofix.diagnostics_after,
                            kept: autofix.kept,
                        },
                    );
                }
                // Harness observations ride the result into the trace:
                // one event per distinct notice in this batch (a healthy
                // default run attaches none).
                for notice in &result.notices {
                    if emitted_notices.insert((notice.category.as_str(), notice.detail.as_str())) {
                        sink.emit(
                            task_id,
                            EventKind::HarnessNotice {
                                category: notice.category.clone(),
                                detail: notice.detail.clone(),
                            },
                        );
                    }
                }
            }
        }
        results
    }
}

/// The loop's refusal result for a repeated failed call: the tool never runs,
/// so no side effect occurs, and the model is told to change approach.
pub(super) fn repeated_call_result(call: &ToolCall) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        ToolStatus::Error(ToolFailure::RepeatedCall),
        format!(
            "error: you already called \"{}\" with these exact arguments and it failed; \
             repeating it unchanged is not permitted. Reassess before calling again: read \
             the failure above, change the arguments, or use a different tool.",
            call.name
        ),
        false,
    )
}

/// The loop's refusal result for a salvaged tool call: the wire adapter
/// committed the model's malformed argument JSON as a placeholder object,
/// the tool never ran, and the model is told what happened and asked to
/// reissue a well-formed call — the field-standard recovery loop
/// (OpenAI/Anthropic agent loops feed the error back as a tool result).
pub(super) fn malformed_arguments_result(call: &ToolCall) -> ToolResult {
    let excerpt = call
        .arguments
        .get(MALFORMED_ARGUMENTS_KEY)
        .and_then(|value| value.as_str())
        .unwrap_or("");
    ToolResult::new(
        call.id.clone(),
        ToolStatus::Error(ToolFailure::InvalidInput),
        format!(
            "error: tool arguments were not a valid JSON object (raw: {excerpt}); \
             the call was not executed. Reissue it with well-formed JSON arguments.",
        ),
        false,
    )
}

/// Call ids whose results the model still sees in `request`, post-projection.
///
/// The tool engine serves a cache hit for one of these as a cheap
/// `unchanged since call <id>` stub (roadmap item 6): the full content is
/// already in view, so re-serving a bounded duplicate spends context twice.
///
/// A pruned result is the exception, and it is why `stubbed` is subtracted.
/// Pruning rewrites a result's body to a one-line "re-run the tool" note
/// while **keeping its call id** (`context::pruning::apply_stubs`), so the
/// id still appears in the projection even though its content is gone.
/// Serving `unchanged since call X` for such an id points the model at a
/// body it can no longer read — it would be told the content is available
/// at a call whose body is a stub. Excluding those ids makes a repeat read
/// fall through to the full, re-bounded cached content, which is exactly
/// what the model needs to recover what pruning took away.
pub(super) fn visible_call_ids(request: &ModelRequest, stubbed: &[String]) -> HashSet<String> {
    // Review item 3.2: build the membership index once instead of
    // rescanning every stub id for every projected tool result (O(n*m)
    // on long transcripts).
    let stubbed: HashSet<&str> = stubbed.iter().map(String::as_str).collect();
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.call_id.as_str().to_owned()),
            Message::System { .. } | Message::User { .. } | Message::Assistant(_) => None,
        })
        .filter(|call_id| !stubbed.contains(call_id.as_str()))
        .collect()
}

/// Appends a deterministic next-step hint to a failure the model will see.
/// The engine (`execute_call`) is untouched, so its content contract is
/// unchanged; the loop layers guidance on top.
pub(super) fn append_failure_hint(
    content: &mut String,
    kind: ToolFailure,
    tool_hint: Option<&str>,
) {
    let generic = match kind {
        ToolFailure::InvalidInput => {
            "Hint: correct the arguments described above before calling again."
        }
        ToolFailure::Execution => {
            "Hint: inspect the error above and change your approach; the same call will be refused."
        }
        ToolFailure::Timeout => {
            "Hint: the call timed out; narrow the operation or change approach."
        }
        ToolFailure::UnknownTool | ToolFailure::NotExposed => {
            "Hint: call one of the available tools listed above."
        }
        // The refusal result already explains itself.
        ToolFailure::RepeatedCall => return,
    };
    // A tool that knows the shape of its own governing mistakes says
    // something more specific than the loop's generic text.
    let hint = tool_hint.unwrap_or(generic);
    // The engine bounded `content` before this, so reserve room for the hint
    // and keep the result within the same output bound.
    let needed = hint.len() + 1;
    if content.len() + needed > MAX_TOOL_OUTPUT_BYTES {
        let mut cut = MAX_TOOL_OUTPUT_BYTES.saturating_sub(needed);
        while cut > 0 && !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    content.push('\n');
    content.push_str(hint);
}
