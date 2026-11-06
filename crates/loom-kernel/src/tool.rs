//! Tool definitions and the tool-execution seam.
//!
//! This module holds the **definitions** a tool contributes and the seam
//! the engine uses to invoke it — no execution policy. Scheduling,
//! concurrency, ordering, timeouts-as-status, and cancellation-as-status
//! (architecture §6.5/§6.6/§10) live in the tool engine, not here and not
//! in tool implementations.
//!
//! The split of responsibility per §9 of the architecture:
//!
//! - [`ToolSpec`] — the model-facing description carried in
//!   [`ModelRequest`](crate::model::ModelRequest).
//! - [`Tool`] — the implementation seam: tools parse `input` into their
//!   own typed struct (ADR-005); a parse or semantic failure is
//!   [`ToolError::InvalidInput`]. Cancellable, workspace-bound.
//! - [`ToolContext`] — per-call inputs (workspace root, cancellation
//!   token, timeout budget).
//! - [`ToolOutput`]/[`ToolError`] — what a call returns; the engine maps
//!   these onto transcript `ToolResult`s.
//! - [`ToolEffects`] — what a call changed outside Loom (files, commands),
//!   recorded at execution; never declared by the model (ADR-012).
//! - [`ToolRegistry`] — name-keyed set of tools the engine dispatches
//!   against.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::artifact::ArtifactRef;
use crate::session::{TodoList, TodoStatus};
use crate::store::ArtifactStore;
use crate::tool_cache::ToolCache;

/// The model-facing definition of one tool (its name, what it does, and
/// the arguments it accepts).
///
/// This is what a provider adapter renders into its wire tool format
/// (e.g. OpenAI `function` definitions); it contains no execution
/// state and no provider details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model uses to invoke the tool. Stable per task —
    /// renaming tools mid-task invalidates the cached prompt prefix.
    pub name: String,
    /// One or two sentences telling the model when to use the tool.
    pub description: String,
    /// JSON Schema (an `object`-typed schema) describing the tool's
    /// arguments. Kept as an opaque [`Value`] so core never depends on a
    /// JSON-Schema library; tools own their schema.
    pub parameters: Value,
}

/// How a call touches a path, for dependency-aware scheduling (ADR-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathAccess {
    /// The call reads the path. Concurrent reads never conflict.
    Read,
    /// The call creates, overwrites, or deletes the path. A write
    /// conflicts with any other access to the same path.
    Write,
}

/// A workspace-relative path a call will touch and how (ADR-018).
///
/// Declared by the tool *before* execution on the raw arguments — never
/// model-declared — and reused by scheduling (V13), result caching (V14)
/// and pruning. Compared as given, so a tool must report the same string
/// the model used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTouch {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// Read or write.
    pub access: PathAccess,
}

impl PathTouch {
    /// A read of `path`.
    #[must_use]
    pub fn read(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: PathAccess::Read,
        }
    }

    /// A write to `path`.
    #[must_use]
    pub fn write(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: PathAccess::Write,
        }
    }
}

/// Whether two workspace-relative paths overlap: identical, or one is an
/// ancestor directory of the other (segment-wise). A read of `src` and a
/// write to `src/a.rs` contend; `src/a.rs` and `src/b.rs` do not. `.` (the
/// workspace root) is an ancestor of everything. Shared by scheduling
/// (V13) and result caching (V14).
pub(crate) fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
}

/// Whether `dir` is a strict ancestor directory of `path`.
pub(crate) fn is_ancestor(dir: &str, path: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        return !path.is_empty() && path != ".";
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

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
        let recent_changes = self
            .recent_changes
            .lock()
            .map(|paths| paths.len())
            .unwrap_or(0);
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
            .map(|paths| paths.clone())
            .unwrap_or_default()
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

/// One external command a tool ran, recorded at execution time.
///
/// Part of [`ToolEffects`]; kept structured — not a rendered shell
/// string — so later policy (dependency scheduling, change→failure
/// linkage, cache invalidation) can reason over it without re-parsing.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEffect {
    /// The executable name (bare, never a path).
    pub program: String,
    /// The arguments, one entry each.
    pub args: Vec<String>,
    /// Process exit code, or `None` when the platform reported none
    /// (e.g. terminated by signal).
    pub exit_code: Option<i32>,
}

impl CommandEffect {
    /// Records one executed command and its exit status.
    pub fn new(program: impl Into<String>, args: Vec<String>, exit_code: Option<i32>) -> Self {
        Self {
            program: program.into(),
            args,
            exit_code,
        }
    }
}

/// The side effects one tool call had on the outside world.
///
/// Recorded by the tool at execution time, never declared by the model
/// and never inferred after the fact (ADR-012, Invariant 12). Read-only
/// tools return [`ToolEffects::none`]. Paths are workspace-relative and
/// `/`-separated so a record is stable across platforms.
///
/// `before_images` carries the per-path pre-image the tool captured
/// before mutating (roadmap gap 1, tier 2): the file's content as it was
/// immediately before this call, content-addressed into the artifact
/// store. It is restore metadata, not a side effect of its own — like
/// `repairs`, it never makes `is_empty` false, and pre-image bodies are
/// part of the GC live set so rewind can trust them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEffects {
    /// Files created or overwritten, workspace-relative.
    pub files_written: Vec<String>,
    /// Files deleted, workspace-relative.
    pub files_deleted: Vec<String>,
    /// External commands executed.
    pub commands_run: Vec<CommandEffect>,
    /// Loom-internal repairs applied to a slightly malformed call before
    /// or during execution (e.g. a whitespace-padded `old_string` that
    /// only matched after trimming). Notes for the record, not side
    /// effects — they never invalidate caches or count as writes.
    #[serde(default)]
    pub repairs: Vec<String>,
    /// Per-path pre-images captured before the call mutated, keyed by the
    /// same path strings the tool recorded in `files_written` /
    /// `files_deleted`. A path with no entry was not captured (run
    /// predates tier 2, or no artifact store was attached) — restore
    /// refuses for that path rather than guessing.
    #[serde(default)]
    pub before_images: Vec<BeforeImage>,
    /// The verify-call autofix outcome (roadmap item 33), when this call
    /// attempted one: MachineApplicable suggestions applied after a
    /// failing verify, gated by a re-run. Like `repairs`, the record
    /// never makes `is_empty` true on its own — the file writes it kept
    /// already do. Additive (schema-era persisted effects deserialize
    /// without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autofix: Option<AutofixEffect>,
}

/// One autofix attempt's outcome record (roadmap item 33): what was
/// applied, what the never-worse gate saw, and whether the edits were
/// kept or reverted. Mirrors [`EventKind::AutofixApplied`]'s payload;
/// the agent loop emits the event from this record.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutofixEffect {
    /// Machine-applicable edits applied to disk before the gate.
    pub edits: u32,
    /// Distinct workspace files those edits touched.
    pub files: u32,
    /// Error+warning diagnostic count of the failing run.
    pub diagnostics_before: u32,
    /// Diagnostic count after the re-run (`<= before` when kept).
    pub diagnostics_after: u32,
    /// Whether the edits survived the never-worse gate.
    pub kept: bool,
}

impl AutofixEffect {
    /// Records one attempt's outcome. Mirrors [`EventKind::AutofixApplied`]'s
    /// payload; the agent loop emits the event from this record.
    #[must_use]
    pub fn new(
        edits: u32,
        files: u32,
        diagnostics_before: u32,
        diagnostics_after: u32,
        kept: bool,
    ) -> Self {
        Self {
            edits,
            files,
            diagnostics_before,
            diagnostics_after,
            kept,
        }
    }
}

/// One file's content immediately before the call that recorded it, or
/// that the file did not exist at all (roadmap gap 1, tier 2:
/// `docs/rewind-design.md` §4).
///
/// `content` is the content-addressed pre-image body, or `None` when
/// the file was absent before the call — restoring a boundary then
/// deletes the file rather than writing anything. Captured by the tool
/// at execution time from the file it is about to modify, never
/// inferred (ADR-012); reverse-replaying serialized tool arguments
/// would silently break on whitespace repairs and out-of-band command
/// effects, which is why this is storage, not reconstruction.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeforeImage {
    /// The path this pre-image describes, in the same string form the
    /// tool recorded in `files_written` / `files_deleted`.
    pub path: String,
    /// The pre-image body, or `None` when the file did not exist before
    /// the matching mutation.
    pub content: Option<ArtifactRef>,
}

impl BeforeImage {
    /// Records a path that did not exist before the call (restoring
    /// deletes it).
    pub fn absent(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: None,
        }
    }

    /// Records the pre-image body captured before the call mutated.
    pub fn captured(path: impl Into<String>, content: ArtifactRef) -> Self {
        Self {
            path: path.into(),
            content: Some(content),
        }
    }
}

impl ToolEffects {
    /// No effects — the read-only default.
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether the call changed anything outside Loom's own state.
    pub fn is_empty(&self) -> bool {
        self.files_written.is_empty()
            && self.files_deleted.is_empty()
            && self.commands_run.is_empty()
    }

    /// Records a file the call created or overwrote.
    #[must_use]
    pub fn with_file_written(mut self, path: impl Into<String>) -> Self {
        self.files_written.push(path.into());
        self
    }

    /// Records a file the call deleted.
    #[must_use]
    pub fn with_file_deleted(mut self, path: impl Into<String>) -> Self {
        self.files_deleted.push(path.into());
        self
    }

    /// Records a command the call executed.
    #[must_use]
    pub fn with_command(mut self, command: CommandEffect) -> Self {
        self.commands_run.push(command);
        self
    }

    /// Records a repair Loom applied to this call's arguments before or
    /// during execution.
    #[must_use]
    pub fn with_repair(mut self, note: impl Into<String>) -> Self {
        self.repairs.push(note.into());
        self
    }

    /// Records one pre-image captured before this call mutated a path
    /// (tier-2 rewind metadata, not a side effect itself).
    #[must_use]
    pub fn with_before_image(mut self, image: BeforeImage) -> Self {
        self.before_images.push(image);
        self
    }

    /// Records an autofix attempt's outcome (roadmap item 33). The last
    /// record wins when a call somehow attempts more than one.
    #[must_use]
    pub fn with_autofix(mut self, autofix: AutofixEffect) -> Self {
        self.autofix = Some(autofix);
        self
    }

    /// Folds `other`'s records into `self` (order-preserving appends;
    /// `other`'s autofix record wins when set). The fused-verify path
    /// composes its own effects with the host tool's through this.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.files_written.extend(other.files_written);
        self.files_deleted.extend(other.files_deleted);
        self.commands_run.extend(other.commands_run);
        self.repairs.extend(other.repairs);
        self.before_images.extend(other.before_images);
        if other.autofix.is_some() {
            self.autofix = other.autofix;
        }
        self
    }
}

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
    pub fn effects(&self) -> &ToolEffects {
        &self.effects
    }
}

/// Why a [`Tool::call`] failed.
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

/// Failure mode of [`ToolRegistry::register`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ToolRegistryError {
    /// A tool with this name is already registered; registration is
    /// refused so `get` stays unambiguous.
    #[error("tool already registered: {name}")]
    DuplicateName {
        /// The duplicated tool name.
        name: String,
    },
}

/// The name-keyed set of tools a task may dispatch against.
///
/// Definitions only: it does not execute anything. The engine resolves
/// model-issued [`ToolCall`](crate::transcript::ToolCall)s through
/// [`ToolRegistry::get`] and offers [`ToolRegistry::specs`] to the
/// context compiler for building
/// [`ModelRequest::tools`](crate::model::ModelRequest::tools).
///
/// Storage preserves registration order, so `specs()` yields a stable
/// tool list — renaming or reordering tools mid-task would invalidate
/// the provider's cached prompt prefix.
///
/// Tools are held behind `Arc` so a registry can be cloned cheaply
/// ([`ToolRegistry::shallow_clone`]). The agent loop uses that to layer
/// a session's dynamic tool surface ([`crate::agent::ToolSurface`]) on
/// top of the base registry without disturbing it.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Tool` isn't `Debug`; tool names are the useful summary.
        let names: Vec<_> = self.tools.iter().map(|t| t.spec().name.as_str()).collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Adds a tool, refusing duplicates by name.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolRegistryError> {
        self.register_arc(tool.into())
    }

    /// Adds an already-shared tool, refusing duplicates by name.
    ///
    /// The dynamic tool surface hands the same `Arc` it tracks to the
    /// per-turn registry view, so dispatch and the surface stay in
    /// lockstep without re-boxing.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let name = tool.spec().name.clone();
        if self.get(&name).is_some() {
            return Err(ToolRegistryError::DuplicateName { name });
        }
        self.tools.push(tool);
        Ok(())
    }

    /// Looks a tool up by the name the model used.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .map(|tool| tool.as_ref())
    }

    /// All specs, in registration order — the stable ordering the model
    /// sees across requests.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.iter().map(|tool| tool.spec())
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Consumes the registry, yielding the registered tools (as shared
    /// handles) in order. For embedders that filter a built registry
    /// (the bare-loop arm keeps the primitive tools only) rather than
    /// rebuilding one.
    pub fn into_arc_tools(self) -> impl Iterator<Item = Arc<dyn Tool>> {
        self.tools.into_iter()
    }

    /// Builds a registry from tools that are already unique by name.
    /// Order is preserved (the stable ordering the model sees).
    pub fn from_arc_tools<I>(tools: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn Tool>>,
    {
        let mut registry = Self::new();
        for tool in tools {
            let result = registry.register_arc(tool);
            debug_assert!(
                result.is_ok(),
                "from_arc_tools received a duplicate tool name: {result:?}"
            );
        }
        registry
    }
}

/// The virtual workspace-relative cell a mutating `todo` op writes.
///
/// The list lives in kernel session state, not the filesystem. Declaring
/// one shared write path lets the scheduler serialize concurrent `todo`
/// ops in model order — so assigned ids stay deterministic — while file
/// tools never contend with them (the path is not a real file any tool
/// reads or writes).
const TODO_STATE_PATH: &str = ".loom/todo";

/// The kernel-owned `todo` tool (roadmap item 8 phase B): create, add,
/// mark, and read the session's plan list.
///
/// This is the one concrete tool the kernel itself implements: its state
/// is per-session ([`crate::session::TodoList`]), so it must mutate
/// through the [`ToolContext::todos`] handle the agent loop seeds from
/// the session — a seam only the kernel can own. Every other tool stays
/// in `loom-tools` (ADR-001).
///
/// Results are deliberately terse (`added 3: write tests`); the loop,
/// not the tool, re-appends the full rendered list at the transcript
/// tail when a batch changed it, so the model sees the current plan in
/// every later projection without the tool result duplicating it. The
/// `read` op is the exception — it returns the rendered block itself.
///
/// The input schema is hand-written rather than generated (ADR-005
/// addendum 1 uses `schemars`, a dependency the kernel deliberately
/// does not carry); the parser below is the source of truth and the two
/// are pinned by unit test.
#[derive(Debug, Default)]
pub struct TodoTool;

impl TodoTool {
    /// Creates the tool. Stateless: the list arrives per call through
    /// [`ToolContext::todos`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// The `todo` tool's input, parsed with serde (`op` selects the rest).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoInput {
    /// Operation: `"create"`, `"add"`, `"mark"`, or `"read"`.
    op: String,
    /// Item text for `"add"`.
    #[serde(default)]
    text: Option<String>,
    /// Seed texts for `"create"` (a fresh list); defaults to empty.
    #[serde(default)]
    items: Option<Vec<String>>,
    /// Item id for `"mark"`.
    #[serde(default)]
    id: Option<String>,
    /// Target status for `"mark"`: `pending`, `in_progress`, `done`, or
    /// `cancelled`.
    #[serde(default)]
    status: Option<String>,
}

impl Tool for TodoTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "todo".to_owned(),
            description: "Manages the session's kernel-owned todo list: the plan \
                          you are executing. Create the list when a task has \
                          multiple steps, add steps as they are discovered, mark \
                          them in_progress when started and done/cancelled when \
                          settled, and read to see the current list. The current \
                          list is re-shown at the end of the context whenever it \
                          changes; keep it current."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["create", "add", "mark", "read"],
                        "description": "Operation: create a fresh list (optional \
                                        `items` seed texts), add a step (`text`), \
                                        set a step's status (`id` + `status`), or \
                                        read the current list."
                    },
                    "text": {
                        "type": "string",
                        "description": "The step text, for op=add."
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Seed step texts, for op=create. Optional."
                    },
                    "id": {
                        "type": "string",
                        "description": "The step id to update, for op=mark."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done", "cancelled"],
                        "description": "The status to set, for op=mark."
                    }
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &Value) -> Option<Vec<PathTouch>> {
        // A mutating op writes one virtual cell, serializing concurrent
        // todo ops in model order; `read` (and anything that will fail
        // validation) touches nothing.
        let op = input.get("op").and_then(Value::as_str);
        match op {
            Some("create" | "add" | "mark") => Some(vec![PathTouch::write(TODO_STATE_PATH)]),
            _ => Some(Vec::new()),
        }
    }

    fn call<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: TodoInput = serde_json::from_value(input)
                .map_err(|error| ToolError::InvalidInput(format!("invalid arguments: {error}")))?;
            let mut todos = ctx
                .todos
                .as_ref()
                .ok_or_else(|| {
                    ToolError::Execution("todo state is not attached to this call".to_owned())
                })?
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match input.op.as_str() {
                "create" => {
                    let items = input.items.unwrap_or_default();
                    let count = items.len();
                    todos.replace(items);
                    Ok(ToolOutput::new(format!(
                        "created list with {count} item{}",
                        if count == 1 { "" } else { "s" }
                    )))
                }
                "add" => {
                    let text = input.text.filter(|text| !text.trim().is_empty());
                    let Some(text) = text else {
                        return Err(ToolError::InvalidInput(
                            "op=add requires a non-empty `text`".to_owned(),
                        ));
                    };
                    let id = todos.add(text);
                    Ok(ToolOutput::new(format!("added {id}")))
                }
                "mark" => {
                    let Some(id) = input.id else {
                        return Err(ToolError::InvalidInput(
                            "op=mark requires an `id`".to_owned(),
                        ));
                    };
                    let Some(wire) = input.status else {
                        return Err(ToolError::InvalidInput(
                            "op=mark requires a `status` of pending, in_progress, done, \
                             or cancelled"
                                .to_owned(),
                        ));
                    };
                    let Some(status) = TodoStatus::parse(&wire) else {
                        return Err(ToolError::InvalidInput(format!(
                            "unknown status {wire:?}; use pending, in_progress, done, \
                             or cancelled"
                        )));
                    };
                    if !todos.mark(&id, status) {
                        return Err(ToolError::InvalidInput(format!(
                            "unknown todo id {id:?}; use `read` to see the list"
                        )));
                    }
                    Ok(ToolOutput::new(format!("marked {id} as {wire}")))
                }
                "read" => Ok(ToolOutput::new(if todos.is_empty() {
                    "(todo list is empty)".to_owned()
                } else {
                    todos.render()
                })),
                other => Err(ToolError::InvalidInput(format!(
                    "unknown op {other:?}; use create, add, mark, or read"
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactRef;
    use crate::id::ArtifactId;
    use serde_json::json;

    /// Minimal read-only tool that echoes its input back as JSON —
    /// exercises the whole seam without filesystem effects.
    struct EchoTool {
        spec: ToolSpec,
    }

    impl EchoTool {
        fn new() -> Self {
            Self {
                spec: ToolSpec {
                    name: "echo".into(),
                    description: "Echoes its input.".into(),
                    parameters: json!({"type": "object"}),
                },
            }
        }
    }

    #[test]
    fn before_images_round_trip_and_default_for_old_transcripts() {
        // Legacy effects JSON (recorded before tier-2 capture) has no
        // before_images key; serde's default keeps those transcripts
        // loadable rather than versioning the event schema.
        let legacy = json!({
            "files_written": ["a.rs"],
            "files_deleted": [],
            "commands_run": [],
            "repairs": [],
        });
        let effects: ToolEffects = serde_json::from_value(legacy).expect("legacy effects parse");
        assert!(effects.before_images.is_empty());

        // A captured image and an absent-file image both survive a
        // round trip verbatim — the absent one is a positive claim
        // ("did not exist"), not a missing entry.
        let reference = ArtifactRef::new(ArtifactId::new(), "pre_image", 12, "deadbeefdeadbeef");
        let effects = ToolEffects::none()
            .with_file_written("a.rs")
            .with_before_image(BeforeImage::captured("a.rs", reference.clone()))
            .with_before_image(BeforeImage::absent("b.txt"));
        let round: ToolEffects =
            serde_json::from_value(serde_json::to_value(&effects).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round.before_images, effects.before_images);
        assert_eq!(
            round.before_images[1],
            BeforeImage {
                path: "b.txt".to_owned(),
                content: None,
            }
        );

        // Pre-images are restore metadata, not side effects: they never
        // make is_empty false (mirrors `repairs`).
        assert!(
            ToolEffects::none()
                .with_before_image(BeforeImage::absent("b.txt"))
                .is_empty()
        );
    }

    impl Tool for EchoTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn is_read_only(&self) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            input: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                let content = serde_json::to_string(&input)
                    .map_err(|e| ToolError::Execution(e.to_string()))?;
                Ok(ToolOutput::new(content))
            })
        }
    }

    #[test]
    fn tool_spec_round_trips() {
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "Read a file's contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn tool_trait_is_object_safe_and_callable() {
        // The engine holds tools as `Box<dyn Tool>`; calling through the
        // trait object must work.
        let tool: Box<dyn Tool> = Box::new(EchoTool::new());
        assert!(tool.is_read_only());
        assert_eq!(tool.spec().name, "echo");

        let ctx = ToolContext::new(
            PathBuf::from("/tmp/workspace"),
            CancellationToken::new(),
            Duration::from_secs(30),
        );
        let output = futures::executor::block_on(tool.call(json!({"a": 1}), &ctx)).unwrap();
        assert_eq!(output.content, r#"{"a":1}"#);
        assert!(!output.truncated);
    }

    #[test]
    fn tool_context_wires_cancellation() {
        let cancellation = CancellationToken::new();
        let ctx = ToolContext::new(
            PathBuf::from("/w"),
            cancellation.clone(),
            Duration::from_secs(5),
        );
        assert!(!ctx.cancellation.is_cancelled());
        cancellation.cancel();
        assert!(ctx.cancellation.is_cancelled());
        assert_eq!(ctx.timeout, Duration::from_secs(5));
    }

    #[test]
    fn tool_output_tracks_truncation() {
        let full = ToolOutput::new("body");
        assert_eq!(full.content, "body");
        assert!(!full.truncated);

        let cut = ToolOutput::new("body…").with_truncated();
        assert!(cut.truncated);
    }

    #[test]
    fn tool_errors_distinguish_input_from_execution() {
        let bad = ToolError::InvalidInput("path must be relative".into());
        let boom = ToolError::Execution("permission denied".into());
        assert_eq!(bad.to_string(), "invalid input: path must be relative");
        assert_eq!(boom.to_string(), "execution failed: permission denied");
        assert_ne!(bad, boom);
    }

    #[test]
    fn registry_registers_looks_up_and_orders_specs() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());

        registry
            .register(Box::new(EchoTool::new()))
            .expect("first registration succeeds");
        registry
            .register(Box::new(SecondTool::new()))
            .expect("second registration succeeds");

        assert_eq!(registry.len(), 2);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("nope").is_none());

        // Registration order is the spec order the model sees.
        let names: Vec<_> = registry.specs().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["echo", "second"]);
    }

    #[test]
    fn registry_rejects_duplicate_names() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool::new())).unwrap();
        let err = registry.register(Box::new(EchoTool::new())).unwrap_err();
        assert_eq!(
            err,
            ToolRegistryError::DuplicateName {
                name: "echo".into()
            }
        );
        // The first registration is untouched.
        assert_eq!(registry.len(), 1);
    }

    /// A context with a todo list attached, plus the handle to read it
    /// back after the call — the shape the agent loop uses.
    fn todo_ctx() -> (ToolContext, Arc<Mutex<crate::session::TodoList>>) {
        let todos = Arc::new(Mutex::new(crate::session::TodoList::default()));
        let ctx = ToolContext::new(
            PathBuf::from("/tmp/workspace"),
            CancellationToken::new(),
            Duration::from_secs(30),
        )
        .with_todos(Some(todos.clone()));
        (ctx, todos)
    }

    fn call_todo(input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let tool: Box<dyn Tool> = Box::new(TodoTool::new());
        futures::executor::block_on(tool.call(input, ctx))
    }

    #[test]
    fn todo_add_assigns_stable_ids_and_confirms_terse() {
        let (ctx, todos) = todo_ctx();
        let first =
            call_todo(json!({"op": "add", "text": "write the test"}), &ctx).expect("add succeeds");
        assert_eq!(first.content, "added 1");
        let second =
            call_todo(json!({"op": "add", "text": "fix the bug"}), &ctx).expect("add succeeds");
        assert_eq!(second.content, "added 2");
        let list = todos.lock().unwrap();
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].id, "1");
        assert_eq!(list.items[1].text, "fix the bug");
    }

    #[test]
    fn todo_create_replaces_and_read_renders_the_list() {
        let (ctx, todos) = todo_ctx();
        call_todo(json!({"op": "add", "text": "stale step"}), &ctx).unwrap();
        let created = call_todo(json!({"op": "create", "items": ["one", "two"]}), &ctx)
            .expect("create succeeds");
        assert_eq!(created.content, "created list with 2 items");

        let read = call_todo(json!({"op": "read"}), &ctx).expect("read succeeds");
        assert_eq!(
            read.content, "## todo (kernel)\n- [ ] 2: one\n- [ ] 3: two",
            "read returns the rendered block; ids never restart"
        );
        assert!(todos.lock().unwrap().items[0].id != "1");
    }

    #[test]
    fn todo_mark_updates_status_and_rejects_unknown_ids() {
        let (ctx, todos) = todo_ctx();
        call_todo(json!({"op": "add", "text": "step"}), &ctx).unwrap();
        for status in ["in_progress", "done", "cancelled", "pending"] {
            let marked = call_todo(json!({"op": "mark", "id": "1", "status": status}), &ctx)
                .expect("mark succeeds");
            assert_eq!(marked.content, format!("marked 1 as {status}"));
        }
        assert_eq!(todos.lock().unwrap().items[0].status, TodoStatus::Pending);

        let err = call_todo(json!({"op": "mark", "id": "9", "status": "done"}), &ctx)
            .expect_err("unknown id");
        assert!(err.to_string().contains("unknown todo id"), "{err:?}");
        // A rejected mark changed nothing.
        assert_eq!(todos.lock().unwrap().revision, 5);
    }

    #[test]
    fn todo_rejects_a_bad_op_and_missing_fields() {
        let (ctx, _todos) = todo_ctx();
        for bad in [
            json!({"op": "delete"}),
            json!({"op": "add"}),
            json!({"op": "add", "text": "   "}),
            json!({"op": "mark", "id": "1"}),
            json!({"op": "mark", "id": "1", "status": "finished"}),
            json!({"text": "no op"}),
            json!({"op": "read", "unexpected": true}),
        ] {
            let err = call_todo(bad, &ctx).expect_err("input must be rejected");
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err:?}");
        }
    }

    #[test]
    fn todo_without_attached_state_fails_execution() {
        let ctx = ToolContext::new(
            PathBuf::from("/tmp/workspace"),
            CancellationToken::new(),
            Duration::from_secs(30),
        );
        let err = call_todo(json!({"op": "read"}), &ctx).expect_err("no todo state attached");
        assert!(matches!(err, ToolError::Execution(_)), "{err:?}");
    }

    #[test]
    fn todo_declares_one_virtual_write_cell_for_mutating_ops() {
        let tool = TodoTool::new();
        assert!(!tool.is_read_only());
        // Mutating ops serialize against each other via one shared path.
        for op in ["create", "add", "mark"] {
            let touched = tool.touched_paths(&json!({"op": op})).expect("declared");
            assert_eq!(touched.len(), 1);
            assert_eq!(touched[0].access, PathAccess::Write);
        }
        // Read (and unparseable input) touches nothing.
        assert_eq!(tool.touched_paths(&json!({"op": "read"})), Some(Vec::new()));
        assert_eq!(tool.touched_paths(&json!({})), Some(Vec::new()));
    }

    #[test]
    fn todo_spec_schema_matches_the_parser() {
        // The schema is hand-written (the kernel carries no schemars);
        // pin the shape the parser enforces so the two cannot drift.
        let tool = TodoTool::new();
        let parameters = tool.spec().parameters.clone();
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(parameters["required"], json!(["op"]));
        assert_eq!(
            parameters["properties"]["op"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );
        assert_eq!(
            parameters["properties"]["status"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );
        // Every advertised op parses; every op's fields exist in the schema.
        for field in ["op", "text", "items", "id", "status"] {
            assert!(parameters["properties"][field].is_object(), "{field}");
        }
    }

    /// Second distinct tool so ordering tests are meaningful.
    struct SecondTool {
        spec: ToolSpec,
    }

    impl SecondTool {
        fn new() -> Self {
            Self {
                spec: ToolSpec {
                    name: "second".into(),
                    description: "Another tool.".into(),
                    parameters: json!({"type": "object"}),
                },
            }
        }
    }

    impl Tool for SecondTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn is_read_only(&self) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async { Ok(ToolOutput::new("ok")) })
        }
    }
}
