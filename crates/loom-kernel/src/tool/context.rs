//! The execution seam: the [`Tool`] trait implementations implement and
//! the per-call [`ToolContext`] the engine hands them.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::effects::ToolEffects;
use super::output::{ToolError, ToolOutput};
use super::paths::PathTouch;
use super::spec::ToolSpec;
use crate::session::TodoList;
use crate::store::ArtifactStore;
use crate::tool_cache::ToolCache;

/// The execution seam for one tool.
///
/// Implementations live in `loom-tools`; the engine (and only the engine)
/// calls [`Tool::call`]. Input validation is tool-side (ADR-005): each
/// tool parses `input` into its own typed struct, and a parse or
/// semantic failure is reported as [`ToolError::InvalidInput`] — never a
/// panic.
pub trait Tool: Send + Sync {
    /// The model-facing definition this implementation backs.
    fn spec(&self) -> &ToolSpec;

    /// Whether the tool only reads state (never mutates files, processes,
    /// or the outside world).
    ///
    /// Input to the engine's scheduler: read-only calls
    /// may run concurrently, mutating calls run sequentially in call
    /// order (§10). A tool lying here breaks the execution contract.
    fn is_read_only(&self) -> bool;

    /// The workspace paths this call will touch, for dependency-aware
    /// scheduling (ADR-018).
    ///
    /// * `None` — the tool cannot describe its paths; the engine treats it
    ///   as touching anything, so such a call serializes against the rest
    ///   of the batch. This is the conservative default.
    /// * `Some(vec![])` — the call provably touches no workspace path
    ///   (e.g. `artifact_read`), so it never conflicts.
    /// * `Some(touches)` — exactly these paths.
    ///
    /// The engine calls this before execution, on the raw arguments; it
    /// must be cheap and must not block. Arguments that fail to parse are
    /// reported as touching nothing, since the call will then fail without
    /// mutating.
    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        let _ = input;
        None
    }

    /// Whether this call needs the operator's approval before it runs.
    ///
    /// The engine emits `ApprovalRequested` before such a call and
    /// `ApprovalDecided` after it, on the run's own sink. Defaults to
    /// `false`: a tool gates only when it has an approval hook attached.
    fn requires_approval(&self, input: &Value) -> bool {
        let _ = input;
        false
    }

    /// An optional, tool-specific remediation hint for a failed call.
    ///
    /// The loop always appends a generic next-step hint to a failure the
    /// model will see. A tool that knows the shape of its own governing
    /// mistakes — the argument fields it expects, the anchor it needs —
    /// may replace that generic text with something more useful; returning
    /// `None` keeps the generic text. A refusal (`RepeatedCall`) is never
    /// overridable: the tool was not invoked, so it has nothing to say.
    fn failure_hint(&self, failure: crate::transcript::ToolFailure) -> Option<String> {
        let _ = failure;
        None
    }

    /// An optional execution budget for one call of this tool, overriding
    /// the engine's default.
    ///
    /// A tool that shells out to a build or a test suite needs minutes
    /// where a read needs a second: the default budget killed
    /// `run_command` mid-`cargo test` seven times in the pre-fix sweep,
    /// each one a turn spent on nothing. Returning `None` keeps the
    /// engine default, which stays the backstop for every other tool.
    fn timeout(&self) -> Option<Duration> {
        None
    }

    /// Executes the tool.
    ///
    /// The returned future is *not* required to observe
    /// [`ToolContext::cancellation`] — dropping it is the engine's
    /// cancellation mechanism (§6.6) — but long-running tools should poll
    /// the token and exit early so blocking work stops promptly.
    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

/// Per-call inputs handed to every [`Tool::call`].
///
/// Fields may grow (hence `#[non_exhaustive]`); construct with [`ToolContext::new`].
/// The engine builds **one context per call**: [`Clone`] is cheap and the
/// cancellation token clone-shares the turn's token, so per-call timeout
/// budgets stay expressible while cancellation stays turn-wide.
#[non_exhaustive]
#[derive(Clone)]
pub struct ToolContext {
    /// The workspace root all relative paths resolve against. Filesystem
    /// tools must enforce this boundary themselves: relative paths,
    /// absolute paths, `..` traversal, symlinks, and canonicalization
    /// failures all have to be handled (§9.1) — an escape is an execution
    /// invariant violation, not a policy question.
    pub workspace_root: PathBuf,
    /// Cooperative cancellation for long-running tools (§6.5). Clones
    /// share state, so every call's token cancels with the turn. The
    /// engine also simply drops the call future; this token lets
    /// well-behaved tools stop blocking work earlier.
    pub cancellation: CancellationToken,
    /// The execution budget for this call. Advisory input to the tool;
    /// the *authoritative* timeout that produces a
    /// `ToolStatus::Error(Timeout)` result is enforced by the engine
    /// (§6.6).
    pub timeout: Duration,
    /// The store a tool may spill an oversized result into (ADR-012
    /// decision 3). `None` when the run has no artifacts configured, in
    /// which case results are only bounded inline.
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
    /// Recently changed workspace-relative paths, oldest first (2F,
    /// ADR-017). Derived from the turn's recorded [`ToolEffects`], not
    /// from a model declaration, so verification can link a failure to a
    /// recent change without a symbol graph. Empty for read-only runs.
    ///
    /// Shared rather than snapshotted so effects recorded by calls earlier
    /// in the *same* batch are visible to a later `verify` call in that
    /// batch (same-batch change→failure linkage); the loop seeds it from
    /// committed results and the tool engine appends as calls finish.
    pub recent_changes: Arc<Mutex<Vec<String>>>,
    /// Shared result cache for read-only calls (2G, ADR-018, V14). `None`
    /// disables caching. A hit is verified by content hash before reuse,
    /// and the engine invalidates entries when any recorded effect touches
    /// their paths — so a stale result is never served.
    pub cache: Option<Arc<Mutex<ToolCache>>>,
    /// Call ids whose results are still visible in the request the model
    /// sees this turn (post-projection). A cache hit whose original call
    /// is in this set is served as a cheap `unchanged since call <id>`
    /// stub — the full result is one scroll up; re-serving bounded
    /// duplicates of in-view output spends context twice. Hits whose
    /// original has been compacted away fall back to the full content.
    pub projected_calls: HashSet<String>,
    /// The session's kernel-owned todo list (roadmap item 8 phase B),
    /// shared between the loop and the `todo` tool for one tool batch:
    /// the loop seeds it from the session, the tool mutates it, and the
    /// loop copies the value back before the turn ends. `None` on
    /// contexts built without todo state — the `todo` tool then fails
    /// with [`ToolError::Execution`] rather than silently dropping edits.
    pub todos: Option<Arc<Mutex<TodoList>>>,
}

/// Cap on [`ToolContext::recent_changes`]: the most recent mutations are
/// what verification links to.
pub const MAX_RECENT_CHANGES: usize = 32;

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn ArtifactStore` is not `Debug`; report only whether a store
        // is attached, which is all a caller needs from a log line. The
        // recent-change list is behind a lock, so read its length first.
        let recent_changes = self.recent_changes.lock().map_or(0, |paths| paths.len());
        formatter
            .debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .field("cancellation", &self.cancellation)
            .field("timeout", &self.timeout)
            .field("artifacts", &self.artifacts.is_some())
            .field("recent_changes", &recent_changes)
            .field("cache", &self.cache.is_some())
            .field("projected_calls", &self.projected_calls.len())
            .field("todos", &self.todos.is_some())
            .finish()
    }
}

impl ToolContext {
    /// Assembles a context from its three per-call inputs.
    ///
    /// Explicit rather than defaulted: the engine always knows all three,
    /// and a silent default timeout or token would hide wiring mistakes.
    /// No artifact store is attached; add one with [`ToolContext::with_artifacts`].
    #[must_use]
    pub fn new(
        workspace_root: PathBuf,
        cancellation: CancellationToken,
        timeout: Duration,
    ) -> Self {
        Self {
            workspace_root,
            cancellation,
            timeout,
            artifacts: None,
            recent_changes: Arc::new(Mutex::new(Vec::new())),
            cache: None,
            projected_calls: HashSet::new(),
            todos: None,
        }
    }

    /// Attaches the artifact store oversized results may spill into
    /// (builder style).
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Replaces this call's execution budget (builder style).
    ///
    /// The engine applies the tool's own declared budget through this
    /// before the call is timed, so the tool and the enforcement agree on
    /// one number.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Supplies the recent-change list verification links failures to
    /// (builder style, 2F). Oldest first; the derive is workspace-relative.
    #[must_use]
    pub fn with_recent_changes(mut self, recent_changes: Vec<String>) -> Self {
        self.recent_changes = Arc::new(Mutex::new(recent_changes));
        self
    }

    /// Attaches the shared result cache read-only calls may be served from
    /// (builder style, 2G/ADR-018). `None` disables caching.
    #[must_use]
    pub fn with_cache(mut self, cache: Option<Arc<Mutex<ToolCache>>>) -> Self {
        self.cache = cache;
        self
    }

    /// Supplies the call ids still visible in the projected request, used
    /// to decide whether a cache hit is served as an `unchanged since
    /// call <id>` stub (builder style, roadmap item 6).
    #[must_use]
    pub fn with_projected_calls(mut self, projected_calls: HashSet<String>) -> Self {
        self.projected_calls = projected_calls;
        self
    }

    /// Attaches the session's todo list for this batch (builder style,
    /// roadmap item 8 phase B); the `todo` tool reads and mutates
    /// through this handle and the loop copies the value back.
    #[must_use]
    pub fn with_todos(mut self, todos: Option<Arc<Mutex<TodoList>>>) -> Self {
        self.todos = todos;
        self
    }

    /// A snapshot of the recently changed paths, oldest first — what
    /// verification links failures to.
    #[must_use]
    pub fn recent_changes(&self) -> Vec<String> {
        self.recent_changes
            .lock()
            .map_or_default(|paths| paths.clone())
    }

    /// Records a completed call's file effects so calls later in the same
    /// batch see them (same-batch change→failure linkage, 2F). Deduped and
    /// capped at [`MAX_RECENT_CHANGES`]; the tool engine calls this as each
    /// call finishes, before the next call in a sequential batch starts.
    pub fn note_effects(&self, effects: &ToolEffects) {
        let Ok(mut paths) = self.recent_changes.lock() else {
            return;
        };
        for path in effects
            .files_written
            .iter()
            .chain(effects.files_deleted.iter())
        {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        if paths.len() > MAX_RECENT_CHANGES {
            let excess = paths.len() - MAX_RECENT_CHANGES;
            paths.drain(..excess);
        }
    }
}
