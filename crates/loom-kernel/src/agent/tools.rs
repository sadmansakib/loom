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

use crate::artifact::ArtifactRef;
use crate::tool::{
    PathAccess, PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolRegistry,
    paths_overlap,
};
use crate::transcript::{ToolCall, ToolFailure, ToolResult, ToolStatus};

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
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Number of exposed tools.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether no tools are exposed.
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
    if ctx.cancellation.is_cancelled() {
        return cancelled_result(call, false);
    }
    started.store(true, Ordering::Release);

    // Serve a repeated read-only call from the cache when the files it
    // reads are unchanged (ADR-018, V14); a hit is re-bounded exactly as a
    // fresh call would be. `cacheable` is `None` for anything the cache
    // must not serve (mutating tools, wildcards, write-declaring reads).
    let cacheable = cacheable_reads(tool, &call.arguments);
    if let (Some(reads), Some(cache)) = (&cacheable, ctx.cache.as_ref())
        && let Ok(mut cache) = cache.lock()
        && let Some(output) = cache.lookup(&ctx.workspace_root, &call.name, &call.arguments, reads)
    {
        let (content, truncated, effects, artifact) = bound(output, ctx);
        return finish(
            call,
            ToolStatus::Success,
            content,
            truncated,
            effects,
            artifact,
        );
    }

    let mut timed = std::pin::pin!(timeout(
        ctx.timeout,
        tool.call(call.arguments.clone().into(), ctx)
    ));
    let mut cancelled = std::pin::pin!(ctx.cancellation.cancelled());
    match future::select(cancelled.as_mut(), timed.as_mut()).await {
        // Cancelled mid-flight: the tool future is dropped here.
        Either::Left(((), _timed)) => cancelled_result(call, true),
        Either::Right((Ok(Ok(output)), _)) => {
            // Cache read-only, side-effect-free results before bounding, so
            // a later hit is re-bounded exactly like a fresh call (ADR-018,
            // V14). `store` itself refuses outputs with effects.
            if let (Some(reads), Some(cache)) = (&cacheable, ctx.cache.as_ref())
                && let Ok(mut cache) = cache.lock()
            {
                cache.store(
                    &ctx.workspace_root,
                    &call.name,
                    &call.arguments,
                    reads,
                    &output,
                );
            }
            let (content, truncated, effects, artifact) = bound(output, ctx);
            finish(
                call,
                ToolStatus::Success,
                content,
                truncated,
                effects,
                artifact,
            )
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

/// `error: unknown tool "…"` content with the registered names in
/// registration order (the §9.3 example format).
fn unknown_tool_content(registry: &ToolRegistry, name: &str) -> String {
    if registry.is_empty() {
        format!("error: unknown tool \"{name}\"; no tools registered")
    } else {
        let names: Vec<&str> = registry.specs().map(|spec| spec.name.as_str()).collect();
        format!(
            "error: unknown tool \"{name}\"; available: {}",
            names.join(", ")
        )
    }
}

/// The `kind` recorded on artifacts spilled from oversized tool output.
const ARTIFACT_KIND: &str = "tool_output";

/// Applies [`MAX_TOOL_OUTPUT_BYTES`] on a char boundary; a tool-set
/// `truncated` flag is never cleared. Effects pass through untouched —
/// truncating text never changes what the call did.
///
/// Output over the bound prefers spilling the whole body into the
/// artifact store over silently cutting it (ADR-014): the model gets a
/// bounded head plus a note naming the artifact it can refetch with
/// `artifact_read`. The spill is best-effort — without a store, or if
/// the store refuses the write, the result is the plain truncation it
/// would have been before artifacts existed.
fn bound(
    output: ToolOutput,
    ctx: &ToolContext,
) -> (String, bool, ToolEffects, Option<ArtifactRef>) {
    let ToolOutput {
        content,
        truncated,
        effects,
        artifact,
    } = output;
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return (content, truncated, effects, artifact);
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
        return (head, true, effects, Some(reference));
    }
    let mut content = content;
    let mut cut = MAX_TOOL_OUTPUT_BYTES;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    (content, true, effects, artifact)
}

/// A cancelled call's result; `started` decides which honest message
/// the model sees (§6.5).
fn cancelled_result(call: &ToolCall, started: bool) -> ToolResult {
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
fn finish(
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
        }
    }

    /// Overrides the concurrency bound (clamped to at least 1).
    #[must_use]
    pub fn with_max_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max.max(1);
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

        'waves: for wave in &waves {
            let mut stream = stream::iter(wave.iter().map(|&index| {
                let call = &calls[index];
                let slot = &slots[index];
                let started = &started_flags[index];
                async move {
                    let result =
                        execute_call(self.registry, self.exposure, call, ctx, started).await;
                    // Record file effects before the next call in a
                    // sequential batch starts, so a `verify` later in this
                    // same batch links to a write earlier in it
                    // (same-batch linkage, 2F). Conflict-free waves record
                    // no conflicting effects.
                    ctx.note_effects(&result.effects);
                    // Drop cached reads that this call invalidated: a file
                    // mutation clears entries whose paths overlap it, and a
                    // `run_command` clears the whole cache (ADR-018, V14).
                    if let Some(cache) = ctx.cache.as_ref()
                        && let Ok(mut cache) = cache.lock()
                    {
                        cache.invalidate_effects(&result.effects);
                    }
                    lock_slot(slot).replace(result);
                }
            }))
            .buffered(self.max_concurrency);

            let mut remaining = wave.len();
            while remaining > 0 {
                let mut cancelled = std::pin::pin!(ctx.cancellation.cancelled());
                match future::select(cancelled.as_mut(), stream.next()).await {
                    Either::Right((Some(_), _)) => remaining -= 1,
                    Either::Right((None, _)) => break,
                    Either::Left(_) => {
                        drop(stream);
                        break 'waves;
                    }
                }
            }
        }

        calls
            .iter()
            .zip(slots.iter().zip(started_flags.iter()))
            .map(
                |(call, (slot, started_flag))| match lock_slot(slot).take() {
                    Some(result) => result,
                    None => cancelled_result(call, started_flag.load(Ordering::Acquire)),
                },
            )
            .collect()
    }
}

/// The workspace paths a call reads, when the call is safely cacheable
/// (ADR-018, V14): a read-only tool that declares exactly its read paths.
/// `None` for a mutating tool, a wildcard tool (`touched_paths == None`),
/// or any tool declaring a write — none of which the cache may serve.
fn cacheable_reads(tool: &dyn Tool, arguments: &serde_json::Value) -> Option<Vec<String>> {
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

/// The paths one call will touch, split into read/write sets for the
/// scheduler. [`Access::Unknown`] is a wildcard: possible conflict with
/// anything that touches a path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallAccess {
    reads: Access,
    writes: Access,
}

/// One side of a [`CallAccess`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Access {
    /// Touches no path of this kind.
    Nothing,
    /// Cannot be described — a wildcard.
    Unknown,
    /// Exactly these workspace-relative paths.
    Known(HashSet<String>),
}

/// Derives a call's access from its tool: a not-exposed/unknown call never
/// executes and so touches nothing; a read-only tool contributes its declared
/// reads; a mutating tool contributes its declared writes and treats its own
/// reads as unknown (it may read anything before writing).
fn call_access(call: &ToolCall, registry: &ToolRegistry, exposure: &ToolExposure) -> CallAccess {
    let tool = exposure
        .contains(&call.name)
        .then(|| registry.get(&call.name))
        .flatten();
    let Some(tool) = tool else {
        return CallAccess {
            reads: Access::Nothing,
            writes: Access::Nothing,
        };
    };
    let (reads, writes) = split_access(tool.touched_paths(&call.arguments));
    if tool.is_read_only() {
        // A read-only tool never writes, whatever it declares.
        CallAccess {
            reads,
            writes: Access::Nothing,
        }
    } else {
        // A mutating tool contributes exactly the paths it declares (its
        // reads are usually empty); undeclared paths stay `Unknown` via
        // `split_access` and therefore serialize.
        CallAccess { reads, writes }
    }
}

/// Splits declared touches into read/write sets; `None` (undeclared) is a
/// wildcard on both sides.
fn split_access(declared: Option<Vec<PathTouch>>) -> (Access, Access) {
    let Some(touches) = declared else {
        return (Access::Unknown, Access::Unknown);
    };
    let mut reads = HashSet::new();
    let mut writes = HashSet::new();
    for touch in touches {
        match touch.access {
            PathAccess::Read => {
                reads.insert(touch.path);
            }
            PathAccess::Write => {
                writes.insert(touch.path);
            }
        }
    }
    (known(reads), known(writes))
}

/// A non-empty set becomes [`Access::Known`]; an empty one is
/// [`Access::Nothing`] (touches nothing).
fn known(paths: HashSet<String>) -> Access {
    if paths.is_empty() {
        Access::Nothing
    } else {
        Access::Known(paths)
    }
}

/// Whether two calls conflict: one writes a path the other touches.
fn conflicts(a: &CallAccess, b: &CallAccess) -> bool {
    writes_overlap(&a.writes, &b.reads)
        || writes_overlap(&a.writes, &b.writes)
        || writes_overlap(&b.writes, &a.reads)
}

/// Whether `writer` writes anything `other` touches.
fn writes_overlap(writer: &Access, other: &Access) -> bool {
    match (writer, other) {
        (Access::Nothing, _) | (_, Access::Nothing) => false,
        (Access::Unknown, _) | (_, Access::Unknown) => true,
        (Access::Known(written), Access::Known(touched)) => written
            .iter()
            .any(|w| touched.iter().any(|t| paths_overlap(w, t))),
    }
}

/// Assigns each call to a wave: the longest path over conflict edges. Two
/// calls share a wave only when conflict-free, and a conflict always puts the
/// later call in a strictly later wave, preserving model order on shared
/// paths. Waves run in order; calls within a wave run concurrently.
fn schedule(accesses: &[CallAccess]) -> Vec<Vec<usize>> {
    let mut level = vec![0usize; accesses.len()];
    for later in 0..accesses.len() {
        for earlier in 0..later {
            if conflicts(&accesses[earlier], &accesses[later]) {
                level[later] = level[later].max(level[earlier] + 1);
            }
        }
    }
    let mut waves: Vec<Vec<usize>> = Vec::new();
    for (index, &wave) in level.iter().enumerate() {
        if waves.len() <= wave {
            waves.resize(wave + 1, Vec::new());
        }
        waves[wave].push(index);
    }
    waves
}

/// Locks a result slot, surviving poisoning (a panicked tool future
/// must not poison cancellation assembly).
fn lock_slot(slot: &Mutex<Option<ToolResult>>) -> MutexGuard<'_, Option<ToolResult>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ArtifactStore;
    use crate::tool::{CommandEffect, PathTouch, Tool, ToolEffects, ToolOutput, ToolSpec};
    use crate::transcript::{ToolArguments, ToolCallId};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// What a [`MockTool`] does when called.
    #[derive(Clone)]
    enum Action {
        /// Succeeds with the given text.
        Text(String),
        /// Fails input validation.
        InvalidInput,
        /// Fails execution.
        Fail,
        /// Sleeps, then succeeds (timeout / ordering / cancellation
        /// substrate).
        Sleep { ms: u64 },
        /// Signals `notify`, then succeeds immediately: a deterministic
        /// sync point for the cancellation-table probe.
        SignalText {
            notify: Arc<tokio::sync::Notify>,
            text: String,
        },
        /// Tracks concurrent invocations while sleeping (concurrency
        /// bound and sequential-mode probes).
        CountSleep {
            active: Arc<AtomicUsize>,
            max_seen: Arc<AtomicUsize>,
            ms: u64,
        },
        /// Sleeps for a minute (a call that must be cancelled).
        Hang,
        /// Succeeds and reports side effects (the effects seam).
        Effects { effects: ToolEffects },
        /// Tracks concurrency and records completion order, and — when
        /// `declared` — declares its touched paths to the scheduler
        /// (ADR-018 probes).
        Track {
            reads: Vec<String>,
            writes: Vec<String>,
            declared: bool,
            active: Arc<AtomicUsize>,
            max_seen: Arc<AtomicUsize>,
            log: Arc<Mutex<Vec<String>>>,
            label: String,
            ms: u64,
        },
    }

    struct MockTool {
        spec: ToolSpec,
        read_only: bool,
        action: Action,
    }

    impl MockTool {
        fn text(name: &str, text: &str) -> Self {
            Self {
                spec: spec(name),
                read_only: true,
                action: Action::Text(text.to_owned()),
            }
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "mock tool".to_owned(),
            parameters: json!({"type": "object"}),
        }
    }

    impl Tool for MockTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn is_read_only(&self) -> bool {
            self.read_only
        }

        fn touched_paths(&self, _input: &Value) -> Option<Vec<PathTouch>> {
            match &self.action {
                Action::Track {
                    reads,
                    writes,
                    declared: true,
                    ..
                } => {
                    let mut touches: Vec<PathTouch> =
                        reads.iter().cloned().map(PathTouch::read).collect();
                    touches.extend(writes.iter().cloned().map(PathTouch::write));
                    Some(touches)
                }
                _ => None,
            }
        }

        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            let action = self.action.clone();
            Box::pin(async move {
                match action {
                    Action::Text(text) => Ok(ToolOutput::new(text)),
                    Action::InvalidInput => Err(ToolError::InvalidInput("bad args".to_owned())),
                    Action::Fail => Err(ToolError::Execution("boom".to_owned())),
                    Action::Sleep { ms } => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(ToolOutput::new("slept"))
                    }
                    Action::SignalText { notify, text } => {
                        notify.notify_one();
                        Ok(ToolOutput::new(text))
                    }
                    Action::CountSleep {
                        active,
                        max_seen,
                        ms,
                    } => {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(ToolOutput::new("counted"))
                    }
                    Action::Hang => {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Ok(ToolOutput::new("finally"))
                    }
                    Action::Effects { effects } => {
                        Ok(ToolOutput::new("touched the world").with_effects(effects))
                    }
                    Action::Track {
                        active,
                        max_seen,
                        log,
                        label,
                        ms,
                        ..
                    } => {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        log.lock().expect("log lock").push(label);
                        Ok(ToolOutput::new("tracked"))
                    }
                }
            })
        }
    }

    fn make_call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(format!("call-{name}")).expect("valid id"),
            name: name.to_owned(),
            arguments: ToolArguments::try_from(json!({})).expect("object"),
        }
    }

    fn registry(tools: Vec<MockTool>) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        for tool in tools {
            registry.register(Box::new(tool)).expect("unique name");
        }
        registry
    }

    fn test_ctx(timeout: Duration, token: &CancellationToken) -> ToolContext {
        ToolContext::new(PathBuf::new(), token.clone(), timeout)
    }

    #[test]
    fn exposure_set_membership() {
        let exposure = ToolExposure::from_names(["read_file", "list_dir"]);
        assert!(exposure.contains("read_file"));
        assert!(!exposure.contains("search"));
        assert_eq!(exposure.len(), 2);
        assert!(!exposure.is_empty());
        assert!(ToolExposure::default().is_empty());
    }

    #[tokio::test]
    async fn successful_call_persists_the_effects_it_reported() {
        let effects = ToolEffects::none()
            .with_file_written("src/lib.rs")
            .with_file_deleted("src/old.rs")
            .with_command(CommandEffect::new("cargo", vec!["test".into()], Some(101)));
        let mut tool = MockTool::text("edit_file", "ignored");
        tool.read_only = false;
        tool.action = Action::Effects {
            effects: effects.clone(),
        };
        let registry = registry(vec![tool]);
        let exposure = ToolExposure::from_names(["edit_file"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(
            &registry,
            &exposure,
            &make_call("edit_file"),
            &ctx,
            &started,
        )
        .await;
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.effects, effects);
    }

    #[tokio::test]
    async fn a_repeated_read_is_served_from_cache_until_a_write_invalidates() {
        let dir = std::env::temp_dir().join(format!("loom-tools-cache-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("src")).expect("workspace");
        std::fs::write(dir.join("src/a.rs"), "fn a() {}").expect("file");

        let log = Arc::new(Mutex::new(Vec::new()));
        let tool = MockTool {
            spec: spec("read_file"),
            read_only: true,
            action: Action::Track {
                reads: vec!["src/a.rs".to_owned()],
                writes: Vec::new(),
                declared: true,
                active: Arc::new(AtomicUsize::new(0)),
                max_seen: Arc::new(AtomicUsize::new(0)),
                log: log.clone(),
                label: "read".to_owned(),
                ms: 0,
            },
        };
        let registry = registry(vec![tool]);
        let exposure = ToolExposure::from_names(["read_file"]);
        let token = CancellationToken::new();
        let cache = Arc::new(Mutex::new(crate::tool_cache::ToolCache::new()));
        let ctx = ToolContext::new(dir.clone(), token.clone(), Duration::from_secs(5))
            .with_cache(Some(cache.clone()));
        let started = AtomicBool::new(false);

        let first = execute_call(
            &registry,
            &exposure,
            &make_call("read_file"),
            &ctx,
            &started,
        )
        .await;
        let second = execute_call(
            &registry,
            &exposure,
            &make_call("read_file"),
            &ctx,
            &started,
        )
        .await;
        assert_eq!(first.status, ToolStatus::Success);
        assert_eq!(second.status, ToolStatus::Success);
        assert_eq!(second.content, first.content);
        assert_eq!(
            log.lock().expect("log").len(),
            1,
            "the second identical read must be served from the cache"
        );
        assert_eq!(cache.lock().expect("cache").hits(), 1);

        cache
            .lock()
            .expect("cache")
            .invalidate_effects(&ToolEffects::none().with_file_written("src/a.rs"));
        let third = execute_call(
            &registry,
            &exposure,
            &make_call("read_file"),
            &ctx,
            &started,
        )
        .await;
        assert_eq!(third.status, ToolStatus::Success);
        assert_eq!(
            log.lock().expect("log").len(),
            2,
            "a recorded write to the read path invalidates the entry"
        );
        assert_eq!(third.content, first.content);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_call_that_never_ran_reports_no_effects() {
        let registry = registry(vec![MockTool::text("read_file", "unused")]);
        let exposure = ToolExposure::from_names(["read_file"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(
            &registry,
            &exposure,
            &make_call("edit_file"),
            &ctx,
            &started,
        )
        .await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::UnknownTool));
        assert!(result.effects.is_empty());
    }

    #[tokio::test]
    async fn execute_call_unknown_tool_lists_registered() {
        let registry = registry(vec![MockTool::text("read_file", "ok")]);
        let exposure = ToolExposure::from_names(["read_file"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("grep"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::UnknownTool));
        assert_eq!(
            result.content,
            "error: unknown tool \"grep\"; available: read_file"
        );
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn execute_call_unknown_tool_empty_registry() {
        let registry = registry(vec![]);
        let exposure = ToolExposure::default();
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("grep"), &ctx, &started).await;
        assert_eq!(
            result.content,
            "error: unknown tool \"grep\"; no tools registered"
        );
    }

    #[tokio::test]
    async fn execute_call_registered_but_not_exposed() {
        let registry = registry(vec![MockTool::text("read_file", "ok")]);
        let exposure = ToolExposure::from_names(["list_dir"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(
            &registry,
            &exposure,
            &make_call("read_file"),
            &ctx,
            &started,
        )
        .await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::NotExposed));
        assert_eq!(
            result.content,
            "error: tool \"read_file\" is not exposed in this request"
        );
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn execute_call_maps_tool_errors() {
        let registry = registry(vec![
            MockTool {
                spec: spec("bad"),
                read_only: true,
                action: Action::InvalidInput,
            },
            MockTool {
                spec: spec("boom"),
                read_only: true,
                action: Action::Fail,
            },
        ]);
        let exposure = ToolExposure::from_names(["bad", "boom"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);

        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("bad"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::InvalidInput));
        assert_eq!(result.content, "error: invalid input: bad args");

        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("boom"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::Execution));
        assert_eq!(result.content, "error: execution failed: boom");
        assert!(started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn execute_call_enforces_timeout() {
        let registry = registry(vec![MockTool {
            spec: spec("slow"),
            read_only: true,
            action: Action::Sleep { ms: 10_000 },
        }]);
        let exposure = ToolExposure::from_names(["slow"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_millis(25), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("slow"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Error(ToolFailure::Timeout));
        assert_eq!(result.content, "error: tool \"slow\" timed out after 25ms");
        assert!(started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn execute_call_bounds_output_at_char_boundary() {
        // 3-byte chars: 66_000 bytes total; the 65_536-byte cut lands
        // mid-char and must back off to 65_535.
        let big = "✀".repeat(22_000);
        let registry = registry(vec![MockTool::text("big", &big)]);
        let exposure = ToolExposure::from_names(["big"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("big"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.content.len(), MAX_TOOL_OUTPUT_BYTES - 1);
        assert!(result.content.is_char_boundary(result.content.len()));
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn execute_call_preserves_tool_truncated_flag() {
        // A tool that already truncated (short content, flag set):
        // the engine must not clear the flag.
        let mut registry = ToolRegistry::default();
        registry
            .register(Box::new(TruncatingTool { spec: spec("pre") }))
            .expect("unique");
        let exposure = ToolExposure::from_names(["pre"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("pre"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.content, "small");
        assert!(result.truncated);
    }

    struct TruncatingTool {
        spec: ToolSpec,
    }

    impl Tool for TruncatingTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn is_read_only(&self) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async { Ok(ToolOutput::new("small").with_truncated()) })
        }
    }

    #[tokio::test]
    async fn execute_call_pre_cancelled_never_starts() {
        let registry = registry(vec![MockTool::text("t", "ok")]);
        let exposure = ToolExposure::from_names(["t"]);
        let token = CancellationToken::new();
        token.cancel();
        let ctx = test_ctx(Duration::from_secs(5), &token);
        let started = AtomicBool::new(false);
        let result = execute_call(&registry, &exposure, &make_call("t"), &ctx, &started).await;
        assert_eq!(result.status, ToolStatus::Cancelled { started: false });
        assert_eq!(result.content, "error: cancelled before execution; not run");
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn execute_call_cancelled_mid_flight_reports_started() {
        let registry = registry(vec![MockTool {
            spec: spec("hang"),
            read_only: true,
            action: Action::Sleep { ms: 10_000 },
        }]);
        let exposure = ToolExposure::from_names(["hang"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(30), &token);
        let started = AtomicBool::new(false);
        let hang_call = make_call("hang");
        let mut call = std::pin::pin!(execute_call(
            &registry, &exposure, &hang_call, &ctx, &started
        ));
        // Let the call reach its sleep, then cancel from the outside.
        let grace = std::pin::pin!(tokio::time::sleep(Duration::from_millis(10)));
        match future::select(call.as_mut(), grace).await {
            Either::Left(_) => panic!("hang tool finished without cancellation"),
            Either::Right(_) => {}
        }
        token.cancel();
        let result = call.await;
        assert_eq!(result.status, ToolStatus::Cancelled { started: true });
        assert_eq!(
            result.content,
            "error: cancelled during execution; side effects may have occurred"
        );
        assert!(started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn batch_results_in_call_order() {
        // Later calls finish earlier; `buffered` must still yield in
        // call order.
        let tools: Vec<MockTool> = (0..6)
            .map(|i| MockTool {
                spec: spec(&format!("t{i}")),
                read_only: true,
                action: Action::Sleep { ms: (6 - i) * 5 },
            })
            .collect();
        let names: Vec<String> = tools.iter().map(|t| t.spec.name.clone()).collect();
        let registry = registry(tools);
        let exposure = ToolExposure::from_names(names.clone());
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&registry, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert_eq!(results.len(), 6);
        for (call, result) in calls.iter().zip(results.iter()) {
            assert_eq!(call.id, result.call_id);
            assert_eq!(result.status, ToolStatus::Success);
        }
    }

    #[tokio::test]
    async fn batch_respects_concurrency_bound() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let names: Vec<String> = (0..10).map(|i| format!("c{i}")).collect();
        let mut reg = ToolRegistry::default();
        for name in &names {
            reg.register(Box::new(MockTool {
                spec: spec(name),
                read_only: true,
                action: Action::CountSleep {
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    ms: 20,
                },
            }))
            .expect("unique");
        }
        let exposure = ToolExposure::from_names(names.clone());
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(4);
        let results = batch.execute(&calls, &ctx).await;
        assert_eq!(results.len(), 10);
        assert!(results.iter().all(|r| r.status == ToolStatus::Success));
        assert_eq!(max_seen.load(Ordering::SeqCst), 4, "bound must be exact");
    }

    #[tokio::test]
    async fn wildcard_mutation_runs_after_concurrent_readers() {
        // A mutating call that cannot describe its paths (no declaration)
        // is a wildcard: it conflicts with everything, but the read-only
        // calls it runs after are still concurrent with each other
        // (ADR-018).
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::default();
        let mut names: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
        for name in &names {
            reg.register(Box::new(MockTool {
                spec: spec(name),
                read_only: true,
                action: Action::Track {
                    reads: vec![format!("{name}.txt")],
                    writes: Vec::new(),
                    declared: true,
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    log: log.clone(),
                    label: name.clone(),
                    ms: 20,
                },
            }))
            .expect("unique");
        }
        names.push("writer".to_owned());
        reg.register(Box::new(MockTool {
            spec: spec("writer"),
            read_only: false,
            action: Action::Track {
                reads: Vec::new(),
                writes: Vec::new(),
                declared: false,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: "writer".to_owned(),
                ms: 1,
            },
        }))
        .expect("unique");

        let exposure = ToolExposure::from_names(names.clone());
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
        let results = batch.execute(&calls, &ctx).await;
        assert!(results.iter().all(|r| r.status == ToolStatus::Success));
        assert_eq!(max_seen.load(Ordering::SeqCst), 6);
        let entries = log.lock().expect("log lock").clone();
        assert_eq!(entries.len(), 7);
        assert_eq!(entries.last().map(String::as_str), Some("writer"));
        assert!(entries[..6].iter().all(|entry| entry != "writer"));
    }

    #[tokio::test]
    async fn disjoint_writes_run_concurrently() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::default();
        for (name, path) in [("w0", "a.txt"), ("w1", "b.txt")] {
            reg.register(Box::new(MockTool {
                spec: spec(name),
                read_only: false,
                action: Action::Track {
                    reads: Vec::new(),
                    writes: vec![path.to_owned()],
                    declared: true,
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    log: log.clone(),
                    label: name.to_owned(),
                    ms: 20,
                },
            }))
            .expect("unique");
        }
        let names = ["w0", "w1"];
        let exposure = ToolExposure::from_names(names);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert!(results.iter().all(|r| r.status == ToolStatus::Success));
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            2,
            "writes to disjoint paths must overlap"
        );
    }

    #[tokio::test]
    async fn same_path_write_then_read_keeps_model_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::default();
        reg.register(Box::new(MockTool {
            spec: spec("writer"),
            read_only: false,
            action: Action::Track {
                reads: Vec::new(),
                writes: vec!["a.txt".to_owned()],
                declared: true,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: "write".to_owned(),
                ms: 30,
            },
        }))
        .expect("unique");
        reg.register(Box::new(MockTool {
            spec: spec("reader"),
            read_only: true,
            action: Action::Track {
                reads: vec!["a.txt".to_owned()],
                writes: Vec::new(),
                declared: true,
                active: active.clone(),
                max_seen: max_seen.clone(),
                log: log.clone(),
                label: "read".to_owned(),
                ms: 1,
            },
        }))
        .expect("unique");
        let names = ["writer", "reader"];
        let exposure = ToolExposure::from_names(names);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert!(results.iter().all(|r| r.status == ToolStatus::Success));
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "a read after a write to the same path must not overlap it"
        );
        assert_eq!(
            &*log.lock().expect("log lock"),
            &["write".to_owned(), "read".to_owned()]
        );
    }

    #[tokio::test]
    async fn overlapping_writes_serialize_in_model_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::default();
        for name in ["w0", "w1"] {
            reg.register(Box::new(MockTool {
                spec: spec(name),
                read_only: false,
                action: Action::Track {
                    reads: Vec::new(),
                    writes: vec!["a.txt".to_owned()],
                    declared: true,
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    log: log.clone(),
                    label: name.to_owned(),
                    ms: 10,
                },
            }))
            .expect("unique");
        }
        let names = ["w0", "w1"];
        let exposure = ToolExposure::from_names(names);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert!(results.iter().all(|r| r.status == ToolStatus::Success));
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            &*log.lock().expect("log lock"),
            &["w0".to_owned(), "w1".to_owned()]
        );
    }

    #[tokio::test]
    async fn batch_failure_isolation() {
        let registry = registry(vec![
            MockTool::text("ok1", "first"),
            MockTool {
                spec: spec("bad"),
                read_only: true,
                action: Action::Fail,
            },
            MockTool::text("ok2", "second"),
        ]);
        let exposure = ToolExposure::from_names(["ok1", "bad", "ok2"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = ["ok1", "bad", "ok2"].map(make_call).to_vec();
        let batch = ToolBatch::new(&registry, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert_eq!(results[0].status, ToolStatus::Success);
        assert_eq!(results[0].content, "first");
        assert_eq!(results[1].status, ToolStatus::Error(ToolFailure::Execution));
        assert_eq!(results[2].status, ToolStatus::Success);
        assert_eq!(results[2].content, "second");
    }

    #[tokio::test]
    async fn batch_not_exposed_mutating_keeps_concurrent() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut names: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
        names.push("writer".to_owned());
        let mut reg = ToolRegistry::default();
        for name in &names {
            let is_writer = name == "writer";
            reg.register(Box::new(MockTool {
                spec: spec(name),
                read_only: !is_writer,
                action: Action::CountSleep {
                    active: active.clone(),
                    max_seen: max_seen.clone(),
                    ms: 10,
                },
            }))
            .expect("unique");
        }
        // `writer` is registered but NOT exposed: its call is rejected
        // without running and must not serialize the read-only rest.
        let exposure = ToolExposure::from_names(names[..6].iter().map(String::as_str));
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token);
        let calls: Vec<ToolCall> = names.iter().map(|n| make_call(n)).collect();
        let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
        let results = batch.execute(&calls, &ctx).await;
        assert_eq!(results.len(), 7);
        assert_eq!(
            results[6].status,
            ToolStatus::Error(ToolFailure::NotExposed)
        );
        for result in &results[..6] {
            assert_eq!(result.status, ToolStatus::Success);
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            6,
            "read-only calls must stay concurrent despite the rejected mutating call"
        );
    }

    #[tokio::test]
    async fn batch_pre_cancelled_marks_all_not_started() {
        let registry = registry(vec![MockTool {
            spec: spec("hang"),
            read_only: true,
            action: Action::Hang,
        }]);
        let exposure = ToolExposure::from_names(["hang"]);
        let token = CancellationToken::new();
        token.cancel();
        let ctx = test_ctx(Duration::from_secs(30), &token);
        let calls: Vec<ToolCall> = vec![make_call("hang"); 3];
        let batch = ToolBatch::new(&registry, &exposure);
        let results = batch.execute(&calls, &ctx).await;
        assert_eq!(results.len(), 3);
        for result in &results {
            assert_eq!(result.status, ToolStatus::Cancelled { started: false });
        }
    }

    #[tokio::test]
    async fn batch_cancellation_table() {
        // 20 calls, bound 8 (§6.5 table): call 0 completes fast; calls
        // 1..=8 are in flight when cancellation lands (buffered refills
        // call 0's slot with call 8); calls 9..=19 were never polled.
        // Call 0 signals `ready` as it completes, so the cancel point is
        // deterministic instead of racing a sleep window (which flaked
        // when the runner was slow to first-poll the batch).
        let ready = Arc::new(tokio::sync::Notify::new());
        let mut reg = ToolRegistry::default();
        reg.register(Box::new(MockTool {
            spec: spec("fast"),
            read_only: true,
            action: Action::SignalText {
                notify: ready.clone(),
                text: "done".to_owned(),
            },
        }))
        .expect("unique");
        reg.register(Box::new(MockTool {
            spec: spec("sleep"),
            read_only: true,
            action: Action::Sleep { ms: 100 },
        }))
        .expect("unique");
        let exposure = ToolExposure::from_names(["fast", "sleep"]);
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(30), &token);
        let mut calls = vec![make_call("fast")];
        for i in 1..20 {
            let mut call = make_call("sleep");
            call.id = ToolCallId::try_from(format!("call-sleep-{i}")).expect("valid id");
            calls.push(call);
        }

        let batch = ToolBatch::new(&reg, &exposure).with_max_concurrency(8);
        let mut execute = std::pin::pin!(batch.execute(&calls, &ctx));
        // On the current-thread test runtime, one poll of the batch runs
        // call 0 to completion, writes its slot, and refills the free slot
        // with call 8 — so once call 0 signals, calls 1..=8 are in flight.
        let notified = std::pin::pin!(ready.notified());
        match future::select(execute.as_mut(), notified).await {
            Either::Left(_) => panic!("batch finished before call 0 signalled"),
            Either::Right(_) => {}
        }
        token.cancel();
        let results = execute.await;

        assert_eq!(results.len(), 20);
        assert_eq!(results[0].status, ToolStatus::Success);
        assert_eq!(results[0].content, "done");
        for result in &results[1..=8] {
            assert_eq!(
                result.status,
                ToolStatus::Cancelled { started: true },
                "in-flight calls must report started"
            );
        }
        for result in &results[9..] {
            assert_eq!(
                result.status,
                ToolStatus::Cancelled { started: false },
                "never-polled calls must report not started"
            );
        }
    }

    #[tokio::test]
    async fn batch_empty_calls_return_empty() {
        let registry = registry(vec![]);
        let exposure = ToolExposure::default();
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(1), &token);
        let batch = ToolBatch::new(&registry, &exposure);
        let results = batch.execute(&[], &ctx).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn oversized_output_spills_to_an_artifact() {
        let content = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 1024);
        let tool = MockTool::text("big", &content);
        let store = Arc::new(crate::store::MemoryArtifactStore::new());
        let token = CancellationToken::new();
        let ctx = test_ctx(Duration::from_secs(10), &token).with_artifacts(Some(store.clone()));
        let registry = registry(vec![tool]);
        let exposure = ToolExposure::from_names(["big"]);

        let result = execute_call(
            &registry,
            &exposure,
            &make_call("big"),
            &ctx,
            &AtomicBool::new(false),
        )
        .await;

        assert!(result.truncated, "spilled output must be marked truncated");
        assert!(
            result.content.len() <= MAX_TOOL_OUTPUT_BYTES,
            "inline content must stay within the budget"
        );
        assert!(
            result
                .content
                .ends_with("bytes); read it with artifact_read]"),
            "content must point at the artifact: {}",
            result.content
        );
        let reference = result.artifact.expect("output should spill to an artifact");
        assert_eq!(reference.kind, ARTIFACT_KIND);
        assert_eq!(reference.bytes, content.len() as u64);
        let fetched = store.get(&reference.id).expect("get").expect("stored");
        assert_eq!(fetched.body, content.into_bytes());
    }
}
