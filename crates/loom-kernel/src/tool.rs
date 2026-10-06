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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::artifact::ArtifactRef;
use crate::store::ArtifactStore;

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
        }
    }

    /// Attaches the artifact store oversized results may spill into
    /// (builder style).
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Supplies the recent-change list verification links failures to
    /// (builder style, 2F). Oldest first; the derive is workspace-relative.
    #[must_use]
    pub fn with_recent_changes(mut self, recent_changes: Vec<String>) -> Self {
        self.recent_changes = Arc::new(Mutex::new(recent_changes));
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
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEffects {
    /// Files created or overwritten, workspace-relative.
    pub files_written: Vec<String>,
    /// Files deleted, workspace-relative.
    pub files_deleted: Vec<String>,
    /// External commands executed.
    pub commands_run: Vec<CommandEffect>,
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
    /// `content` says how to refetch the body with `artifact_read`. `None`
    /// for an ordinary inline result.
    pub artifact: Option<ArtifactRef>,
}

impl ToolOutput {
    /// Full (untruncated) output with no recorded effects.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            effects: ToolEffects::none(),
            artifact: None,
        }
    }

    /// Marks the output as truncated (builder style).
    #[must_use]
    pub fn with_truncated(mut self) -> Self {
        self.truncated = true;
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
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
