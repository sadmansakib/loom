//! Tool-result shaping: output bounding and artifact spill (ADR-014),
//! the read-only result cache (ADR-018, V14), and the shared result
//! assemblers every execution path funnels through (§9.3).

use crate::artifact::ArtifactRef;
use crate::tool::{PathAccess, Tool, ToolContext, ToolEffects, ToolOutput, ToolRegistry};
use crate::transcript::{ToolCall, ToolResult, ToolStatus};

use super::MAX_TOOL_OUTPUT_BYTES;

/// `error: unknown tool "…"` content with the registered names in
/// registration order (the §9.3 example format).
pub(super) fn unknown_tool_content(registry: &ToolRegistry, name: &str) -> String {
    let mut content = if registry.is_empty() {
        format!("error: unknown tool \"{name}\"; no tools registered")
    } else {
        let names: Vec<&str> = registry.specs().map(|spec| spec.name.as_str()).collect();
        format!(
            "error: unknown tool \"{name}\"; available: {}",
            names.join(", ")
        )
    };
    // A stop-like name is the model reaching for another harness's
    // `finish`; the tool list alone does not say what replaces it (three
    // such calls in the pre-fix sweep).
    if is_stop_like(name) {
        content.push_str(
            "\nThere is no tool for ending a task: a turn ends by replying with no tool call.",
        );
    }
    content
}

/// Whether an unknown tool name is the model trying to stop the run
/// (SWE-agent spells it `finish`) rather than a mistyped tool.
fn is_stop_like(name: &str) -> bool {
    matches!(
        name,
        "finish" | "done" | "submit" | "complete" | "task_complete"
    )
}

/// Serves a repeat read-only call from the cache (ADR-018, V14).
///
/// Cheap existence probe first — a key never stored skips the signature
/// walk entirely (the common first-read case). The signature walk is
/// file I/O + SHA-256, so it runs on a blocking thread, and the cache
/// lock is taken only for the in-memory map operations (ADR-018:
/// hashing never runs on an async worker or under the lock).
///
/// Roadmap item 6: when the original result is still in the projected
/// request, a repeat is served as a cheap stub — the full content is
/// already visible to the model, so re-serving a bounded duplicate
/// spends context twice. When compaction dropped the original, the full
/// (re-bounded) content is served as before. `None` means no hit; the
/// call runs for real.
pub(super) async fn cached_result(
    reads: &[String],
    call: &ToolCall,
    ctx: &ToolContext,
) -> Option<ToolResult> {
    let cache = ctx.cache.as_ref()?;
    if !cache
        .lock()
        .map(|cache| cache.has_entry(&call.name, &call.arguments))
        .unwrap_or(false)
    {
        return None;
    }
    let root = ctx.workspace_root.clone();
    let reads = reads.to_vec();
    let current =
        tokio::task::spawn_blocking(move || crate::tool_cache::compute_signature(&root, &reads))
            .await
            .ok()
            .flatten();
    let (output, original_id) = cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.lookup(&call.name, &call.arguments, current))?;
    Some(if ctx.projected_calls.contains(&original_id) {
        finish(
            call,
            ToolStatus::Success,
            format!("unchanged since call {original_id}"),
            false,
            ToolEffects::none(),
            None,
        )
    } else {
        let (content, truncated, effects, artifact, notices) = bound(output, ctx);
        let mut result = finish(
            call,
            ToolStatus::Success,
            content,
            truncated,
            effects,
            artifact,
        );
        result.notices = notices;
        result
    })
}

/// Caches a fresh read-only, side-effect-free output (ADR-018, V14):
/// `store` itself refuses outputs with effects, and the signature walk
/// runs on the blocking pool, outside the lock. A missing cache or a
/// poisoned lock is a silent skip — caching is an optimization, never
/// a side channel for failures.
pub(super) async fn cache_result(
    reads: &[String],
    call: &ToolCall,
    output: &ToolOutput,
    ctx: &ToolContext,
) {
    let Some(cache) = ctx.cache.as_ref() else {
        return;
    };
    let root = ctx.workspace_root.clone();
    let reads = reads.to_vec();
    let signature =
        tokio::task::spawn_blocking(move || crate::tool_cache::compute_signature(&root, &reads))
            .await
            .ok()
            .flatten();
    if let Ok(mut cache) = cache.lock() {
        cache.store(
            &call.name,
            &call.arguments,
            signature,
            call.id.as_str(),
            output,
        );
    }
}

/// The `kind` recorded on artifacts spilled from oversized tool output.
pub(super) const ARTIFACT_KIND: &str = "tool_output";

/// Applies [`MAX_TOOL_OUTPUT_BYTES`] on a char boundary; a tool-set
/// `truncated` flag is never cleared. Effects and notices pass through
/// untouched — truncating text never changes what the call did, and the
/// harness observations a tool attached are trace-side, not content.
///
/// Output over the bound prefers spilling the whole body into the
/// artifact store over silently cutting it (ADR-014): the model gets a
/// bounded head plus a note naming the artifact it can refetch with
/// `artifact_read`. The spill is best-effort — without a store, or if
/// the store refuses the write, the result is the plain truncation it
/// would have been before artifacts existed.
pub(super) fn bound(
    output: ToolOutput,
    ctx: &ToolContext,
) -> (
    String,
    bool,
    ToolEffects,
    Option<ArtifactRef>,
    Vec<crate::tool::ToolNotice>,
) {
    let ToolOutput {
        content,
        truncated,
        effects,
        artifact,
        notices,
    } = output;
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return (content, truncated, effects, artifact, notices);
    }
    if artifact.is_none()
        && let Some(store) = &ctx.artifacts
        && let Ok(reference) =
            crate::artifact::store_artifact(store.as_ref(), ARTIFACT_KIND, content.as_bytes())
    {
        let note = format!(
            "\n... [full output stored as artifact {} ({} bytes); read it with artifact_read]",
            reference.id, reference.bytes
        );
        let budget = MAX_TOOL_OUTPUT_BYTES.saturating_sub(note.len());
        let mut head = content;
        let mut cut = budget.min(head.len());
        while !head.is_char_boundary(cut) {
            cut -= 1;
        }
        head.truncate(cut);
        head.push_str(&note);
        return (head, true, effects, Some(reference), notices);
    }
    let mut content = content;
    let mut cut = MAX_TOOL_OUTPUT_BYTES;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    (content, true, effects, artifact, notices)
}

/// A cancelled call's result; `started` decides which honest message
/// the model sees (§6.5).
pub(super) fn cancelled_result(call: &ToolCall, started: bool) -> ToolResult {
    let content = if started {
        "error: cancelled during execution; side effects may have occurred"
    } else {
        "error: cancelled before execution; not run"
    };
    finish(
        call,
        ToolStatus::Cancelled { started },
        content.to_owned(),
        false,
        ToolEffects::none(),
        None,
    )
}

/// Assembles and traces the final result (`tool.status`, §9.3).
pub(super) fn finish(
    call: &ToolCall,
    status: ToolStatus,
    content: String,
    truncated: bool,
    effects: ToolEffects,
    artifact: Option<ArtifactRef>,
) -> ToolResult {
    tracing::debug!(
        target: "tool",
        tool = call.name,
        call_id = call.id.as_str(),
        status = ?status,
        "tool call finished"
    );
    ToolResult {
        call_id: call.id.clone(),
        status,
        content,
        truncated,
        effects,
        artifact,
        notices: Vec::new(),
    }
}

/// The workspace paths a call reads, when the call is safely cacheable
/// (ADR-018, V14): a read-only tool that declares exactly its read paths.
/// `None` for a mutating tool, a wildcard tool (`touched_paths == None`),
/// or any tool declaring a write — none of which the cache may serve.
pub(super) fn cacheable_reads(
    tool: &dyn Tool,
    arguments: &serde_json::Value,
) -> Option<Vec<String>> {
    if !tool.is_read_only() {
        return None;
    }
    let touches = tool.touched_paths(arguments)?;
    let mut reads = Vec::with_capacity(touches.len());
    for touch in touches {
        if touch.access != PathAccess::Read {
            return None;
        }
        reads.push(touch.path);
    }
    Some(reads)
}
