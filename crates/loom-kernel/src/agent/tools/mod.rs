//! Tool engine: the single choke point for executing model-requested
//! tool calls (architecture.md §17).
//!
//! [`execute_call`] implements the fixed pipeline — lookup, exposure
//! check, start marking, timeout-enforced execution, output bounding —
//! so a future permissions/policy hook has one place to live. It is
//! infallible at the engine level: every failure path produces a
//! [`ToolResult`] the model can read (§9.3), never a panic and never
//! a dropped call.
//!
//! [`ToolBatch`] executes the calls of one assistant turn with the
//! concurrency rules of architecture.md §10 and ADR-018: touched paths
//! are derived from the tool engine (never from model declarations), calls
//! with disjoint paths run concurrently up to a bound, and any shared path
//! keeps model order. Results return in the model's call order
//! (`buffered`), and cancellation preserves completed results while
//! marking the rest [`ToolStatus::Cancelled`] with an honest
//! `started` flag (§6.5).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::{self, Either};
use futures::stream::{self, StreamExt};
use tokio::time::timeout;

use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::tool::{ToolContext, ToolEffects, ToolError, ToolRegistry};
use crate::transcript::{ToolCall, ToolFailure, ToolResult, ToolStatus};

mod result;
mod schedule;
#[cfg(test)]
mod tests;

use result::{
    bound, cache_result, cacheable_reads, cached_result, cancelled_result, finish,
    unknown_tool_content,
};
use schedule::{CallAccess, call_access, schedule};

/// Upper bound on tool output entering the model context
/// (architecture.md §8.1.4). 64 KiB keeps one runaway tool from
/// consuming the context window while staying far above anything a
/// well-behaved tool returns.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// Default concurrency bound for [`ToolBatch::execute`] when not
/// overridden (architecture.md §10).
pub const DEFAULT_MAX_CONCURRENCY: usize = 8;

/// The set of tool names exposed to the model in one request
/// (architecture.md §8.2). Registration alone is not exposure: a call
/// to a registered-but-not-exposed tool is rejected with
/// [`ToolFailure::NotExposed`], so the context compiler remains the
/// only selection seam.
#[derive(Debug, Clone, Default)]
pub struct ToolExposure {
    names: HashSet<String>,
}

impl ToolExposure {
    /// Builds an exposure set from tool names (e.g. the `name` of each
    /// spec in a [`crate::model::ModelRequest`]).
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether `name` was exposed in this request.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Number of exposed tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether no tools are exposed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Executes one tool call through the fixed engine pipeline and
/// returns its [`ToolResult`] — never panics, never fails outright:
/// engine-level failures become results the model reads (§9.3).
///
/// `started` is set the moment [`crate::tool::Tool::call`] is about
/// to be invoked — after lookup and exposure (and a pre-cancellation
/// check), so calls rejected there report `started: false` under
/// cancellation (§6.5).
///
/// Input validation is each tool's own first step *inside*
/// `Tool::call`, so it runs after `started` is set: a call cancelled
/// while its arguments are being parsed reports `started: true`. That
/// over-states rather than under-states possible side effects, which
/// §6.5 prefers — the engine deliberately does not guess a tool's
/// argument rules.
///
/// The timeout bounds the *agent's wait* (§6.6): on elapse the result
/// is [`ToolFailure::Timeout`] and the underlying future is dropped,
/// but non-cancellable work a tool spawned internally may still be
/// running — honoring `ToolContext::cancellation` is the tool's
/// contract.
pub async fn execute_call(
    registry: &ToolRegistry,
    exposure: &ToolExposure,
    call: &ToolCall,
    ctx: &ToolContext,
    started: &AtomicBool,
) -> ToolResult {
    let Some(tool) = registry.get(&call.name) else {
        return finish(
            call,
            ToolStatus::Error(ToolFailure::UnknownTool),
            unknown_tool_content(registry, &call.name),
            false,
            ToolEffects::none(),
            None,
        );
    };
    if !exposure.contains(&call.name) {
        return finish(
            call,
            ToolStatus::Error(ToolFailure::NotExposed),
            format!(
                "error: tool \"{}\" is not exposed in this request",
                call.name
            ),
            false,
            ToolEffects::none(),
            None,
        );
    }
    // A tool may declare a longer budget than the engine default: a full
    // `cargo test` outlives 120 s, and 7 of the 124 tool failures in the
    // pre-fix sweep were `run_command` timing out at exactly that cap.
    // Shadowing the context keeps the tool's own view of its budget and
    // the enforcement below in agreement.
    let ctx = ctx
        .clone()
        .with_timeout(tool.timeout().unwrap_or(ctx.timeout));

    if ctx.cancellation.is_cancelled() {
        return cancelled_result(call, false);
    }
    started.store(true, Ordering::Release);

    // Serve a repeated read-only call from the cache when the files it
    // reads are unchanged (ADR-018, V14); a hit is re-bounded exactly as a
    // fresh call would be. `cacheable` is `None` for anything the cache
    // must not serve (mutating tools, wildcards, write-declaring reads).
    let cacheable = cacheable_reads(tool, &call.arguments);
    if let Some(reads) = &cacheable
        && let Some(result) = cached_result(reads, call, &ctx).await
    {
        return result;
    }

    let mut timed = std::pin::pin!(timeout(
        ctx.timeout,
        tool.call(call.arguments.clone().into(), &ctx)
    ));
    let mut cancelled = std::pin::pin!(ctx.cancellation.cancelled());
    match future::select(cancelled.as_mut(), timed.as_mut()).await {
        // Cancelled mid-flight: the tool future is dropped here.
        Either::Left(((), _timed)) => cancelled_result(call, true),
        Either::Right((Ok(Ok(output)), _)) => {
            // Cache read-only, side-effect-free results before bounding, so
            // a later hit is re-bounded exactly like a fresh call (ADR-018,
            // V14). `store` itself refuses outputs with effects, and the
            // signature is computed on a blocking thread, outside the lock.
            if let Some(reads) = &cacheable {
                cache_result(reads, call, &output, &ctx).await;
            }
            let (content, truncated, effects, artifact, notices) = bound(output, &ctx);
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
        }
        Either::Right((Ok(Err(ToolError::InvalidInput(msg))), _)) => finish(
            call,
            ToolStatus::Error(ToolFailure::InvalidInput),
            format!("error: invalid input: {msg}"),
            false,
            ToolEffects::none(),
            None,
        ),
        Either::Right((Ok(Err(ToolError::Execution(msg))), _)) => finish(
            call,
            ToolStatus::Error(ToolFailure::Execution),
            format!("error: execution failed: {msg}"),
            false,
            ToolEffects::none(),
            None,
        ),
        Either::Right((Ok(Err(ToolError::Declined(msg))), _)) => finish(
            call,
            ToolStatus::Error(ToolFailure::Declined),
            format!("error: {msg}"),
            false,
            ToolEffects::none(),
            None,
        ),
        Either::Right((Err(_elapsed), _)) => finish(
            call,
            ToolStatus::Error(ToolFailure::Timeout),
            format!(
                "error: tool \"{}\" timed out after {:?}",
                call.name, ctx.timeout
            ),
            false,
            ToolEffects::none(),
            None,
        ),
    }
}

/// Executes the tool calls of one assistant turn under the §10 rules and
/// ADR-018:
///
/// * calls whose touched-path sets are disjoint run concurrently, bounded
///   by `max_concurrency` (default [`DEFAULT_MAX_CONCURRENCY`]);
/// * a shared path keeps model order — a conflicting later call runs in a
///   strictly later wave, so a read after a write sees the write;
/// * a mutating call that cannot describe its paths (e.g. `run_command`)
///   is treated as a wildcard and serializes against the rest;
/// * results always in the model's call order (`buffered`), failures
///   included — no early abort;
/// * on cancellation: results already written are preserved verbatim,
///   in-flight calls become `Cancelled { started: true }`, calls the
///   bounded stream never polled become `Cancelled { started: false }`
///   (§6.5).
#[derive(Debug, Clone)]
pub struct ToolBatch<'a> {
    registry: &'a ToolRegistry,
    exposure: &'a ToolExposure,
    max_concurrency: usize,
    progress: Option<ToolProgress<'a>>,
}

/// The event seam for live per-call lifecycle updates. A batch without
/// one stays silent, which keeps the tool-engine unit tests independent
/// of the event layer.
#[derive(Clone, Copy)]
struct ToolProgress<'a> {
    sink: &'a dyn EventSink,
    task_id: TaskId,
}

impl std::fmt::Debug for ToolProgress<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolProgress")
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

impl<'a> ToolBatch<'a> {
    /// Creates a batch over `registry` with the given exposure set and
    /// the default concurrency bound.
    #[must_use]
    pub fn new(registry: &'a ToolRegistry, exposure: &'a ToolExposure) -> Self {
        Self {
            registry,
            exposure,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            progress: None,
        }
    }

    /// Overrides the concurrency bound (clamped to at least 1).
    #[must_use]
    pub fn with_max_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max.max(1);
        self
    }

    /// Reports start and completion events as each call reaches them.
    /// Without this the batch emits nothing and the caller owns the
    /// event timeline.
    #[must_use]
    pub fn with_events(mut self, sink: &'a dyn EventSink, task_id: TaskId) -> Self {
        self.progress = Some(ToolProgress { sink, task_id });
        self
    }

    /// Executes `calls` under `ctx`; see the type docs. The returned
    /// vector has one result per call, in call order.
    pub async fn execute(&self, calls: &[ToolCall], ctx: &ToolContext) -> Vec<ToolResult> {
        // Derive each call's touched paths (ADR-018) and schedule into
        // waves: within a wave every pair is conflict-free, and conflicting
        // calls are always in strictly increasing waves, so model order is
        // preserved for anything that shares a path.
        let accesses: Vec<CallAccess> = calls
            .iter()
            .map(|call| call_access(call, self.registry, self.exposure))
            .collect();
        let waves = schedule(&accesses);

        // Each future writes its result into its slot on completion, so
        // results completed but not yet yielded when cancellation fires are
        // still recoverable.
        let slots: Vec<Arc<Mutex<Option<ToolResult>>>> =
            calls.iter().map(|_| Arc::new(Mutex::new(None))).collect();
        let started_flags: Vec<Arc<AtomicBool>> = calls
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        // Whether this call already emitted its completion. In-flight
        // calls dropped by cancellation, and calls the bound never
        // polled, get one final completion after the waves.
        let completed_flags: Vec<Arc<AtomicBool>> = calls
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();

        'waves: for wave in &waves {
            // Wave futures are pushed through a named `async fn`, not a
            // closure returning an async block: the closure shape is
            // unprovable for rustc (#102211) the moment an execute
            // future must be `Send` — which any tool that nests a child
            // `Agent::run_task` (the scout) requires. The loop shape
            // borrows with `execute`'s own lifetimes and type-checks.
            let mut wave_futures = Vec::with_capacity(wave.len());
            for &index in wave {
                wave_futures.push(run_wave_item(
                    self.registry,
                    self.exposure,
                    &calls[index],
                    ctx,
                    &started_flags[index],
                    &slots[index],
                    self.progress,
                    &completed_flags[index],
                ));
            }
            let mut stream = stream::iter(wave_futures).buffered(self.max_concurrency);

            let mut remaining = wave.len();
            while remaining > 0 {
                let mut cancelled = std::pin::pin!(ctx.cancellation.cancelled());
                match future::select(cancelled.as_mut(), stream.next()).await {
                    Either::Right((Some(()), _)) => remaining -= 1,
                    Either::Right((None, _)) => break,
                    Either::Left(_) => {
                        drop(stream);
                        break 'waves;
                    }
                }
            }
        }

        let results: Vec<ToolResult> = calls
            .iter()
            .zip(slots.iter().zip(started_flags.iter()))
            .map(
                |(call, (slot, started_flag))| match lock_slot(slot).take() {
                    Some(result) => result,
                    None => cancelled_result(call, started_flag.load(Ordering::Acquire)),
                },
            )
            .collect();

        if let Some(progress) = self.progress {
            for (call, (completed, result)) in
                calls.iter().zip(completed_flags.iter().zip(results.iter()))
            {
                if !completed.load(Ordering::Acquire) {
                    progress.sink.emit(
                        progress.task_id,
                        EventKind::ToolCompleted {
                            name: call.name.clone(),
                            call_id: call.id.clone(),
                            status: result.status,
                            content_head: crate::event::text_head(
                                &result.content,
                                crate::event::CONTENT_EVENT_HEAD,
                            ),
                        },
                    );
                }
            }
        }
        results
    }
}

/// One scheduled wave item: run the call, record its effects, and
/// publish the result into its slot. Extracted from
/// [`ToolBatch::execute`] so the wave futures are named-fn futures
/// (see the wave-loop comment for the rustc #102211 details).
#[allow(clippy::too_many_arguments)] // one future per call shares the batch context
async fn run_wave_item(
    registry: &ToolRegistry,
    exposure: &ToolExposure,
    call: &ToolCall,
    ctx: &ToolContext,
    started: &AtomicBool,
    slot: &Mutex<Option<ToolResult>>,
    progress: Option<ToolProgress<'_>>,
    completed: &AtomicBool,
) {
    if let Some(progress) = progress {
        progress.sink.emit(
            progress.task_id,
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
    let result = execute_call(registry, exposure, call, ctx, started).await;
    // Record file effects before the next call in a sequential batch
    // starts, so a `verify` later in this same batch links to a write
    // earlier in it (same-batch linkage, 2F). Conflict-free waves
    // record no conflicting effects.
    ctx.note_effects(&result.effects);
    // Drop cached reads that this call invalidated: a file mutation
    // clears entries whose paths overlap it, and a `run_command`
    // clears the whole cache (ADR-018, V14).
    if let Some(cache) = ctx.cache.as_ref()
        && let Ok(mut cache) = cache.lock()
    {
        cache.invalidate_effects(&result.effects);
    }
    if let Some(progress) = progress {
        if started.load(Ordering::Acquire)
            && registry
                .get(&call.name)
                .is_some_and(|tool| tool.requires_approval(&call.arguments))
        {
            progress.sink.emit(
                progress.task_id,
                EventKind::ApprovalDecided {
                    call_id: call.id.clone(),
                    allowed: !matches!(result.status, ToolStatus::Error(ToolFailure::Declined)),
                },
            );
        }
        progress.sink.emit(
            progress.task_id,
            EventKind::ToolCompleted {
                name: call.name.clone(),
                call_id: call.id.clone(),
                status: result.status,
                content_head: crate::event::text_head(
                    &result.content,
                    crate::event::CONTENT_EVENT_HEAD,
                ),
            },
        );
        completed.store(true, Ordering::Release);
    }
    // Same wedged-lock policy as `lock_slot`: poison-tolerant.
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(result);
}

/// Locks a result slot, surviving poisoning (a panicked tool future
/// must not poison cancellation assembly).
pub(super) fn lock_slot(slot: &Mutex<Option<ToolResult>>) -> MutexGuard<'_, Option<ToolResult>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
