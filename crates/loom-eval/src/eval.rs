//! Evaluation harness (plan-iteration-2.md §1, step 2A).
//!
//! Precedes all optimization work. Four pieces:
//!
//! 1. **Task directories** under `tests/eval-tasks/<ID>/`: a `task.json`
//!    (prompt, tools, workspace kind, gate, `heavy`), an optional
//!    `system-prompt.txt`, and an optional `workspace/` tree copied
//!    verbatim into a fresh run root per run. B1 inspects the Loom repo
//!    with the read-only tools; B2–B4 mutate a small copied crate; B5 is
//!    a read-then-edit long-context task over a generated corpus (each
//!    read is superseded by the following edit, so pruning is safe); B6
//!    and B7 seed real shipped bugs. Heavy tasks (B5, B7) run only when
//!    named in `LOOM_EVAL_TASKS`. Prompts are frozen.
//! 2. **N-run protocol.** B1 showed 1.85× input-token variance on a pinned
//!    task (thinking mode ignores temperature), so live evaluation always
//!    runs N ≥ 5 times and reports mean/median/min/max — never a single
//!    run. Per-run records follow the 1H schema.
//! 3. **Record/replay provider.** Live runs record every
//!    `(ModelRequest, ModelResponse)` exchange as JSONL. Replays re-run the
//!    agent offline (canned responses, real tools) and require the
//!    transcript's assistant items to equal the recordings verbatim — the
//!    V3 / Invariant-10 replayability claim, tested without a network.
//! 4. **Outcome grading.** Field tasks are gated on the workspace state
//!    they leave behind — outcome checks (exact bytes, substrings, a
//!    green suite) for B2–B4, and for the real-bug tasks (B6–B7) the
//!    SWE-bench shape: FAIL_TO_PASS tests that must flip green,
//!    PASS_TO_PASS tests that must stay green, and protected test files
//!    compared byte-for-byte. The model claiming success is never
//!    sufficient. Lab tasks (B1/B5) stay un-gated on purpose — they are
//!    regression instruments with pinned prompts; field tasks run with
//!    the engine-default prompt users actually get. Every live run
//!    produces a suite report with a gated pass-rate headline
//!    (`report-suite-<model>.json`).
//!
//! Also: a **context census** per turn, reported as **exact serialized bytes
//! by source** (system / user / assistant / tool_result / tool specs) plus the
//! bytes÷3 token estimate (`est_tokens`). Bytes are the provider-agnostic,
//! deterministic measurement; the ÷3 figure is the budget heuristic the I1
//! compiler enforces against (architecture §8). Per-run `input_tokens` /
//! `output_tokens` / `cost_usd` are what the provider *billed* — inherently
//! per-model, so the report labels `model`, `provider`, and `price_window`.
//! The compiler also reports its own authoritative per-source census (ADR-016,
//! carried on `TaskSummary`); the harness records both the request-side
//! attribution and the compiler's.
//!
//! Running:
//!
//! ```text
//! # live (spends tokens; resolves credentials like the CLI —
//! # keychain or DEEPSEEK_API_KEY; free models cost $0):
//! cargo test -p loom-eval eval::eval_live -- --ignored --nocapture
//! # live on OpenRouter (free-tier models cost nothing but are rate-limited):
//! LOOM_EVAL_PROVIDER=openrouter \
//!     LOOM_EVAL_MODEL=deepseek/deepseek-chat-v3-0324:free \
//!     cargo test -p loom-eval eval::eval_live -- --ignored --nocapture
//! # offline replay of whatever recordings exist (always safe):
//! cargo test -p loom-eval -- --nocapture
//! ```
//!
//! Knobs: `LOOM_EVAL_PROVIDER` (`deepseek` default, or `openrouter`),
//! `LOOM_EVAL_MODEL` (default `deepseek-flash`; OpenRouter ids like
//! `vendor/model:free` are accepted — files are named with a slug),
//! `LOOM_EVAL_RUNS`
//! (default 5), `LOOM_EVAL_DIR` (default `<workspace>/target/eval`),
//! `LOOM_EVAL_TASKS` (comma-separated task ids; default all non-heavy;
//! the literal `hard-gate` expands to every gated task, heavies
//! included).
//! Live output lands in `LOOM_EVAL_DIR`: `records/<task>-<model>-<run>.jsonl`
//! and `report-<task>-<model>.json`. When recording a baseline, copy the
//! report into `docs/benchmarks/`.
//!
//! A run lost to infrastructure (the wire, the per-run watchdog, a teardown
//! before it could stamp a reason) never measured the agent, so it is
//! excluded from `pass_rate` — and then re-measured by the retry pass
//! (`LOOM_EVAL_INFRA_RETRIES`, default 1) rather than left as a hole in the
//! sample. Agent negatives are never re-run.
//!
//! **Never point this at the GLM Coding Plan** (docs/roadmap.md, "Current state").

// A test harness: panicking on a setup failure is the intended behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use loom_core::engine::auth::AuthError;
use loom_core::engine::{
    Engine, EngineConfig, EngineError, EngineOverrides, ProviderHook, estimator_for,
};
use loom_core::kernel::agent::{DEFAULT_MAX_TURNS, TaskControl};
use loom_core::kernel::context::{
    ContextCompiler, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
};
use loom_core::kernel::event::{EventKind, EventSink, StrictSink};
use loom_core::kernel::id::{SessionId, TaskId};
use loom_core::kernel::model::{
    Message, ModelCapabilities, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
    ModelStream, StopReason, Usage,
};
use loom_core::kernel::session::Session;
use loom_core::kernel::tool::ToolRegistry;
use loom_core::kernel::transcript::{
    AssistantMessage, Item, ReasoningTrace, ToolArguments, ToolCall, ToolCallId,
};
use loom_core::providers::ReasoningEffort;
use loom_core::tools::{
    EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool, VerifyTool, WriteFileTool,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- tasks ---

/// The tools a task may call. Deliberately coarse: the point is to pin
/// which capabilities a baseline exercised, not to configure the agent.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ToolSet {
    /// `read_file`, `list_dir`, `search` (B1's 2026-10-06 baseline).
    ReadOnly,
    /// Read-only set plus `write_file`, `edit_file`, `run_command`,
    /// `verify` (B2–B4, B6–B7).
    Coding,
    /// `read_file` + `list_dir` + `edit_file` (B5). The task reads each file
    /// and then edits it, so every read becomes stale (superseded by the
    /// write) and pruning can drop it without the model needing to recover
    /// the content — the pruning-appropriate long-context shape.
    ReadEdit,
}

/// Which workspace a task runs against.
enum TaskWorkspace {
    /// The Loom repository itself (read-only inspection, B1).
    Repo,
    /// The task's `workspace/` directory, copied verbatim under the eval
    /// dir per run (B2–B4, B6–B7). Copying — not symlinking — keeps the
    /// run writable and byte-identical at start.
    Copy(PathBuf),
    /// A generated long-context corpus of ~40 KB markdown documents
    /// (B5); sized so a sequential read pushes the projection past the
    /// 64 K pruning trigger.
    Corpus,
}

/// Post-task outcome checks: the run is graded by the workspace state it
/// leaves behind, not by the model claiming success. Checks are
/// deliberately cheap and objective — exact bytes where the prompt pins
/// them, distinctive substrings where it does not, and `cargo test`
/// where the task is code. Live runs only: replay re-executes recorded
/// edits for determinism, not for grading.
struct OutcomeSpec {
    /// `(relative path, required bytes)` pairs; one optional trailing
    /// newline is tolerated (editors and writers emit it, and the prompt
    /// pins the text, not the byte count).
    exact_files: Vec<(String, String)>,
    /// `(relative path, required substring)` pairs, case-insensitive: a
    /// proxy for "the file addresses this subject".
    contains: Vec<(String, String)>,
    /// When set, `cargo test` must exit 0 with at least this many passing
    /// tests — the task must not break the suite it touched.
    tests_green: Option<u64>,
}

/// SWE-bench-shaped gate for real-bug tasks: `cargo test` must exit 0,
/// every FAIL_TO_PASS test must have flipped from failing to passing,
/// every PASS_TO_PASS test must still pass (so the fix cannot trade one
/// regression for another), and the protected test files must be
/// byte-identical to their pre-run snapshot. Grading by name subsumes
/// the old passing-count check: disabling the test target removes the
/// names from the passing set.
struct BugfixSpec {
    /// Files the agent must not modify; the gate compares their bytes.
    protected: Vec<String>,
    /// Test names, exactly as libtest prints them, that fail on the
    /// seeded bug and must pass after the run.
    fail_to_pass: Vec<String>,
    /// Test names that pass on the seed and must keep passing.
    pass_to_pass: Vec<String>,
}

enum TaskGate {
    Outcome(OutcomeSpec),
    Bugfix(BugfixSpec),
}

/// A loaded evaluation task. Tasks live as directories under
/// `tests/eval-tasks/<id>/` — a `task.json` manifest, an optional
/// `system-prompt.txt`, and a `workspace/` tree — so the corpus grows by
/// adding files, not harness code. Prompts are frozen: changing one
/// invalidates every baseline recorded against it.
struct EvalTask {
    id: String,
    prompt: String,
    workspace: TaskWorkspace,
    tools: ToolSet,
    /// Long-context or otherwise costly: skipped unless explicitly named
    /// in `LOOM_EVAL_TASKS` (B5, B7, LONG1).
    heavy: bool,
    /// Human-time difficulty tier (`s` ≤15 min, `m` 15–60 min,
    /// `l` 1–4 h, `xl` ≥4 h) — the stratification Multi-SWE-bench
    /// validated; the suite headline reports the pass rate per tier,
    /// because a blended rate hides the tier cliff (84.7 % → 33.1 %
    /// from easy to hard in field data).
    tier: Option<String>,
    /// System prompt: `None` runs with the engine default — coding tasks
    /// must measure the prompt users actually get, so parity cannot
    /// drift. Lab tasks (B1/B5) pin theirs in a file deliberately.
    system_prompt: Option<String>,
    gate: Option<TaskGate>,
    /// Free-form provenance note carried into reports (e.g. the commit a
    /// real-bug task reproduces).
    notes: Option<String>,
}

#[derive(Deserialize)]
struct TaskDef {
    id: String,
    prompt: String,
    tools: String,
    #[serde(default)]
    heavy: bool,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
    workspace: String,
    #[serde(default)]
    gate: Option<GateDef>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum GateDef {
    #[serde(rename = "outcome")]
    Outcome {
        #[serde(default)]
        exact_files: Vec<(String, String)>,
        #[serde(default)]
        contains: Vec<(String, String)>,
        #[serde(default)]
        tests_green: Option<u64>,
    },
    #[serde(rename = "bugfix")]
    Bugfix {
        #[serde(default)]
        protected: Vec<String>,
        fail_to_pass: Vec<String>,
        #[serde(default)]
        pass_to_pass: Vec<String>,
    },
}

fn eval_tasks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("eval-tasks")
}

/// Loads every task directory, sorted by path for deterministic order.
/// Malformed tasks panic: this is a test harness, and a half-loaded
/// corpus would silently shrink the suite.
/// The hard-gate preset keeps exactly the tasks that carry a gate —
/// lab benches (no gate) never run in it, gated heavies always do.
/// Derived, not listed: the gate grows only by addition, so a new
/// gated task cannot be silently left out (anti-benchmaxxing rule).
fn hard_gate_keeps(task: &EvalTask) -> bool {
    task.gate.is_some()
}

fn tasks() -> Vec<EvalTask> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(eval_tasks_dir())
        .expect("eval-tasks directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no task directories in eval-tasks");
    let mut loaded: Vec<EvalTask> = Vec::new();
    for dir in dirs {
        let def: TaskDef =
            serde_json::from_str(&std::fs::read_to_string(dir.join("task.json")).unwrap()).unwrap();
        let tools = match def.tools.as_str() {
            "read-only" => ToolSet::ReadOnly,
            "coding" => ToolSet::Coding,
            "read-edit" => ToolSet::ReadEdit,
            other => panic!("{}: unknown tool set `{other}`", def.id),
        };
        let workspace = match def.workspace.as_str() {
            "repo" => TaskWorkspace::Repo,
            "copy" => TaskWorkspace::Copy(dir.join("workspace")),
            "corpus" => TaskWorkspace::Corpus,
            other => panic!("{}: unknown workspace `{other}`", def.id),
        };
        let gate = def.gate.map(|gate| match gate {
            GateDef::Outcome {
                exact_files,
                contains,
                tests_green,
            } => TaskGate::Outcome(OutcomeSpec {
                exact_files,
                contains,
                tests_green,
            }),
            GateDef::Bugfix {
                protected,
                fail_to_pass,
                pass_to_pass,
            } => TaskGate::Bugfix(BugfixSpec {
                protected,
                fail_to_pass,
                pass_to_pass,
            }),
        });
        let system_prompt = def.system_prompt_file.map(|file| {
            let text = std::fs::read_to_string(dir.join(file)).expect("system prompt file");
            // The file ends with one newline; the prompt itself does not.
            text.strip_suffix('\n').map(str::to_owned).unwrap_or(text)
        });
        if let Some(tier) = &def.tier {
            assert!(
                matches!(tier.as_str(), "s" | "m" | "l" | "xl"),
                "{}: unknown tier `{tier}` (s | m | l | xl)",
                def.id
            );
        }
        loaded.push(EvalTask {
            id: def.id,
            prompt: def.prompt,
            workspace,
            tools,
            heavy: def.heavy,
            tier: def.tier,
            system_prompt,
            gate,
            notes: def.notes,
        });
    }
    assert!(
        loaded.windows(2).all(|pair| pair[0].id != pair[1].id),
        "duplicate task ids under {}",
        eval_tasks_dir().display()
    );
    loaded
}

/// Shared request meter for the live runner: a fractional token bucket
/// refilled at `refill_per_sec`, small burst capacity. Free tiers rate
/// limit by requests/minute (NVIDIA NIM: 40 rpm), so the bucket sizes
/// the eval's total request rate regardless of task concurrency.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    updated: Instant,
}

async fn bucket_acquire(bucket: &Arc<Mutex<TokenBucket>>) {
    loop {
        let wait = {
            let mut b = bucket.lock().unwrap();
            let now = Instant::now();
            b.tokens = (b.tokens + now.duration_since(b.updated).as_secs_f64() * b.refill_per_sec)
                .min(b.capacity);
            b.updated = now;
            if b.tokens >= 1.0 {
                b.tokens -= 1.0;
                0.0
            } else {
                (1.0 - b.tokens) / b.refill_per_sec
            }
        };
        if wait == 0.0 {
            return;
        }
        tokio::time::sleep(Duration::from_secs_f64(wait.min(5.0))).await;
    }
}

/// Waits for a bucket permit before the first byte of the wrapped
/// provider's stream is polled. The kernel's own RateLimited retry
/// policy stays the backstop for provider-side throttling this bucket
/// misses.
struct ThrottledProvider {
    inner: Box<dyn ModelProvider>,
    bucket: Arc<Mutex<TokenBucket>>,
}

impl ModelProvider for ThrottledProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        self.inner.capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let bucket = Arc::clone(&self.bucket);
        Box::pin(
            futures::stream::once(async move { bucket_acquire(&bucket).await })
                .flat_map(move |()| self.inner.stream(request)),
        )
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn eval_dir() -> PathBuf {
    std::env::var("LOOM_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target").join("eval"))
}

fn corpus_root() -> PathBuf {
    eval_dir().join("corpus")
}

/// (Re)creates the long-context corpus (B5): eight markdown documents,
/// each ~40 KB, so a sequential read crosses the 64 K-token pruning
/// trigger around the fifth document. Deterministic bytes, wiped and
/// rewritten each run.
fn ensure_corpus() {
    let root = corpus_root();
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    std::fs::create_dir_all(&root).unwrap();
    for i in 1..=8 {
        let mut body = format!("# Doc {i:02} — subsystem {i:02}\n\n");
        for line in 0..700 {
            let value = (i * 101 + line) % 997;
            body.push_str(&format!(
                "Line {line:04}: subsystem {i:02} records metric {value} and forwards it.\n"
            ));
        }
        std::fs::write(root.join(format!("doc-{i:02}.md")), body).unwrap();
    }
}
/// The per-task run root under the eval dir. Wiped and re-copied (or
/// regenerated) before every run, so runs start from identical bytes.
fn run_root(task_id: &str) -> PathBuf {
    eval_dir().join(task_id)
}

/// (Re)creates a task's run workspace by copying its `workspace/` tree.
fn ensure_copy(src: &Path, run_root: &Path) {
    if run_root.exists() {
        std::fs::remove_dir_all(run_root).unwrap();
    }
    copy_tree(src, run_root).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            // Preserve symlinks as links: imported repos carry git
            // mode-120000 entries that may point at directories (e.g.
            // ripgrep's HomebrewFormula), which `fs::copy` rejects.
            let link = std::fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(&link, &target)?;
        } else if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Whether a task's run workspace should survive its run. Default is to
/// delete it: the gate runs `cargo test` with the run root as its working
/// directory, so every root accumulates a full build tree (hundreds of MB
/// per task) that nothing else reclaims — the growth that filled the disk
/// and aborted the 2026-10-16 sweep. Set `LOOM_EVAL_KEEP_RUN_ROOTS=1` to
/// keep the trees for post-mortem inspection instead. (Unrelated to
/// `LOOM_EVAL_PRUNE`, which selects the context-pruning arm.)
fn run_roots_kept(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(raw) => {
            let value = raw.trim();
            !value.is_empty()
                && !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
        }
    }
}

/// Deletes a task's run root once its artifacts are durable. Best effort:
/// a missing root is expected on a re-run, and a failed delete only warns,
/// because a run that already produced its record must not be failed by
/// cleanup.
fn prune_run_root_when(run_root: &Path, keep: bool) {
    if keep || !run_root.exists() {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(run_root) {
        eprintln!(
            "warning: could not prune run root {}: {error}",
            run_root.display()
        );
    }
}

/// `prune_run_root_when` honouring `LOOM_EVAL_KEEP_RUN_ROOTS`.
fn prune_run_root(run_root: &Path) {
    let keep = run_roots_kept(std::env::var("LOOM_EVAL_KEEP_RUN_ROOTS").ok().as_deref());
    prune_run_root_when(run_root, keep);
}

/// Writes a per-task artifact without letting a full disk abort the sweep
/// (2026-10-16: one `StorageFull` on this exact write unwound all of
/// `eval_live` after 19 of 142 tasks). On `StorageFull` the run root is
/// pruned — its build tree is the space the retry needs — and the write is
/// retried once; any remaining error is reported and the sweep continues.
fn write_artifact(path: &Path, body: &str, run_root: &Path) {
    match std::fs::write(path, body) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::StorageFull => {
            eprintln!(
                "warning: {}: no space left; pruning {} and retrying once",
                path.display(),
                run_root.display()
            );
            prune_run_root_when(run_root, false);
            if let Err(error) = std::fs::write(path, body) {
                eprintln!(
                    "warning: still could not write {} after pruning: {error}",
                    path.display()
                );
            }
        }
        Err(error) => eprintln!("warning: could not write {}: {error}", path.display()),
    }
}

// --------------------------------------------------------------- census ---

/// Per-source attribution for one request, in **exact serialized bytes**
/// (`serde_json` canonical form — no provider, tokenizer, or network
/// involved, so the figure is deterministic and provider-agnostic).
/// `est_tokens` divides by 3 only for comparison with the compiler's budget
/// heuristic (architecture §8).
#[derive(Debug, Default, Serialize)]
struct Census {
    system: u64,
    user: u64,
    assistant: u64,
    tool_result: u64,
    tool_specs: u64,
}

impl Census {
    fn est_tokens(&self) -> u64 {
        (self.system + self.user + self.assistant + self.tool_result + self.tool_specs) / 3
    }
}

fn serialized_size(value: &impl Serialize) -> u64 {
    serde_json::to_string(value)
        .map(|s| s.len() as u64)
        .unwrap_or(0)
}

fn census(request: &ModelRequest) -> Census {
    let mut c = Census::default();
    for message in &request.messages {
        match message {
            Message::System { .. } => c.system += serialized_size(message),
            Message::User { .. } => c.user += serialized_size(message),
            Message::Assistant(_) => c.assistant += serialized_size(message),
            Message::ToolResult(_) => c.tool_result += serialized_size(message),
        }
    }
    c.tool_specs = request.tools.iter().map(serialized_size).sum();
    c
}

// ---------------------------------------------------- record/replay ------'

/// One recorded model exchange: what the compiler produced and what the
/// model answered. Both sides are serde round-trippable (ADR-003 seam).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Exchange {
    request: ModelRequest,
    response: ModelResponse,
}

/// Wraps a live provider (the engine's own build, via
/// `EngineOverrides::with_provider_hook`) and records every completed
/// exchange.
struct RecordingProvider {
    inner: Box<dyn ModelProvider>,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
}

impl RecordingProvider {
    fn new(inner: Box<dyn ModelProvider>) -> (Self, Arc<Mutex<Vec<Exchange>>>) {
        let exchanges = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner,
                exchanges: Arc::clone(&exchanges),
            },
            exchanges,
        )
    }
}

impl ModelProvider for RecordingProvider {
    fn name(&self) -> &str {
        // The label that reaches reports and billing attribution: the
        // wrapped provider, not "record".
        self.inner.name()
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        self.inner.capabilities(model)
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        let inner = self.inner.stream(request);
        let request = request.clone();
        let sink = Arc::clone(&self.exchanges);
        Box::pin(inner.map(move |event| {
            if let Ok(ModelEvent::Completed(response)) = &event {
                sink.lock().unwrap().push(Exchange {
                    request: request.clone(),
                    response: response.clone(),
                });
            }
            event
        }))
    }
}

/// Replays recorded responses verbatim; tools still execute for real.
struct ReplayProvider {
    model: String,
    /// Capabilities derived from the recording itself, not from any
    /// provider's table: the exchanges prove what the live run was
    /// served under, so replay never disagrees with an era's provider.
    capabilities: ModelCapabilities,
    script: Mutex<VecDeque<ModelResponse>>,
    served: AtomicUsize,
    /// How many past-the-recording turns were served. Loop-level
    /// additions that react after the recording's final EndTurn (the
    /// completion gate's refusal) need one extra model beat; genuine
    /// drift needs many, so anything beyond one still fails loud.
    overruns: Arc<AtomicUsize>,
}

impl ReplayProvider {
    fn new(model: &str, exchanges: &[Exchange]) -> Self {
        let tool_calls = exchanges
            .iter()
            .any(|e| !e.response.message.tool_calls.is_empty());
        let parallel_tool_calls = exchanges
            .iter()
            .any(|e| e.response.message.tool_calls.len() > 1);
        let reasoning = exchanges
            .iter()
            .any(|e| e.response.message.reasoning.is_some());
        // Context floor: every recorded request fit the real window, so
        // the largest one bounds it from below. The ×8 headroom (over a
        // ~4-bytes-per-token raw-JSON estimate) keeps replay's pruning
        // compiler from reclaiming memory the live run never reclaimed —
        // equivalence compares transcripts, and an undersized window
        // would prune where live did not, while an oversized one only
        // leaves more room. 256K is the absolute floor.
        let largest_request = exchanges
            .iter()
            .map(|e| {
                serde_json::to_string(&e.request)
                    .map(|text| text.len())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        let window = (largest_request as u32 / 4).saturating_mul(8).max(262_144);
        Self {
            model: model.to_owned(),
            capabilities: ModelCapabilities::new(window)
                .with_tool_calls(tool_calls)
                .with_parallel_tool_calls(parallel_tool_calls)
                .with_reasoning(reasoning),
            script: Mutex::new(exchanges.iter().map(|e| e.response.clone()).collect()),
            served: AtomicUsize::new(0),
            overruns: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many past-the-recording beats were served (the completion
    /// gate's refusal turn); the turn-count assertion allows exactly
    /// this many extra assistant messages.
    #[allow(dead_code)]
    fn served_overruns(&self) -> usize {
        self.overruns.load(Ordering::SeqCst)
    }

    /// Shared handle to the overrun counter, readable after the provider
    /// itself is boxed into the agent.
    fn overruns_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.overruns)
    }
}

impl ModelProvider for ReplayProvider {
    fn name(&self) -> &str {
        "replay"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        // Derived from the recording (see [`ReplayProvider::new`]): the
        // exchanges are ground truth for what tool and reasoning shapes
        // the live run served, so no provider's static table can veto a
        // model replay actually used.
        (model == self.model).then_some(self.capabilities)
    }

    fn capability_source(&self) -> &'static str {
        "recording"
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        assert_eq!(request.model, self.model, "replay: model drifted");
        let next = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
            let overruns = self.overruns.fetch_add(1, Ordering::SeqCst);
            assert!(
                overruns == 0,
                "replay drifted: a second turn past the recording"
            );
            ModelResponse {
                message: AssistantMessage {
                    content: String::from("(replay: past the end of the recording)"),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                },
                usage: Usage::default(),
            }
        });
        self.served.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter([Ok(ModelEvent::Completed(next))]))
    }
}

fn read_exchanges(path: &std::path::Path) -> Vec<Exchange> {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ------------------------------------------------------------- metrics ---

/// Event tally (same shape as benchmark.rs).
#[derive(Default, Clone, Copy)]
struct Tally {
    events: u64,
    retries: u32,
    retries_after_output: u32,
}

struct CountingSink(Arc<Mutex<Tally>>);

impl EventSink for CountingSink {
    fn emit(&self, _task_id: TaskId, kind: EventKind) {
        let mut tally = self.0.lock().unwrap();
        tally.events += 1;
        if let EventKind::ModelRetry {
            response_started, ..
        } = kind
        {
            tally.retries += 1;
            if response_started {
                tally.retries_after_output += 1;
            }
        }
    }
}

/// Peak prices in USD per 1M tokens (docs/roadmap.md, item 1); duplicates
/// benchmark.rs — test binaries cannot share modules across files.
struct Prices {
    input_miss: f64,
    input_hit: f64,
    output: f64,
}

const FLASH: Prices = Prices {
    input_miss: 0.30,
    input_hit: 0.006,
    output: 1.20,
};

const PRO: Prices = Prices {
    input_miss: 1.32,
    input_hit: 0.044,
    output: 3.96,
};

pub(crate) fn cost_usd(
    model: &str,
    provider: &str,
    hit: u64,
    miss: u64,
    output: u64,
    multiplier: f64,
) -> Option<f64> {
    // NVIDIA NIM's free trial tier bills nothing (roadmap item 1's
    // zero-cost candidate); priced-at-zero is a known price, not a
    // missing one.
    if provider == "nvidia" {
        return Some(0.0);
    }
    let prices = match model {
        "deepseek-flash" => &FLASH,
        "deepseek-v4-pro" => &PRO,
        // Free-tier OpenRouter ids price at zero by construction; the
        // provider-reported figure (preferred when present) agrees.
        other if other.ends_with(":free") => return Some(0.0),
        // Any other model is unpriced, not an error: pricing is report
        // metadata, and a model outside the tables must not take a run
        // down. The report says `cost_source: "unpriced"` so the gap is
        // auditable, and the provider-reported cost (preferred whenever
        // the live run emits one) is unaffected.
        _ => return None,
    };
    let per_million = |tokens: u64, price: f64| tokens as f64 / 1_000_000.0 * price;
    Some(
        multiplier
            * (per_million(miss, prices.input_miss)
                + per_million(hit, prices.input_hit)
                + per_million(output, prices.output)),
    )
}

/// Filesystem-safe form of a model id — OpenRouter ids carry `/` and `:`
/// (`deepseek/deepseek-chat-v3-0324:free` → `deepseek-deepseek-chat-v3-0324-free`),
/// and every report/record filename embeds the model.
fn slug(model: &str) -> String {
    model.replace(['/', ':'], "-")
}

/// Peak Mon–Fri 01:00–04:00 & 06:00–10:00 UTC; else off-peak (0.5×).
pub(crate) fn price_multiplier(now: SystemTime) -> (f64, &'static str) {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let weekday = ((seconds / 86_400) + 4) % 7;
    let hour = (seconds % 86_400) / 3_600;
    let peak = (1..=5).contains(&weekday) && ((1..4).contains(&hour) || (6..10).contains(&hour));
    if peak {
        (1.0, "peak")
    } else {
        (0.5, "off-peak")
    }
}

fn aggregate(values: &[u64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!(null);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2] as f64
    } else {
        (sorted[n / 2 - 1] as f64 + sorted[n / 2] as f64) / 2.0
    };
    serde_json::json!({
        "mean": (values.iter().map(|v| *v as f64).sum::<f64>() / n as f64),
        "median": median,
        "min": sorted[0],
        "max": sorted[n - 1],
    })
}

fn aggregate_f64(values: &[f64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!(null);
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    serde_json::json!({
        "mean": values.iter().sum::<f64>() / n as f64,
        "median": median,
        "min": sorted[0],
        "max": sorted[n - 1],
    })
}

// ------------------------------------------------------------ run loop ---

pub(crate) fn registry_for(tools: ToolSet) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool)).unwrap();
    registry.register(Box::new(ListDirTool)).unwrap();
    if !matches!(tools, ToolSet::ReadEdit) {
        registry.register(Box::new(SearchTool)).unwrap();
    }
    if matches!(tools, ToolSet::Coding | ToolSet::ReadEdit) {
        registry.register(Box::new(EditFileTool)).unwrap();
    }
    if matches!(tools, ToolSet::Coding) {
        registry.register(Box::new(WriteFileTool)).unwrap();
        registry.register(Box::new(RunCommandTool::new())).unwrap();
        registry.register(Box::new(VerifyTool::new())).unwrap();
    }
    registry
}

/// 2L measurement hook: force an absolute pruning trigger so a benchmark can
/// exercise the window-relative compiler below the model's window. Unset
/// keeps the production default (80 % of the window, 64 K floor).
fn prune_config() -> PruneConfig {
    match std::env::var("LOOM_EVAL_PRUNE_TRIGGER_TOKENS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        Some(floor_tokens) => PruneConfig {
            trigger_percent: 0,
            floor_tokens,
            ..PruneConfig::default()
        },
        None => PruneConfig::default(),
    }
}

/// 2L/ADR-011: pruning is OFF by default so baseline numbers stay
/// comparable; set `LOOM_EVAL_PRUNE=1` (or on/true) for the pruning arm.
fn prune_enabled() -> bool {
    std::env::var("LOOM_EVAL_PRUNE").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value != "0" && value != "off" && value != "false"
    })
}

/// Roadmap item 1's bare-loop arm: `LOOM_EVAL_BARE=1` runs the same
/// tasks on the basic-harness baseline — no repository facts, no
/// pruning, no completion gate or failure-ledger feedback, and the
/// primitive tool set only — so every full-arm number has a delta to be
/// judged against. Artifacts are suffixed `-bare` so the two arms never
/// overwrite each other's recordings or reports.
fn bare_loop() -> bool {
    std::env::var("LOOM_EVAL_BARE").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value != "0" && value != "off" && value != "false"
    })
}

/// 1.5 measurement arm: `LOOM_EVAL_MAX_TURNS=N` overrides the loop's turn
/// budget for the eval only. Absent — or unparsable, or zero — keeps the
/// product default, so a missing knob cannot silently change what the
/// eval measures. The default itself is never raised here: a higher
/// budget is a hypothesis to test with this knob, not a new product
/// setting.
fn max_turns() -> u32 {
    std::env::var("LOOM_EVAL_MAX_TURNS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|turns| *turns > 0)
        .unwrap_or(DEFAULT_MAX_TURNS)
}

/// The bare-loop registry: the primitive tools a basic harness offers
/// (read/list/write/edit/shell), intersected with the task's set — a
/// ReadEdit task keeps its read/edit shape, a Coding task loses
/// `search`, `verify`, and the artifact refetch.
fn bare_registry(tools: ToolSet) -> ToolRegistry {
    const PRIMITIVES: [&str; 5] = [
        "read_file",
        "list_dir",
        "write_file",
        "edit_file",
        "run_command",
    ];
    ToolRegistry::from_tools(
        registry_for(tools)
            .into_tools()
            .filter(|tool| PRIMITIVES.contains(&tool.spec().name.as_str())),
    )
}

/// Roadmap item 8 phase B: the kernel-owned `todo` tool is a full-arm
/// lever (plan enforcement is part of what the full harness measures),
/// and the bare arm — the basic-harness baseline — must keep excluding
/// it. This is a pure registry-construction check: no provider, no
/// workspace, no network.
#[test]
fn bare_registry_excludes_the_kernel_todo_tool() {
    for tools in [ToolSet::ReadOnly, ToolSet::Coding, ToolSet::ReadEdit] {
        let registry = bare_registry(tools);
        let names: Vec<&str> = registry.specs().map(|spec| spec.name.as_str()).collect();
        assert!(
            !names.contains(&"todo"),
            "the bare arm must never expose the todo tool: {names:?}"
        );
        assert!(
            names.iter().all(|name| [
                "read_file",
                "list_dir",
                "write_file",
                "edit_file",
                "run_command"
            ]
            .contains(name)),
            "the bare arm is the primitive set only: {names:?}"
        );
    }
}

/// The engine overrides for one eval arm. The full arm keeps the
/// production wiring (facts, loop shaping) plus the eval's compiler
/// choice; the bare arm additionally strips facts and loop shaping in
/// the engine and forces the plain append-only compiler — pruning is
/// the full harness's lever, not the baseline's.
fn eval_overrides(
    provider: &str,
    tools: ToolSet,
    hook: ProviderHook,
    bare: bool,
) -> EngineOverrides {
    let mut overrides = EngineOverrides::default()
        .with_provider_hook(hook)
        .with_registry(if bare {
            bare_registry(tools)
        } else {
            registry_for(tools)
        });
    if bare {
        // The plain compiler, estimator-matched like the production
        // chain, but with no pruning wrapper regardless of
        // LOOM_EVAL_PRUNE: the bare arm never gets the lever.
        let estimator = estimator_for(provider);
        overrides = overrides
            .bare_loop()
            .with_compiler(Box::new(DefaultContextCompiler::with_estimator(estimator)));
    } else {
        overrides = overrides.with_compiler(eval_compiler(provider));
    }
    overrides
}

/// The eval's engine configuration for one run root. Everything the
/// harness pins explicitly (model, workspace, no persistence, no
/// per-account targeting); credentials resolve exactly like the CLI
/// (keychain first, then provider env vars) because the db path is the
/// shared one the CLI uses.
pub(crate) fn eval_config(
    root: PathBuf,
    provider: &str,
    model: &str,
    system_prompt: Option<String>,
) -> EngineConfig {
    EngineConfig {
        provider: provider.to_owned(),
        model: model.to_owned(),
        base_url: None,
        reasoning_effort: ReasoningEffort::default(),
        account: None,
        workspace: root,
        system_prompt,
        max_turns: max_turns(),
        db_path: loom_core::paths::db_path(),
        persist: false,
        catalog: None,
    }
}

/// The compiler the eval pins per provider — the same estimator choice
/// `Engine::open` makes for that provider (`estimator_for`), so offline
/// arms can no longer drift from the live wire on token accounting.
/// The pruning arm is the 2L measurement hook.
pub(crate) fn eval_compiler(provider: &str) -> Box<dyn ContextCompiler> {
    let estimator = estimator_for(provider);
    if prune_enabled() {
        Box::new(
            PruningContextCompiler::new(
                Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                prune_config(),
            )
            .with_estimator(estimator),
        )
    } else {
        Box::new(DefaultContextCompiler::with_estimator(estimator))
    }
}

/// Outcome gate, live runs only. Bugfix tasks (SWE-bench-shaped):
/// `cargo test` exits 0, every FAIL_TO_PASS test has flipped to passing,
/// every PASS_TO_PASS test still passes, and the protected files are
/// byte-identical to their pre-run snapshot. Outcome tasks: the
/// workspace state satisfies the task's [`OutcomeSpec`].
async fn grade_gate(
    gate: Option<&TaskGate>,
    root: &Path,
    protected_before: Option<&[(PathBuf, Vec<u8>)]>,
    live: bool,
) -> Option<serde_json::Value> {
    let gate = gate?;
    if !live {
        return None;
    }
    let needs_tests = match gate {
        TaskGate::Bugfix(_) => true,
        TaskGate::Outcome(spec) => spec.tests_green.is_some(),
    };
    let (code, output) = match gate_tests(root, needs_tests).await {
        Ok(pair) => pair,
        Err(error) => return Some(error),
    };
    match gate {
        TaskGate::Bugfix(spec) => Some(grade_bugfix(spec, code, &output, protected_before)),
        TaskGate::Outcome(spec) => Some(grade_outcome(spec, root, code, &output)),
    }
}

/// Runs the fixture tests a gate needs, returning `(exit code, combined
/// output)` — or the ready-to-emit failure JSON when `cargo test` could
/// not run at all. Tasks that need no tests get the empty pair.
async fn gate_tests(
    root: &Path,
    needs_tests: bool,
) -> Result<(Option<i32>, String), serde_json::Value> {
    if !needs_tests {
        return Ok((None, String::new()));
    }
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || run_fixture_tests(&root)).await {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(error)) => Err(serde_json::json!({
            "bug_fixed": false,
            "error": format!("cargo test failed to run: {error}"),
        })),
        Err(join) => Err(serde_json::json!({
            "bug_fixed": false,
            "error": format!("gate task join error: {join}"),
        })),
    }
}

/// The SWE-bench-shaped verdict: exit 0, every FAIL_TO_PASS flipped to
/// passing, every PASS_TO_PASS still passing, and the protected files
/// byte-identical to their pre-run snapshot.
fn grade_bugfix(
    spec: &BugfixSpec,
    code: Option<i32>,
    output: &str,
    protected_before: Option<&[(PathBuf, Vec<u8>)]>,
) -> serde_json::Value {
    let outcomes = test_outcomes(output);
    let passed = |name: &str| outcomes.iter().any(|(n, ok)| n == name && *ok);
    let fail_to_pass: Vec<_> = spec
        .fail_to_pass
        .iter()
        .map(|name| serde_json::json!({"name": name, "passed": passed(name)}))
        .collect();
    let pass_to_pass: Vec<_> = spec
        .pass_to_pass
        .iter()
        .map(|name| serde_json::json!({"name": name, "passed": passed(name)}))
        .collect();
    let protected_unmodified = protected_before.is_some_and(|snapshot| {
        snapshot
            .iter()
            .all(|(path, before)| std::fs::read(path).is_ok_and(|now| now == *before))
    });
    let bug_fixed = code == Some(0)
        && fail_to_pass
            .iter()
            .all(|r| r["passed"].as_bool().unwrap_or(false))
        && pass_to_pass
            .iter()
            .all(|r| r["passed"].as_bool().unwrap_or(false))
        && protected_unmodified;
    serde_json::json!({
        "bug_fixed": bug_fixed,
        "cargo_exit": code,
        "fail_to_pass": fail_to_pass,
        "pass_to_pass": pass_to_pass,
        "tests_passed": outcomes.iter().filter(|(_, ok)| *ok).count(),
        "protected_unmodified": protected_unmodified,
    })
}

/// The outcome-task verdict: every exact-file, contains, and (when
/// requested) tests-green check satisfied by the workspace state.
fn grade_outcome(
    spec: &OutcomeSpec,
    root: &Path,
    code: Option<i32>,
    output: &str,
) -> serde_json::Value {
    let mut checks = Vec::new();
    for (rel, expected) in &spec.exact_files {
        // One optional trailing newline is tolerated (see OutcomeSpec).
        let ok = std::fs::read(root.join(rel))
            .is_ok_and(|body| body.strip_suffix(b"\n").unwrap_or(&body) == expected.as_bytes());
        checks.push(serde_json::json!({"exact": rel, "ok": ok}));
    }
    for (rel, needle) in &spec.contains {
        let ok = std::fs::read_to_string(root.join(rel)).is_ok_and(|body| {
            body.to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase())
        });
        checks.push(serde_json::json!({"contains": rel, "needle": needle, "ok": ok}));
    }
    if let Some(expected) = spec.tests_green {
        let passed = count_tests_passed(output);
        checks.push(serde_json::json!({
            "tests_green": true,
            "ok": code == Some(0) && passed >= expected,
        }));
    }
    let bug_fixed = checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false));
    serde_json::json!({
        "bug_fixed": bug_fixed,
        "checks": checks,
        "cargo_exit": code,
    })
}

/// Per-test outcomes from libtest output: `(name, passed)` for every
/// `test NAME ... ok` / `... FAILED` / `... ignored` line. Ignored counts
/// as not passed — a suite that ignores its regressions has not run
/// them. Summary lines (`test result: ok. …`) do not match the
/// `<name> ... <status>` shape and are skipped.
fn test_outcomes(output: &str) -> Vec<(String, bool)> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("test ")?;
            let (name, status) = rest.rsplit_once(" ... ")?;
            let passed = status == "ok";
            matches!(status, "ok" | "FAILED" | "ignored").then(|| (name.to_owned(), passed))
        })
        .collect()
}

/// `cargo test` inside a fixture, bounded by a wall deadline. Returns the
/// exit code (`None` on timeout) and the combined output.
fn run_fixture_tests(root: &Path) -> std::io::Result<(Option<i32>, String)> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("cargo")
        .args(["test", "--color", "never"])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match child.try_wait()? {
            Some(status) => {
                let output = child.wait_with_output()?;
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                return Ok((status.code(), text));
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok((None, String::from("gate timeout after 300s")));
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Sums the `N passed` counts over every successful libtest summary line.
/// A disabled test target prints `ok. 0 passed`, so this is what catches a
/// manifest that silences the regression suite instead of fixing the code.
fn count_tests_passed(output: &str) -> u64 {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("test result: ok.")?;
            let rest = rest.trim_start();
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
        .sum()
}

/// Runs one task against an engine and returns the 1H-schema report plus
/// the recorded exchanges and the finished session. `provider_label`
/// names the concrete backend the run used, so billed-token figures in
/// the report are attributable ("deepseek", "openrouter", "replay", …)
/// and never read as provider-agnostic.
///
/// The engine arrives through `open`: the caller decides whether it is
/// built live (`open_async_with`, credential-resolved) or replayed
/// (`open_with`, a scripted provider override) — this function only
/// owns the run root, the strict sink, the session, and the report.
async fn run_once<F, O>(
    task: &EvalTask,
    model: &str,
    provider_label: &str,
    exchanges: Option<Arc<Mutex<Vec<Exchange>>>>,
    open: F,
) -> (serde_json::Value, Vec<Exchange>, Session)
where
    F: FnOnce(PathBuf, Option<String>) -> O,
    O: Future<Output = Result<Engine, EngineError>>,
{
    let root = match &task.workspace {
        TaskWorkspace::Repo => workspace_root(),
        TaskWorkspace::Copy(src) => {
            let root = run_root(&task.id);
            ensure_copy(src, &root);
            root
        }
        TaskWorkspace::Corpus => {
            ensure_corpus();
            corpus_root()
        }
    };
    // Bugfix tasks snapshot their protected files after the workspace is
    // (re)generated and before the agent runs, so the gate can prove the
    // tests were graded, not rewritten.
    let protected_before: Option<Vec<(PathBuf, Vec<u8>)>> = match &task.gate {
        Some(TaskGate::Bugfix(spec)) => Some(
            spec.protected
                .iter()
                .map(|rel| {
                    let path = root.join(rel);
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    (path, bytes)
                })
                .collect(),
        ),
        _ => None,
    };
    let engine = open(root.clone(), task.system_prompt.clone())
        .await
        .expect("opening the eval engine");
    let mut session = Session::new(SessionId::new());
    let control = TaskControl::new();
    // V21: strict evaluation sink — a run that loses events fails rather than
    // silently producing a truncated trace. The tally sink cannot drop, so
    // this only bites if the harness ever swaps in a lossy sink.
    let tally_state = Arc::new(Mutex::new(Tally::default()));
    let sink = StrictSink::new(CountingSink(Arc::clone(&tally_state)), control.hard_token());

    let started = Instant::now();
    let outcome = engine
        .run_task(&mut session, task.prompt.clone(), &sink, &control)
        .await;
    let observed_ms = started.elapsed().as_millis() as u64;
    let tally = *tally_state.lock().unwrap();
    assert_eq!(
        sink.dropped(),
        0,
        "{} ({model}): the strict evaluation sink saw dropped events",
        task.id
    );

    // Clone out of the shared mutex: the provider (still owned by the
    // agent) holds the other Arc, so try_unwrap would fail.
    let recorded: Vec<Exchange> = exchanges
        .as_ref()
        .map(|x| x.lock().unwrap().clone())
        .unwrap_or_default();
    // Census over the recorded requests (live runs only). Reported as exact
    // bytes — the provider-agnostic measurement — plus the ÷3 estimate the
    // context budget itself enforces.
    // Per-turn cache accounting: the provider reports the cached prefix
    // tokens for each request, so `hit% = hit / (hit + miss)` is visible per
    // turn without hand-computing it from the run totals.
    let census_turns: Vec<serde_json::Value> = recorded
        .iter()
        .map(|e| {
            let c = census(&e.request);
            let hit = e.response.usage.cached_input_tokens.unwrap_or(0);
            let miss = e.response.usage.input_tokens.saturating_sub(hit);
            serde_json::json!({
                "system_bytes": c.system,
                "user_bytes": c.user,
                "assistant_bytes": c.assistant,
                "tool_result_bytes": c.tool_result,
                "tool_specs_bytes": c.tool_specs,
                "est_tokens": c.est_tokens(),
                "cache_hit_tokens": hit,
                "cache_miss_tokens": miss,
            })
        })
        .collect();
    let final_census = recorded.last().map(|e| census(&e.request));
    let census_totals = final_census.map(|c| {
        serde_json::json!({
            "system_bytes": c.system,
            "user_bytes": c.user,
            "assistant_bytes": c.assistant,
            "tool_result_bytes": c.tool_result,
            "tool_specs_bytes": c.tool_specs,
            "est_tokens": c.est_tokens(),
            "note": "final-turn attribution; bytes are exact, est_tokens = bytes/3 budget heuristic",
        })
    });

    // Micro-eval block: transcript-derived call quality (per-tool fault
    // taxonomy, edit acceptance, identical repeats, spills). Computed in
    // both arms — a failed run still shows what the model did wrong.
    let micro = serde_json::to_value(loom_core::kernel::analytics::session_stats(&session))
        .unwrap_or(serde_json::json!(null));
    // Idle-turn tax (Scaffold-Effect metric): assistant turns that made
    // no tool call — narration without work. Transcript-derived, so it
    // is exact rather than inferred from call counts.
    let no_tool_turns = session
        .transcript
        .iter()
        .filter(|item| matches!(item, Item::Assistant(message) if message.tool_calls.is_empty()))
        .count() as u64;

    // Provider-reported charges win when present (OpenRouter bills the
    // account and reports the exact figure); the DeepSeek table remains
    // the fallback, since DeepSeek never reports cost.
    let reported_cost: Option<f64> = recorded
        .iter()
        .any(|e| e.response.usage.cost_usd.is_some())
        .then(|| {
            recorded
                .iter()
                .filter_map(|e| e.response.usage.cost_usd)
                .sum()
        });

    let mut report = match &outcome {
        Ok(summary) => {
            assert_eq!(
                summary.events_dropped, 0,
                "{} ({model}): dropped {} events; evaluation fails on loss",
                task.id, summary.events_dropped
            );
            let hit = summary.cached_input_tokens.unwrap_or(0);
            let miss = summary.input_tokens.saturating_sub(hit);
            let hit_percent = if hit + miss == 0 {
                0.0
            } else {
                (1000.0 * hit as f64 / (hit + miss) as f64).round() / 10.0
            };
            let (multiplier, window) = price_multiplier(SystemTime::now());
            let table_cost = cost_usd(
                model,
                provider_label,
                hit,
                miss,
                summary.output_tokens,
                multiplier,
            );
            let (cost, cost_source) = match reported_cost {
                Some(reported) => (reported, "provider-reported"),
                None => match table_cost {
                    Some(table) => (table, "price-table"),
                    // Unpriced, not free and not wrong: the model sits in
                    // no price table and the provider reported nothing.
                    // Zero keeps the report total meaningful; the source
                    // label keeps the gap honest.
                    None => (0.0, "unpriced"),
                },
            };
            serde_json::json!({
                "task": task.id,
                "model": model,
                "provider": provider_label,
                "success": true,
                "turns": summary.turns,
                "no_tool_turns": no_tool_turns,
                "tier": task.tier,
                "model_requests": summary.model_requests,
                "input_tokens": summary.input_tokens,
                "cache_hit_tokens": hit,
                "cache_miss_tokens": miss,
                "hit_percent": hit_percent,
                "output_tokens": summary.output_tokens,
                "successful_tool_calls": summary.successful_tool_calls,
                "failed_tool_calls": summary.failed_tool_calls,
                "retries": tally.retries,
                "retries_after_output": tally.retries_after_output,
                "events": tally.events,
                "events_dropped": summary.events_dropped,
                "wall_time_ms": summary.wall_time.as_millis() as u64,
                "observed_wall_time_ms": observed_ms,
                "price_window": window,
                "cost_usd": cost,
                "cost_source": cost_source,
                "compiler_census": {
                    "system_prompt": summary.last_context_census.system_prompt,
                    "repo_facts": summary.last_context_census.repo_facts,
                    "user_items": summary.last_context_census.user_items,
                    "assistant_history": summary.last_context_census.assistant_history,
                    "reasoning_replay": summary.last_context_census.reasoning_replay,
                    "tool_results": summary.last_context_census.tool_results,
                    "tool_specs": summary.last_context_census.tool_specs,
                    "total": summary.last_context_census.total,
                    "note": "compiler-side attribution, ADR-016; ÷3 estimated tokens (budget-facing)",
                },
                "census_final_turn": census_totals,
                "census_per_turn": census_turns,
                "micro": &micro,
            })
        }
        Err(error) => serde_json::json!({
            "task": task.id,
            "model": model,
            "provider": provider_label,
            "success": false,
            "no_tool_turns": no_tool_turns,
            "tier": task.tier,
            "end_reason": error.end_reason(),
            "error": error.to_string(),
            "retries": tally.retries,
            "observed_wall_time_ms": observed_ms,
            "micro": &micro,
        }),
    };
    // Outcome grading for gated tasks; live runs only.
    if let Some(gate) = grade_gate(
        task.gate.as_ref(),
        &root,
        protected_before.as_deref(),
        exchanges.is_some(),
    )
    .await
    {
        report
            .as_object_mut()
            .unwrap()
            .insert("gate".to_owned(), gate);
    }
    (report, recorded, session)
}

/// The invariant wiring for one live run: everything that does not vary
/// between runs of the same task, hoisted so [`execute_run`] and the
/// infrastructure-retry pass in [`retry_infra_runs`] build the *same*
/// engine. A retry that re-derived this wiring could silently differ from
/// the run it replaces (a different throttle, a different arm), and the
/// replacement would no longer be measuring the same thing.
#[derive(Clone)]
struct RunPlan {
    model: String,
    provider: String,
    file_model: String,
    file_arm: String,
    arm: String,
    runs: usize,
    bare: bool,
    bucket: Option<Arc<Mutex<TokenBucket>>>,
    records_dir: PathBuf,
    sessions_dir: PathBuf,
}

/// Runs one (task, run) pair end to end: builds the engine through the
/// recorded/throttled provider hook, arms the per-run watchdog, writes the
/// durable artifacts (record JSONL + session snapshot), prunes the scratch
/// run root, and returns the report.
///
/// The record and session are the durable artifacts; the run root is
/// scratch that the gate filled with a `cargo test` build tree, so it is
/// deleted as soon as both are written.
async fn execute_run(task: &EvalTask, run: usize, plan: &RunPlan) -> serde_json::Value {
    let RunPlan {
        model,
        provider,
        file_model,
        file_arm,
        arm,
        runs,
        bare,
        bucket,
        records_dir,
        sessions_dir,
    } = plan;
    let exchanges = Arc::new(Mutex::new(Vec::new()));
    let hook_exchanges = Arc::clone(&exchanges);
    let hook_bucket = bucket.clone();
    let hook = Arc::new(move |built: Box<dyn ModelProvider>| {
        let inner: Box<dyn ModelProvider> = match &hook_bucket {
            Some(bucket) => Box::new(ThrottledProvider {
                inner: built,
                bucket: Arc::clone(bucket),
            }),
            None => built,
        };
        Box::new(RecordingProvider {
            inner,
            exchanges: Arc::clone(&hook_exchanges),
        }) as Box<dyn ModelProvider>
    }) as ProviderHook;
    let overrides = eval_overrides(provider, task.tools, hook, *bare);
    // `run_once` takes an `FnOnce`: the engine overrides move into the
    // single closure invocation rather than being cloned per attempt.
    let open_model = model.clone();
    let open_provider = provider.clone();
    let open = move |root: PathBuf, system_prompt: Option<String>| {
        let overrides = overrides;
        let open_model = open_model.clone();
        let open_provider = open_provider.clone();
        async move {
            Engine::open_async_with(
                eval_config(root, &open_provider, &open_model, system_prompt),
                overrides,
            )
            .await
        }
    };
    // Per-run watchdog: a live NIM stall (2026-10-15) wedged whole sweeps
    // forever; a run that exceeds LOOM_EVAL_RUN_TIMEOUT_SECS (default
    // 1800) fails the run instead of hanging the sweep. Heavy tier-l
    // tasks should raise it.
    let watchdog = std::time::Duration::from_secs(
        std::env::var("LOOM_EVAL_RUN_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1800),
    );
    let provider_label = provider.clone();
    let attempt = run_once(task, model, &provider_label, Some(exchanges), open);
    let (report, recorded, session) = match tokio::time::timeout(watchdog, attempt).await {
        Ok((report, recorded, session)) => (report, recorded, Some(session)),
        Err(_) => (
            // The reason is stamped here rather than left to a downstream
            // default: an absent `end_reason` renders as `unknown`, which
            // is indistinguishable from a report that simply lost the
            // field — and `unknown` counts as infrastructure, so the run
            // would be excluded from the denominator with no explanation.
            // `task`/`model`/`tier` ride along so the kill stays
            // attributable to a task, a model, and a tier.
            serde_json::json!({
                "task": task.id,
                "model": model,
                "provider": provider_label,
                "success": false,
                "tier": task.tier,
                "end_reason": "watchdog_timeout",
                "error": "run watchdog timeout",
                "gate": serde_json::Value::Null,
            }),
            Vec::new(),
            None,
        ),
    };
    eprintln!(
        "{} [{arm}] run {run}/{runs}: success={} bug_fixed={}",
        task.id, report["success"], report["gate"]["bug_fixed"]
    );
    let root = run_root(task.id.as_str());
    let body = recorded
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_artifact(
        &records_dir.join(format!("{}-{file_model}{file_arm}-{run}.jsonl", task.id)),
        &body,
        &root,
    );
    if let Some(session) = session {
        write_artifact(
            &sessions_dir.join(format!(
                "{}-{file_model}{file_arm}-{run}.session.json",
                task.id
            )),
            &serde_json::to_string(&session).unwrap(),
            &root,
        );
    }
    prune_run_root(&root);
    report
}

/// The maximum number of re-measurements a single infrastructure-lost run
/// gets, from `LOOM_EVAL_INFRA_RETRIES` (default 1, `0` disables the pass).
///
/// The bound is per run, not per sweep: a provider that is down should cost
/// one wasted request per slot, not N. A dead endpoint (NIM on 2026-10-18)
/// therefore doubles nothing — every retry fails the same way and the run is
/// still reported as infrastructure, which is the honest outcome.
fn infra_retries() -> usize {
    infra_retries_from(std::env::var("LOOM_EVAL_INFRA_RETRIES").ok().as_deref())
}

/// `infra_retries` over an explicit value. An unparseable setting falls back
/// to the default rather than to 0: a typo must not silently turn retries
/// off, which would leave the sample holes exactly where they were.
fn infra_retries_from(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
}

/// The slots the retry pass will re-measure: one entry per
/// infrastructure-lost run, carrying the reason it replaces.
///
/// Selection is exactly [`is_infra_run`], so the pass can never re-roll an
/// agent negative. Run indices are the *positions* in each task's report
/// vector, which the scheduler pushes in run order; [`retry_infra_runs`]
/// converts them back to 1-based run numbers.
fn infra_retry_slots(
    selected: &[&EvalTask],
    by_task: &Arc<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>>,
) -> Vec<(String, usize, String)> {
    let map = by_task.lock().unwrap();
    let mut pending = Vec::new();
    for task in selected {
        let Some(reports) = map.get(task.id.as_str()) else {
            continue;
        };
        for (index, report) in reports.iter().enumerate() {
            if is_infra_run(report) {
                pending.push((task.id.to_owned(), index, end_reason(report).to_owned()));
            }
        }
    }
    pending
}

/// Re-measures every run the first sweep lost to infrastructure.
///
/// A run that died on the wire, on the watchdog, or before it could stamp a
/// reason never measured the agent: [`is_infra_run`] excludes it from
/// `pass_rate`, so leaving it there does not corrupt the number — it just
/// shrinks the sample. On the 2026-10-17 corpus that was 23 of 115 attempts
/// (20%), which is large enough to turn every headline into a sample size
/// nobody trusts. Each such slot gets `LOOM_EVAL_INFRA_RETRIES` extra
/// attempts through the *same* [`RunPlan`], and the retry's report replaces
/// the original in place.
///
/// Only infrastructure is retried, never an agent negative: `max_turns`,
/// `budget_exhausted`, and `gate_failed` are the measurements, and
/// re-rolling them until they pass would be grading the harness's luck.
///
/// The replacement keeps a `retried_from` breadcrumb naming the reason it
/// replaces, so a repaired run is distinguishable from one that succeeded
/// first time — the sweep's cost and flakiness story stays readable.
async fn retry_infra_runs(
    selected: &[&EvalTask],
    plan: &RunPlan,
    by_task: &Arc<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>>,
) {
    let budget = infra_retries();
    if budget == 0 {
        return;
    }
    // Snapshot the slots to repair: (task id, run index within the task's
    // report vector, reason being replaced). Collected under one lock and
    // retried outside it — a retry re-enters the map to swap its report in.
    let pending = infra_retry_slots(selected, by_task);
    if pending.is_empty() {
        return;
    }
    let tasks: std::collections::HashMap<&str, &EvalTask> =
        selected.iter().map(|t| (t.id.as_str(), *t)).collect();
    eprintln!(
        "retrying {} infrastructure-lost run(s) ({budget} attempt(s) each)",
        pending.len()
    );
    let mut repaired = 0usize;
    for (task_id, index, previous) in &pending {
        let Some(task) = tasks.get(task_id.as_str()).copied() else {
            continue;
        };
        // Runs within a task stay sequential: each run wipes and re-copies
        // that task's run root, so two retries of one task cannot overlap.
        let attempt = execute_run(task, index + 1, plan).await;
        // Decided before `attempt` is moved into the map, using the same
        // predicate the headline uses. A retry that came back infrastructure
        // means the endpoint is down, and the run stays reported as
        // infrastructure — an honest measurement of a broken provider rather
        // than a hole in the sample.
        let still_infra = is_infra_run(&attempt);
        let reason = end_reason(&attempt).to_owned();
        let mut map = by_task.lock().unwrap();
        if let Some(reports) = map.get_mut(task_id.as_str())
            && let Some(slot) = reports.get_mut(*index)
        {
            let mut replacement = attempt;
            if let Some(object) = replacement.as_object_mut() {
                object.insert("retried_from".to_owned(), serde_json::json!(previous));
            }
            *slot = replacement;
        }
        drop(map);
        if still_infra {
            eprintln!(
                "warning: {task_id} run {} still lost to {reason}",
                index + 1
            );
        } else {
            repaired += 1;
        }
    }
    eprintln!(
        "retry pass: repaired {repaired} of {} infrastructure-lost run(s)",
        pending.len()
    );
}

/// N live runs of every runnable task, recordings + aggregates to disk.
#[tokio::test]
#[ignore = "live: needs DEEPSEEK_API_KEY (or OPENROUTER_API_KEY); spends real tokens"]
async fn eval_live() {
    // Provider/stream diagnostics reach stderr in the live harness just
    // like the CLI (RUST_LOG; default quiet) — the runner is the one
    // place a silent provider stall is undiagnosable without them.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let provider_name =
        std::env::var("LOOM_EVAL_PROVIDER").unwrap_or_else(|_| String::from("deepseek"));
    let model = std::env::var("LOOM_EVAL_MODEL").unwrap_or_else(|_| String::from("deepseek-flash"));
    // Credential probe, through the engine: resolve exactly like the CLI
    // (keychain account, then provider env vars). No credential → the
    // old skip UX; any other failure is a harness bug and panics. The
    // probe also warms the OpenRouter catalog cache (engine::catalog)
    // when the openrouter arm runs, so per-run opens stay offline.
    match Engine::open_async_with(
        eval_config(workspace_root(), &provider_name, &model, None),
        EngineOverrides::default(),
    )
    .await
    {
        Err(EngineError::Auth(AuthError::NoCredential { .. })) => {
            eprintln!("skipping eval_live: no credential resolved for {provider_name}");
            return;
        }
        Err(error) => panic!("opening the {provider_name} engine: {error}"),
        Ok(_) => {}
    }
    let runs: usize = std::env::var("LOOM_EVAL_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    // Bounded task-level concurrency (default 1 = the sequential
    // behavior). Runs within a task stay sequential: each task wipes and
    // re-copies its own run root per run, so same-task runs must not
    // overlap; different tasks use disjoint roots.
    let concurrency: usize = std::env::var("LOOM_EVAL_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    // Measurement mode: a baseline run must survive a failed run and still
    // write every remaining task's report plus the suite headline. Strict
    // (asserting every run green) stays the default — that is the CI
    // contract; tolerance is opt-in per invocation.
    let tolerate_failures = std::env::var("LOOM_EVAL_TOLERATE_FAILURES")
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    let dir = eval_dir();
    let records_dir = dir.join("records");
    std::fs::create_dir_all(&records_dir).unwrap();
    // Full session snapshots (transcript JSON) per run: what the offline
    // micro-eval walker and `loom stats` consume — the same numbers,
    // recomputable without spending a token.
    let sessions_dir = records_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    // `LOOM_EVAL_TASKS=hard-gate` names the release gate: every task
    // that carries a gate (the lab tasks B1/B5 are excluded), heavy
    // tasks included. The preset is derived from the corpus, not
    // listed, so it grows only by addition — a new gated task cannot
    // be silently left out of the gate.
    let raw_tasks = std::env::var("LOOM_EVAL_TASKS").ok();
    let hard_gate = raw_tasks
        .as_deref()
        .is_some_and(|raw| raw.trim() == "hard-gate");
    let only: Option<Vec<String>> = if hard_gate {
        None
    } else {
        raw_tasks.map(|raw| raw.split(',').map(|id| id.trim().to_owned()).collect())
    };

    // The arm this invocation runs (roadmap item 1): `full` is the
    // production wiring, `bare` the basic-harness baseline. Artifacts
    // carry the arm suffix so both arms coexist under one eval dir.
    let bare = bare_loop();
    let arm = if bare { "bare" } else { "full" };
    let file_arm = if bare { "-bare" } else { "" };

    let all_tasks = tasks();
    // OpenRouter model ids carry `/` and `:` — every artifact filename
    // embeds the model, so files use this slug instead.
    let file_model = slug(&model);
    // Task selection first, then scheduling: the same filters as before,
    // collected so the runner can schedule tasks instead of walking
    // them in order.
    let selected: Vec<&EvalTask> = all_tasks
        .iter()
        .filter(|task| {
            if let Some(only) = &only
                && !only.iter().any(|id| id.as_str() == task.id)
            {
                return false;
            }
            // Hard gate: everything that carries a gate, lab benches never.
            if hard_gate && !hard_gate_keeps(task) {
                return false;
            }
            // Heavy (long-context) tasks run only when explicitly named or
            // under the hard gate: the default suite bounds time and cost.
            if task.heavy && only.is_none() && !hard_gate {
                return false;
            }
            true
        })
        .collect();

    // Optional shared request meter across every concurrent run. Size
    // LOOM_EVAL_RPM under the provider cap (e.g. 36 against NVIDIA NIM's
    // 40 rpm) so the kernel's RateLimited retry stays a backstop.
    let bucket = std::env::var("LOOM_EVAL_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|rpm| *rpm > 0.0)
        .map(|rpm| {
            Arc::new(Mutex::new(TokenBucket {
                tokens: rpm.min(8.0),
                capacity: 8.0,
                refill_per_sec: rpm / 60.0,
                updated: Instant::now(),
            }))
        });

    // Run-index-major scheduling: every task completes run N before any
    // starts run N+1, so a rate-limit wall or a kill leaves a complete
    // 1-run sweep (then 2-run, ...) — medians arrive early and nothing
    // is half-finished. Tasks run concurrently up to
    // LOOM_EVAL_CONCURRENCY; runs within a task stay sequential because
    // each task wipes and re-copies its own run root per run.
    let by_task: Arc<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // One plan for the whole sweep. The main loop clones it per run, and the
    // retry pass below reuses it verbatim, so a repaired run is wired
    // exactly like the run it replaces.
    let plan = RunPlan {
        model: model.clone(),
        provider: provider_name.clone(),
        file_model: file_model.clone(),
        file_arm: file_arm.to_owned(),
        arm: arm.to_owned(),
        runs,
        bare,
        bucket,
        records_dir: records_dir.clone(),
        sessions_dir: sessions_dir.clone(),
    };
    for run in 1..=runs {
        futures::stream::iter(&selected)
            .for_each_concurrent(concurrency, |task| {
                let task: &EvalTask = task;
                let by_task = Arc::clone(&by_task);
                let plan = plan.clone();
                async move {
                    // The recording hook wraps whichever provider the engine
                    // builds (credential-resolved, catalog-aware) — the eval
                    // observes production wiring instead of reproducing it.
                    let report = execute_run(task, run, &plan).await;
                    by_task
                        .lock()
                        .unwrap()
                        .entry(task.id.to_owned())
                        .or_default()
                        .push(report);
                }
            })
            .await;
    }

    retry_infra_runs(&selected, &plan, &by_task).await;

    let by_task = Arc::try_unwrap(by_task)
        .expect("eval runner holds the only map reference")
        .into_inner()
        .unwrap();
    let mut suite: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for task in selected {
        let reports = by_task.get(task.id.as_str()).cloned().unwrap_or_default();
        let report = serde_json::json!({
            "harness": "eval 2A",
            "arm": arm,
            "task": task.id,
            "model": model,
            "provider": provider_name,
            "units": {
                "census": "exact serialized bytes (provider-agnostic, deterministic)",
                "est_tokens": "bytes/3 budget heuristic",
                "tokens/cost": "provider-billed, per-model evidence only",
            },
            "runs": reports,
            "aggregates": aggregates_from(&reports),
            "notes": task.notes,
        });
        let path = dir.join(format!("report-{}-{file_model}{file_arm}.json", task.id));
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        eprintln!("wrote {}", path.display());
        if !tolerate_failures {
            assert!(
                reports.iter().all(|r| r["success"].as_bool().unwrap()),
                "{}: at least one live run failed",
                task.id
            );
            // Gated tasks are graded by outcome: a run whose gate failed
            // (tests not green, tests rewritten, or suite disabled) fails the
            // live eval even though the transcript itself completed.
            assert!(
                reports
                    .iter()
                    .filter(|r| r.get("gate").is_some_and(|g| !g.is_null()))
                    .all(|r| r["gate"]["bug_fixed"].as_bool().unwrap_or(false)),
                "{}: at least one run failed the bug-fix gate",
                task.id
            );
        }
        suite.push((task.id.clone(), reports));
    }

    // Suite headline: the pass rate over every gated run of the session —
    // the number the suite exists to move. Lab tasks (B1/B5) carry no
    // gate and do not enter the rate.
    let headline = suite_headline(
        &model,
        if tolerate_failures {
            "tolerant"
        } else {
            "strict"
        },
        arm,
        &suite,
    );
    let headline_path = dir.join(format!("report-suite-{file_model}{file_arm}.json"));
    std::fs::write(
        &headline_path,
        serde_json::to_string_pretty(&headline).unwrap(),
    )
    .unwrap();
    eprintln!(
        "suite pass rate: {}/{} gated runs ({}) — wrote {}",
        headline["gated_solved"],
        headline["gated_runs"],
        headline["pass_rate"],
        headline_path.display()
    );
}

/// Rebuilds the suite headline from per-task report files already on
/// disk — the merge path for a split live campaign. A live invocation
/// overwrites `report-suite-<model><arm>.json` with only the tasks it
/// ran; when breadth tasks land mid-campaign, the second invocation's
/// headline would clobber the first. The faithful union loads every
/// per-task report (each embeds its full `runs` array) and reuses
/// [`suite_headline`] verbatim, so no economics math is reimplemented.
/// Default scope: the hard gate, and every file must exist — a missing
/// report is a release-gate violation, not something to skip silently.
/// `LOOM_EVAL_TASKS=<ids>` narrows the union to exactly those tasks.
#[tokio::test]
#[ignore = "merge: rebuilds a suite headline from per-task reports on disk (no spend)"]
async fn eval_suite_union() {
    let model = std::env::var("LOOM_EVAL_MODEL").unwrap_or_else(|_| String::from("deepseek-flash"));
    let bare = bare_loop();
    let arm = if bare { "bare" } else { "full" };
    let file_arm = if bare { "-bare" } else { "" };
    let file_model = slug(&model);
    let raw_tasks = std::env::var("LOOM_EVAL_TASKS").ok();
    let hard_gate = raw_tasks
        .as_deref()
        .is_some_and(|raw| raw.trim() == "hard-gate");
    let only: Option<Vec<String>> = if hard_gate || raw_tasks.is_none() {
        None
    } else {
        raw_tasks.map(|raw| raw.split(',').map(|id| id.trim().to_owned()).collect())
    };
    let dir = eval_dir();
    let mut suite: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for task in tasks() {
        if let Some(only) = &only
            && !only.iter().any(|id| id.as_str() == task.id)
        {
            continue;
        }
        if only.is_none() && !hard_gate_keeps(&task) {
            continue;
        }
        let path = dir.join(format!("report-{}-{file_model}{file_arm}.json", task.id));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing per-task report {}", path.display()));
        let report: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("unreadable report {}: {e}", path.display()));
        let runs = report["runs"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("report {} carries no runs array", path.display()));
        suite.push((task.id.clone(), runs));
    }
    assert!(
        !suite.is_empty(),
        "no per-task reports for {file_model}{file_arm} under {}",
        dir.display()
    );
    let mode = if std::env::var("LOOM_EVAL_TOLERATE_FAILURES")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "tolerant"
    } else {
        "strict"
    };
    let headline = suite_headline(&model, mode, arm, &suite);
    let path = dir.join(format!("report-suite-{file_model}{file_arm}-union.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&headline).unwrap()).unwrap();
    println!(
        "union of {} tasks, {}/{} gated runs — wrote {}",
        suite.len(),
        headline["gated_solved"],
        headline["gated_runs"],
        path.display()
    );
}

/// Builds the suite headline from per-task run reports — pure, so a unit
/// test can pin the economics math without a live run.
///
/// **The pass-rate population.** `gated_runs` counts *verdict-bearing*
/// runs: graded, and not lost to infrastructure (see [`is_infra_run`]).
/// The grader evaluates the workspace however a run ended, so a run
/// killed by the wire or the watchdog can still carry a `gate`; those
/// runs are excluded, because grading a crashed run awards solves to
/// crashes and charges crashes to the agent. `attempted_runs` reports
/// everything the suite spent, and each field task carries `attempted`
/// and `infra` beside its `rate`, so a denominator thinned by flaky
/// infrastructure is visible instead of silently flattering the number.
///
/// Beyond the pass rate it carries the release-gate economics:
/// spend per solved run, turns per solved run, the token-weighted
/// cache-read ratio over successful runs, and the transcript-derived
/// tool-failure rate. Failed runs carry no cost/turn numbers (the
/// provider may never have completed a request), so every economics
/// field states its population in the note.
///
/// Efficiency extensions (the SWE-Effi / Scaffold-Effect conventions):
/// `tokens_per_solved` amortizes every billed token — failures bill too
/// — over solved gated runs; `expensive_failure_ratio` is the share of
/// spend that unresolved gated runs consumed (field data: failures burn
/// multiples of successes); `failure_categories` counts how runs died
/// (`end_reason` for transcript failures, `gate_failed` for runs that
/// finished but did not fix); `idle_turns` sums the no-tool-call
/// assistant turns; `by_tier` stratifies the pass rate by the task's
/// human-time tier, because a blended rate hides the tier cliff; and
/// `models` groups gated outcomes by the model id each run itself
/// names — the per-model comparison dimension (roadmap item 1's
/// remaining half), authoritative even in a suite merged across
/// models. The section is absent when no gated run carries a model id
/// (pre-schema reports), so old artifacts render unchanged.
/// The run's recorded end reason. Reports written before the watchdog
/// stamped one — and any run torn down before its reason reached the
/// report — fall back to `unknown`, which [`is_infra_run`] treats as
/// infrastructure: an unlabeled run never earned an agent verdict.
fn end_reason(run: &serde_json::Value) -> &str {
    run["end_reason"].as_str().unwrap_or("unknown")
}

/// True when the run died outside the agent's control: the wire (transport,
/// HTTP, auth), the per-run watchdog, or a teardown that never stamped a
/// reason. Such a run **may still carry a `gate`** — the grader evaluates
/// the workspace however the run ended, so a run that died on turn 1 of 40
/// can be graded `bug_fixed: true` off whatever it had already written.
/// Grading a crashed run therefore says nothing about agent capability:
/// it awards solves to crashes and charges crashes to the agent. These
/// runs are excluded from `pass_rate`, `by_tier`, and `models`, and may be
/// re-run. The agent's own negatives (`max_turns`, `budget_exhausted`,
/// `gate_failed`, …) are not infrastructure and are never re-run.
fn is_infra_run(run: &serde_json::Value) -> bool {
    // A run that reported success has already cleared the agent; absence
    // of an `end_reason` is the normal shape of a successful run.
    if run["success"].as_bool().unwrap_or(false) {
        return false;
    }
    matches!(
        end_reason(run),
        "model_error" | "watchdog_timeout" | "unknown"
    )
}

/// A run that produced a real verdict about the agent: graded, and not
/// lost to infrastructure.
fn is_verdict_run(run: &serde_json::Value) -> bool {
    run.get("gate").is_some_and(|g| !g.is_null()) && !is_infra_run(run)
}

fn suite_headline(
    model: &str,
    mode: &str,
    arm: &str,
    suite: &[(String, Vec<serde_json::Value>)],
) -> serde_json::Value {
    let mut per_task = serde_json::Map::new();
    let mut gated_runs = 0usize;
    let mut gated_solved = 0usize;
    let mut solved_turns: Vec<u64> = Vec::new();
    let mut total_cost = 0.0;
    let mut failed_cost = 0.0;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut hit_tokens = 0u64;
    let mut miss_tokens = 0u64;
    let mut failed_calls = 0u64;
    let mut total_calls = 0u64;
    let mut idle_turns = 0u64;
    let mut all_turns = 0u64;
    let mut categories: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut tiers: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    // model id → (gated runs, gated solved): the comparison dimension.
    // Keyed by `runs[].model` — the label the run itself carries — so a
    // suite merged across models or campaigns stays attributable.
    let mut models: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut infra_failures: u64 = 0;
    let mut agent_negatives: u64 = 0;
    let mut attempted_runs = 0usize;
    let mut category = |run: &serde_json::Value| {
        if !run["success"].as_bool().unwrap_or(false) {
            let reason = end_reason(run).to_owned();
            if is_infra_run(run) {
                infra_failures += 1;
            } else {
                agent_negatives += 1;
            }
            *categories.entry(reason).or_default() += 1;
        } else if run.get("gate").is_some_and(|g| !g.is_null())
            && !run["gate"]["bug_fixed"].as_bool().unwrap_or(false)
        {
            agent_negatives += 1;
            *categories.entry(String::from("gate_failed")).or_default() += 1;
        }
    };
    for (id, reports) in suite {
        // The pass-rate denominator: graded runs that actually reached the
        // agent. `gate` presence alone is not enough — a run killed by the
        // wire or the watchdog is still graded, and letting it in both
        // inflates the numerator (a crash that had already written the fix)
        // and deflates the apparent rate (a crash that had not).
        let gated: Vec<&serde_json::Value> = reports.iter().filter(|r| is_verdict_run(r)).collect();
        attempted_runs += reports.len();
        let entry = if gated.is_empty() {
            serde_json::json!({
                "kind": "lab",
                "runs": reports.len(),
                // No verdict-bearing run at all — a lab task by
                // construction, or a field task whose every run was lost
                // to infrastructure. Either way the spend is visible here.
                "attempted": reports.len(),
                "infra": reports.iter().filter(|r| is_infra_run(r)).count(),
            })
        } else {
            let solved = gated
                .iter()
                .filter(|r| r["gate"]["bug_fixed"].as_bool().unwrap_or(false))
                .count();
            gated_runs += gated.len();
            gated_solved += solved;
            solved_turns.extend(
                gated
                    .iter()
                    .filter(|r| r["gate"]["bug_fixed"].as_bool().unwrap_or(false))
                    .filter_map(|r| r["turns"].as_u64()),
            );
            // Tier rollup: every gated run enters its task's tier bucket.
            if let Some(tier) = reports
                .iter()
                .find_map(|r| r["tier"].as_str().map(str::to_owned))
            {
                let bucket = tiers.entry(tier).or_default();
                bucket.0 += gated.len() as u64;
                bucket.1 += solved as u64;
            }
            serde_json::json!({
                "kind": "field",
                "runs": gated.len(),
                "solved": solved,
                "rate": solved as f64 / gated.len() as f64,
                // What `runs` withheld: the attempts this task spent on
                // infrastructure, so a thin denominator is visible next to
                // the rate it produced instead of hiding behind it.
                "attempted": reports.len(),
                "infra": reports.iter().filter(|r| is_infra_run(r)).count(),
            })
        };
        per_task.insert(id.clone(), entry);
        for run in reports {
            // Per-model tally: verdict-bearing runs only, grouped under the
            // model the run names. Runs without a model id contribute
            // nothing, which is what keeps old reports section-free.
            if is_verdict_run(run)
                && let Some(model_id) = run["model"].as_str()
            {
                let slot = models.entry(model_id.to_owned()).or_default();
                slot.0 += 1;
                slot.1 += u64::from(run["gate"]["bug_fixed"].as_bool().unwrap_or(false));
            }
            let cost = run["cost_usd"].as_f64().unwrap_or(0.0);
            total_cost += cost;
            let run_gated_unsolved = run.get("gate").is_some_and(|g| !g.is_null())
                && !run["gate"]["bug_fixed"].as_bool().unwrap_or(false);
            if run_gated_unsolved || !run["success"].as_bool().unwrap_or(false) {
                failed_cost += cost;
            }
            category(run);
            if run["success"].as_bool().unwrap_or(false) {
                hit_tokens += run["cache_hit_tokens"].as_u64().unwrap_or(0);
                miss_tokens += run["cache_miss_tokens"].as_u64().unwrap_or(0);
                input_tokens += run["input_tokens"].as_u64().unwrap_or(0);
                output_tokens += run["output_tokens"].as_u64().unwrap_or(0);
            }
            idle_turns += run["no_tool_turns"].as_u64().unwrap_or(0);
            all_turns += run["turns"].as_u64().unwrap_or(0);
            failed_calls += run["micro"]["totals"]["failed"].as_u64().unwrap_or(0);
            total_calls += run["micro"]["totals"]["calls"].as_u64().unwrap_or(0);
        }
    }
    let mean_turns = |xs: &[u64]| -> Option<f64> {
        (!xs.is_empty()).then(|| xs.iter().sum::<u64>() as f64 / xs.len() as f64)
    };
    let median_turns = |xs: &mut Vec<u64>| -> Option<f64> {
        if xs.is_empty() {
            return None;
        }
        xs.sort_unstable();
        let n = xs.len();
        Some(if n % 2 == 1 {
            xs[n / 2] as f64
        } else {
            (xs[n / 2 - 1] as f64 + xs[n / 2] as f64) / 2.0
        })
    };
    let round4 = |v: f64| (v * 1e4).round() / 1e4;
    let mut headline = serde_json::json!({
        "model": model,
        "mode": mode,
        "arm": arm,
        "tasks": per_task,
        "gated_runs": gated_runs,
        "gated_solved": gated_solved,
        "attempted_runs": attempted_runs,
        "pass_rate": if gated_runs == 0 {
            serde_json::json!(null)
        } else {
            serde_json::json!(gated_solved as f64 / gated_runs as f64)
        },
        "by_tier": tiers.iter().map(|(tier, (runs, solved))| {
            serde_json::json!({"tier": tier, "runs": runs, "solved": solved, "rate": *solved as f64 / *runs as f64})
        }).collect::<Vec<_>>(),
        "failure_categories": categories,
        "infra_failures": infra_failures,
        "agent_negatives": agent_negatives,
        "economics": {
            "total_cost_usd": (total_cost * 1e4).round() / 1e4,
            "usd_per_solved": if gated_solved == 0 { serde_json::json!(null) } else {
                serde_json::json!(round4(total_cost / gated_solved as f64))
            },
            "tokens_per_solved": if gated_solved == 0 { serde_json::json!(null) } else {
                serde_json::json!((input_tokens + output_tokens) / gated_solved as u64)
            },
            "expensive_failure_ratio": if total_cost == 0.0 { serde_json::json!(null) } else {
                serde_json::json!(round4(failed_cost / total_cost))
            },
            "turns_per_solved_mean": mean_turns(&solved_turns),
            "turns_per_solved_median": median_turns(&mut solved_turns),
            "idle_turns": idle_turns,
            "idle_turn_rate": if all_turns == 0 { serde_json::json!(null) } else {
                serde_json::json!(round4(idle_turns as f64 / all_turns as f64))
            },
            "note": "cost over all runs (failures bill too); turns over gated solved runs; tokens = billed input+output over successful runs, amortized per solved",
            "cache_read_ratio": if hit_tokens + miss_tokens == 0 {
                serde_json::json!(null)
            } else {
                serde_json::json!(round4(hit_tokens as f64 / (hit_tokens + miss_tokens) as f64))
            },
            "cache_note": "token-weighted over successful runs only",
            "tool_failure_rate": {
                "failed": failed_calls,
                "total": total_calls,
                "rate": if total_calls == 0 { serde_json::json!(null) } else {
                    serde_json::json!(round4(failed_calls as f64 / total_calls as f64))
                },
                "note": "micro-eval totals over all runs incl. failures",
            },
        },
    });
    // The per-model section is additive and only ever written when some
    // gated run named a model — an absent key (not a null) is the
    // backward-compatibility contract for pre-model-schema reports.
    if !models.is_empty() {
        let section = models
            .iter()
            .map(|(model_id, (runs, solved))| {
                (
                    model_id.clone(),
                    serde_json::json!({
                        "runs": runs,
                        "solved": solved,
                        "rate": *solved as f64 / *runs as f64,
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        headline
            .as_object_mut()
            .unwrap()
            .insert("models".to_owned(), serde_json::Value::Object(section));
    }
    headline
}

#[test]
fn suite_headline_separates_infra_failures_from_agent_negatives() {
    let died = |reason: &str| {
        serde_json::json!({
            "success": false,
            "end_reason": reason,
            "turns": 4,
            "cost_usd": 0.01,
        })
    };
    let suite = vec![(
        "B1".to_owned(),
        vec![
            // Transport/HTTP death: no verdict about the agent.
            died("model_error"),
            // Exhausted its turns: the agent's own negative.
            died("max_turns"),
            // Finished and billed without fixing: the agent's too.
            serde_json::json!({
                "success": true,
                "turns": 20,
                "cost_usd": 0.02,
                "gate": {"bug_fixed": false},
                "micro": {"totals": {"failed": 1, "calls": 8}},
            }),
        ],
    )];
    let headline = suite_headline("m", "strict", "full", &suite);
    assert_eq!(headline["infra_failures"], serde_json::json!(1));
    assert_eq!(headline["agent_negatives"], serde_json::json!(2));
    assert_eq!(
        headline["failure_categories"],
        serde_json::json!({"model_error": 1, "max_turns": 1, "gate_failed": 1})
    );
}

#[test]
fn suite_headline_excludes_graded_infra_failures_from_the_pass_rate() {
    // A run killed by the wire or the watchdog is still graded: the
    // grader evaluates the workspace however the run ended. Both
    // directions corrupt the rate — the crash that had already written
    // the fix would be counted as a solve, the crash that had not as an
    // agent failure — and neither says anything about agent capability,
    // so both must leave the denominator. The live suite report this
    // pins had 11 such runs inside its 0.893.
    let crashed = |fixed: bool, reason: &str| {
        serde_json::json!({
            "success": false,
            "end_reason": reason,
            "turns": 1,
            "cost_usd": 0.01,
            "gate": {"bug_fixed": fixed},
        })
    };
    let completed = |fixed: bool| {
        serde_json::json!({
            "success": true,
            "turns": 20,
            "cost_usd": 0.02,
            "gate": {"bug_fixed": fixed},
        })
    };
    let suite = vec![(
        "B1".to_owned(),
        vec![
            completed(true),
            completed(true),
            completed(false),
            crashed(true, "model_error"),
            crashed(false, "model_error"),
            // Watchdog kill: ungated, and now carrying its own reason
            // instead of rendering as `unknown`.
            serde_json::json!({"success": false, "end_reason": "watchdog_timeout"}),
        ],
    )];
    let headline = suite_headline("m", "strict", "full", &suite);
    // Only the three completed runs are verdicts: 2 solved of 3.
    assert_eq!(headline["attempted_runs"], serde_json::json!(6));
    assert_eq!(headline["gated_runs"], serde_json::json!(3));
    assert_eq!(headline["gated_solved"], serde_json::json!(2));
    assert_eq!(headline["agent_negatives"], serde_json::json!(1));
    assert_eq!(headline["infra_failures"], serde_json::json!(3));
    assert_eq!(
        headline["failure_categories"],
        serde_json::json!({
            "model_error": 2,
            "watchdog_timeout": 1,
            "gate_failed": 1,
        })
    );
    // The per-task row shows what the rate withheld.
    let task = &headline["tasks"]["B1"];
    assert_eq!(task["runs"], serde_json::json!(3));
    assert_eq!(task["attempted"], serde_json::json!(6));
    assert_eq!(task["infra"], serde_json::json!(3));
}

#[test]
fn suite_headline_treats_an_unlabeled_failure_as_infrastructure() {
    // Pre-watchdog-stamp reports, and any run torn down before its reason
    // reached the report, have no `end_reason`. Those are infrastructure,
    // not agent negatives — and they must not masquerade as a solve.
    let suite = vec![(
        "B1".to_owned(),
        vec![
            serde_json::json!({"success": false, "gate": {"bug_fixed": true}}),
            serde_json::json!({"success": false}),
            serde_json::json!({"success": true, "gate": {"bug_fixed": true}}),
        ],
    )];
    let headline = suite_headline("m", "strict", "full", &suite);
    assert_eq!(headline["gated_runs"], serde_json::json!(1));
    assert_eq!(headline["gated_solved"], serde_json::json!(1));
    assert_eq!(headline["infra_failures"], serde_json::json!(2));
    // A success with no reason is the normal shape, never `unknown`.
    assert_eq!(
        headline["failure_categories"],
        serde_json::json!({"unknown": 2})
    );
}

#[test]
fn suite_headline_computes_pass_rate_and_economics() {
    let run = |solved: bool,
               cost: f64,
               turns: u64,
               hit: u64,
               miss: u64,
               failed: u64,
               calls: u64,
               tier: Option<&str>| {
        serde_json::json!({
            // A gate failure is transcript-successful: the run
            // completed, billed, and still did not fix.
            "success": true,
            "turns": turns,
            "no_tool_turns": turns / 4,
            "tier": tier,
            "input_tokens": hit + miss,
            "output_tokens": 50,
            "cost_usd": cost,
            "cache_hit_tokens": hit,
            "cache_miss_tokens": miss,
            "gate": {"bug_fixed": solved},
            "micro": {"totals": {"failed": failed, "calls": calls}},
        })
    };
    let suite = vec![
        (
            "B2".to_owned(),
            vec![
                run(true, 0.01, 10, 900, 100, 1, 10, Some("s")),
                run(true, 0.02, 14, 800, 200, 3, 10, Some("s")),
            ],
        ),
        (
            "B3".to_owned(),
            vec![run(false, 0.03, 20, 0, 500, 5, 10, Some("m"))],
        ),
        // Lab: no gate, no economics — contributes nothing but presence.
        (
            "B1".to_owned(),
            vec![serde_json::json!({"success": true, "turns": 3})],
        ),
    ];
    let headline = suite_headline("m", "strict", "full", &suite);

    assert_eq!(headline["gated_runs"], serde_json::json!(3));
    assert_eq!(headline["gated_solved"], serde_json::json!(2));
    assert_eq!(headline["pass_rate"], serde_json::json!(2.0 / 3.0));
    assert_eq!(headline["arm"], serde_json::json!("full"));
    assert_eq!(headline["tasks"]["B1"]["kind"], serde_json::json!("lab"));

    // Tier rollup (BTreeMap order, m < s): m = 0/1, s = 2/2 — the
    // blended 2/3 hides that split.
    let m = &headline["by_tier"][0];
    assert_eq!(m["tier"], serde_json::json!("m"));
    assert_eq!(m["rate"], serde_json::json!(0.0));
    let s = &headline["by_tier"][1];
    assert_eq!(s["tier"], serde_json::json!("s"));
    assert_eq!(s["rate"], serde_json::json!(1.0));

    // The one gate failure is transcript-successful, so its billed
    // tokens count toward every population it belongs to.
    assert_eq!(
        headline["failure_categories"],
        serde_json::json!({"gate_failed": 1})
    );
    // The only death here is the agent's own gate failure; nothing is
    // attributed to infrastructure.
    assert_eq!(headline["infra_failures"], serde_json::json!(0));
    assert_eq!(headline["agent_negatives"], serde_json::json!(1));

    let econ = &headline["economics"];
    assert_eq!(econ["total_cost_usd"], serde_json::json!(0.06));
    assert_eq!(econ["usd_per_solved"], serde_json::json!(0.03));
    // tokens_per_solved: every successful run's billed input
    // (1000+1000+500) + output (3×50) over 2 solved = 1325.
    assert_eq!(econ["tokens_per_solved"], serde_json::json!(1325));
    // The failed run's $0.03 is 50 % of total spend — the expensive
    // failure share.
    assert_eq!(econ["expensive_failure_ratio"], serde_json::json!(0.5));
    assert_eq!(econ["turns_per_solved_mean"], serde_json::json!(12.0));
    assert_eq!(econ["turns_per_solved_median"], serde_json::json!(12.0));
    // Idle-turn tax: 2+3+5 = 10 no-tool turns over 47 total turns
    // (the lab run's 3 count — real turns, real spend).
    assert_eq!(econ["idle_turns"], serde_json::json!(10));
    assert_eq!(econ["idle_turn_rate"], serde_json::json!(0.2128));
    // Token-weighted over successful runs: (900+800) / (1000+1000+500).
    assert_eq!(econ["cache_read_ratio"], serde_json::json!(0.68));
    assert_eq!(
        econ["tool_failure_rate"],
        serde_json::json!({"failed": 9, "total": 30, "rate": 0.3, "note": "micro-eval totals over all runs incl. failures"})
    );
}

/// A dummy task for preset tests: only `id`/`heavy`/`gate` matter to
/// selection logic.
fn dummy_task(id: &str, heavy: bool, gate: bool) -> EvalTask {
    EvalTask {
        id: id.to_owned(),
        prompt: String::new(),
        workspace: TaskWorkspace::Repo,
        tools: ToolSet::Coding,
        heavy,
        tier: None,
        system_prompt: None,
        gate: if gate {
            Some(TaskGate::Outcome(OutcomeSpec {
                exact_files: vec![],
                contains: vec![],
                tests_green: None,
            }))
        } else {
            None
        },
        notes: None,
    }
}

#[test]
fn hard_gate_preset_keeps_every_gated_task_and_no_lab() {
    // The preset is the whole point of the release gate: gated heavies
    // run, lab benches never, and nothing gated can be left out.
    let corpus = [
        dummy_task("B1", false, false),  // read-only lab bench
        dummy_task("B2", false, true),   // plain gated task
        dummy_task("B5", true, false),   // heavy lab bench
        dummy_task("LONG1", true, true), // heavy gated task
        dummy_task("EXT-x", true, true), // imported external task
    ];
    let kept: Vec<&str> = corpus
        .iter()
        .filter(|t| hard_gate_keeps(t))
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(kept, vec!["B2", "LONG1", "EXT-x"]);
}

#[test]
fn suite_headline_handles_an_empty_or_all_lab_suite() {
    let headline = suite_headline("m", "tolerant", "full", &[]);
    assert_eq!(headline["gated_runs"], serde_json::json!(0));
    assert_eq!(headline["pass_rate"], serde_json::json!(null));
    assert_eq!(
        headline["economics"]["usd_per_solved"],
        serde_json::json!(null)
    );
    assert_eq!(
        headline["economics"]["tokens_per_solved"],
        serde_json::json!(null)
    );
    assert_eq!(
        headline["economics"]["expensive_failure_ratio"],
        serde_json::json!(null)
    );
    assert_eq!(
        headline["economics"]["cache_read_ratio"],
        serde_json::json!(null)
    );
    assert_eq!(
        headline["economics"]["tool_failure_rate"]["rate"],
        serde_json::json!(null)
    );
    let lab_only = vec![(
        "B1".to_owned(),
        vec![serde_json::json!({"success": true, "turns": 3, "cost_usd": 0.01})],
    )];
    let headline = suite_headline("m", "strict", "bare", &lab_only);
    assert_eq!(headline["arm"], serde_json::json!("bare"));
    assert_eq!(headline["tasks"]["B1"]["kind"], serde_json::json!("lab"));
    assert_eq!(
        headline["economics"]["total_cost_usd"],
        serde_json::json!(0.01)
    );
}

/// The per-model comparison dimension (roadmap item 1): gated outcomes
/// group under the model id each run itself names, across tasks — so a
/// suite merged from several models' per-task reports (the union path)
/// stays attributable without any caller-side bookkeeping.
#[test]
fn suite_headline_groups_gate_outcomes_by_model() {
    let run = |model: &str, solved: bool| {
        serde_json::json!({
            "model": model,
            "success": true,
            "turns": 5,
            "cost_usd": 0.01,
            "gate": {"bug_fixed": solved},
        })
    };
    let suite = vec![
        (
            "B2".to_owned(),
            vec![run("alpha", true), run("alpha", false), run("beta", true)],
        ),
        (
            "B3".to_owned(),
            vec![run("alpha", false), run("beta", true)],
        ),
        // Lab runs name their model too but carry no gate, and the
        // section compares gate outcomes — gamma must not appear.
        (
            "B1".to_owned(),
            vec![serde_json::json!({"model": "gamma", "success": true, "turns": 3})],
        ),
    ];
    let headline = suite_headline("alpha", "strict", "full", &suite);
    let models = headline["models"].as_object().expect("models section");
    assert_eq!(models.len(), 2, "alpha and beta only: {models:?}");
    assert_eq!(
        models["alpha"],
        serde_json::json!({"runs": 3, "solved": 1, "rate": 1.0 / 3.0})
    );
    assert_eq!(
        models["beta"],
        serde_json::json!({"runs": 2, "solved": 2, "rate": 1.0})
    );
    assert!(!models.contains_key("gamma"));
    // The blended headline is unchanged by the new section.
    assert_eq!(headline["gated_runs"], serde_json::json!(5));
    assert_eq!(headline["gated_solved"], serde_json::json!(3));
}

/// Old reports (runs that predate the per-run `model` field) still
/// aggregate: the models section is absent — not null — so renderers
/// treat it as optional and old artifacts render unchanged.
#[test]
fn suite_headline_omits_models_section_for_old_reports() {
    let suite = vec![(
        "B2".to_owned(),
        vec![serde_json::json!({
            "success": true,
            "turns": 4,
            "gate": {"bug_fixed": true},
        })],
    )];
    let headline = suite_headline("m", "strict", "full", &suite);
    assert_eq!(headline["gated_runs"], serde_json::json!(1));
    assert_eq!(headline["gated_solved"], serde_json::json!(1));
    assert!(
        headline.get("models").is_none(),
        "no run carries a model id — the section must be absent, not null"
    );
}

/// `scripts/bench-report.py` renders the per-model comparison table
/// when the suite carries a `models` section, and an old-format suite
/// without one still renders (backward-compatible). Offline: synthetic
/// JSON under the scratch target dir, one stdlib-only script, no
/// network. Self-skips when no python3 is on PATH, the same contract
/// as the replay tests skipping without recordings.
#[test]
fn bench_report_renders_per_model_comparison() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping bench_report_renders_per_model_comparison: no python3 on PATH");
        return;
    }
    let script = workspace_root().join("scripts").join("bench-report.py");
    let scratch = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target"))
        .join("agents")
        .join("permodel")
        .join("scratch")
        .join("bench-report");
    std::fs::create_dir_all(&scratch).unwrap();
    let mut with_models = serde_json::json!({
        "model": "alpha",
        "mode": "strict",
        "arm": "full",
        "gated_runs": 3,
        "gated_solved": 1,
        "pass_rate": 1.0 / 3.0,
        "tasks": {"B2": {"kind": "field", "runs": 3, "solved": 1, "rate": 1.0 / 3.0}},
    });
    with_models["models"] = serde_json::json!({
        "alpha": {"runs": 3, "solved": 1, "rate": 1.0 / 3.0},
        "beta": {"runs": 2, "solved": 2, "rate": 1.0},
    });
    let without_models = serde_json::json!({
        "model": "alpha",
        "mode": "strict",
        "arm": "full",
        "gated_runs": 3,
        "gated_solved": 1,
        "pass_rate": 1.0 / 3.0,
        "tasks": {"B2": {"kind": "field", "runs": 3, "solved": 1, "rate": 1.0 / 3.0}},
    });
    let with_path = scratch.join("suite-with-models.json");
    let without_path = scratch.join("suite-without-models.json");
    std::fs::write(
        &with_path,
        serde_json::to_string_pretty(&with_models).unwrap(),
    )
    .unwrap();
    std::fs::write(
        &without_path,
        serde_json::to_string_pretty(&without_models).unwrap(),
    )
    .unwrap();
    let render = |path: &Path| -> String {
        let output = std::process::Command::new("python3")
            .arg(&script)
            .arg(path)
            .output()
            .expect("python3 (probed above)");
        assert!(
            output.status.success(),
            "bench-report.py failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let rendered = render(&with_path);
    assert!(
        rendered.contains("| model | solved/runs | rate |"),
        "per-model table header missing:\n{rendered}"
    );
    assert!(
        rendered.contains("| `alpha` | 1/3 | 33.3% |"),
        "alpha row missing:\n{rendered}"
    );
    assert!(
        rendered.contains("| `beta` | 2/2 | 100.0% |"),
        "beta row missing:\n{rendered}"
    );
    let old = render(&without_path);
    assert!(
        !old.contains("| model |"),
        "old-format suite must not grow a models table:\n{old}"
    );
    assert!(
        old.contains("pass rate: 1/3"),
        "old-format suite still renders its headline:\n{old}"
    );
}

/// mean/median/min/max over the per-run reports (successful runs only for
/// metric columns; failures would poison token aggregates).
fn aggregates_from(reports: &[serde_json::Value]) -> serde_json::Value {
    let pick = |key: &str| -> Vec<u64> {
        reports
            .iter()
            .filter(|r| r["success"].as_bool().unwrap_or(false))
            .filter_map(|r| r[key].as_u64())
            .collect()
    };
    let pick_f64 = |key: &str| -> Vec<f64> {
        reports
            .iter()
            .filter(|r| r["success"].as_bool().unwrap_or(false))
            .filter_map(|r| r[key].as_f64())
            .collect()
    };
    let mut aggregates = serde_json::json!({
        "turns": aggregate(&pick("turns")),
        "input_tokens": aggregate(&pick("input_tokens")),
        "cache_hit_tokens": aggregate(&pick("cache_hit_tokens")),
        "cache_miss_tokens": aggregate(&pick("cache_miss_tokens")),
        "hit_percent": aggregate_f64(&pick_f64("hit_percent")),
        "output_tokens": aggregate(&pick("output_tokens")),
        "wall_time_ms": aggregate(&pick("wall_time_ms")),
        "cost_usd": aggregate_f64(&pick_f64("cost_usd")),
    });
    // Pass rate for gated (real-bug) tasks: solved runs over all runs,
    // transcript failures included — a failed run is a failed fix.
    let gated: Vec<&serde_json::Value> = reports
        .iter()
        .filter(|r| r.get("gate").is_some_and(|g| !g.is_null()))
        .collect();
    if !gated.is_empty() {
        let solved = gated
            .iter()
            .filter(|r| r["gate"]["bug_fixed"].as_bool().unwrap_or(false))
            .count();
        aggregates.as_object_mut().unwrap().insert(
            "bug_fixed_rate".to_owned(),
            serde_json::json!({
                "solved": solved,
                "runs": gated.len(),
                "rate": solved as f64 / gated.len() as f64,
            }),
        );
    }
    // Micro-eval rollups: per-task call quality from the transcript
    // blocks every run now carries (successes and failures alike).
    let calls: u64 = reports
        .iter()
        .filter_map(|r| r["micro"]["totals"]["calls"].as_u64())
        .sum();
    if calls > 0 {
        let failed: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["totals"]["failed"].as_u64())
            .sum();
        let malformed: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["malformed_calls"].as_u64())
            .sum();
        let attempts: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["edit_apply"]["attempts"].as_u64())
            .sum();
        let applied: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["edit_apply"]["applied"].as_u64())
            .sum();
        let repeats: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["identical_repeats"].as_u64())
            .sum();
        let spills: u64 = reports
            .iter()
            .filter_map(|r| r["micro"]["spills"].as_array().map(|s| s.len() as u64))
            .sum();
        let rate = |n: u64, d: u64| (n as f64 / d as f64 * 1e4).round() / 1e4;
        aggregates.as_object_mut().unwrap().insert(
            "micro".to_owned(),
            serde_json::json!({
                "calls": calls,
                "tool_error_rate": rate(failed, calls),
                "malformed_rate": rate(malformed, calls),
                "edit_apply_rate": if attempts == 0 {
                    serde_json::json!(null)
                } else {
                    serde_json::json!(rate(applied, attempts))
                },
                "identical_repeats": repeats,
                "spills": spills,
            }),
        );
    }
    aggregates
}

// ------------------------------------------------- replay equivalence ----

/// The bugfix seeds must fail their own gates as seeded — the bug is
/// present, the FAIL_TO_PASS tests fail, and the PASS_TO_PASS tests
/// pass (SWE-bench validates the pre-state the same way). B6 is
/// additionally proven fixable in place: applying the one-line real fix
/// flips every declared test to passing.
#[test]
#[ignore = "clones and cargo-tests every task workspace; run with --ignored"]
fn bugfix_seeds_fail_their_gates() {
    for task in tasks() {
        let Some(TaskGate::Bugfix(spec)) = task.gate.as_ref() else {
            continue;
        };
        let TaskWorkspace::Copy(src) = &task.workspace else {
            panic!("{}: bugfix tasks must use a copy workspace", task.id);
        };
        let root = run_root(&task.id);
        ensure_copy(src, &root);
        let (code, output) = run_fixture_tests(&root).expect("cargo test");
        assert_ne!(
            code,
            Some(0),
            "{}: seeded fixture must fail `cargo test`",
            task.id
        );
        let outcomes = test_outcomes(&output);
        let passed = |name: &str| outcomes.iter().any(|(n, ok)| n == name && *ok);
        let failed = |name: &str| outcomes.iter().any(|(n, ok)| n == name && !*ok);
        for name in &spec.fail_to_pass {
            assert!(
                failed(name),
                "{}: FAIL_TO_PASS `{name}` must fail on the seed (outcomes: {outcomes:?})",
                task.id
            );
        }
        for name in &spec.pass_to_pass {
            assert!(
                passed(name),
                "{}: PASS_TO_PASS `{name}` must pass on the seed",
                task.id
            );
        }
    }

    // The B6 pre-state is one line wrong; the real fix must flip every
    // declared test. This also exercises `test_outcomes` against real
    // libtest output.
    let b6 = tasks().into_iter().find(|t| t.id == "B6").unwrap();
    let TaskWorkspace::Copy(src) = &b6.workspace else {
        panic!("B6 must use a copy workspace");
    };
    let root = run_root("B6");
    ensure_copy(src, &root);
    let lib = root.join("src/lib.rs");
    let body = std::fs::read_to_string(&lib).unwrap();
    let fixed = body.replacen(
        "parallel_tool_calls: self.supports(\"tools\"),",
        "parallel_tool_calls: self.supports(\"parallel_tool_calls\"),",
        1,
    );
    std::fs::write(&lib, fixed).unwrap();
    let (code, output) = run_fixture_tests(&root).expect("cargo test");
    assert_eq!(code, Some(0), "the real B6 fix must turn the gate green");
    let outcomes = test_outcomes(&output);
    let Some(TaskGate::Bugfix(spec)) = b6.gate.as_ref() else {
        panic!("B6 must be a bugfix task");
    };
    for name in spec.fail_to_pass.iter().chain(&spec.pass_to_pass) {
        assert!(
            outcomes.iter().any(|(n, ok)| n == name && *ok),
            "the fixed suite must report `{name}` passing"
        );
    }
}

/// The outcome gates must reject an untouched workspace: a fresh copy
/// satisfies none of B2/B3/B4 (no greeting, no `sub`, no summaries), so
/// a run that only claims success cannot pass. B3's tests-green check
/// is additionally proven satisfiable — the seeded suite passes at the
/// declared count, so the check discriminates on the code change, not
/// on a broken baseline.
#[tokio::test]
async fn outcome_gates_reject_an_untouched_workspace() {
    let mut saw_b3 = false;
    for task in tasks() {
        let Some(gate @ TaskGate::Outcome(spec)) = task.gate.as_ref() else {
            continue;
        };
        let TaskWorkspace::Copy(src) = &task.workspace else {
            panic!("{}: outcome tasks must use a copy workspace", task.id);
        };
        let root = run_root(&task.id);
        ensure_copy(src, &root);
        let graded = grade_gate(Some(gate), &root, None, true)
            .await
            .expect("outcome tasks are gated");
        assert_eq!(
            graded["bug_fixed"], false,
            "{}: an untouched workspace must fail its gate: {graded}",
            task.id
        );
        if task.id == "B3" {
            saw_b3 = true;
            let (code, output) = run_fixture_tests(&root).expect("cargo test");
            assert_eq!(code, Some(0), "the seeded B3 suite must be green");
            assert_eq!(
                count_tests_passed(&output),
                spec.tests_green.unwrap_or(0),
                "the seeded B3 suite must pass at the declared count"
            );
        }
    }
    assert!(saw_b3, "the B3 task must exist");
}

/// The positive counterpart to the rejection test: each fast-path
/// outcome task's canonical solve must actually pass its gate. FPATH3's
/// solve is `cargo fmt` (the format fast path's exact action); FPATH4's
/// is the one-line manifest bump (the dependency fast path's exact
/// action). This keeps an exact-file gate from drifting out of reach —
/// a hand-written expectation the real formatter no longer produces —
/// and a tests-green gate from demanding a count a test-less suite can
/// never report.
#[tokio::test]
async fn outcome_gate_solves_are_satisfiable() {
    let mut seen = 0;
    for task in tasks() {
        if !matches!(task.id.as_str(), "FPATH3" | "FPATH4") {
            continue;
        }
        let Some(gate) = task.gate.as_ref() else {
            panic!("{}: must stay gated", task.id);
        };
        let TaskWorkspace::Copy(src) = &task.workspace else {
            panic!("{}: must use a copy workspace", task.id);
        };
        let root = run_root(&task.id);
        ensure_copy(src, &root);
        match task.id.as_str() {
            // The format fast path's exact action.
            "FPATH3" => {
                let status = std::process::Command::new("cargo")
                    .arg("fmt")
                    .current_dir(&root)
                    .status()
                    .expect("cargo fmt");
                assert!(status.success(), "FPATH3: cargo fmt must succeed");
            }
            // The dependency fast path's exact action: the one-line bump.
            "FPATH4" => {
                let manifest = root.join("Cargo.toml");
                let body = std::fs::read_to_string(&manifest).expect("read Cargo.toml");
                let bumped = body.replace("bitflags = \"1.0.0\"", "bitflags = \"2.0.0\"");
                assert_ne!(bumped, body, "FPATH4: seed must carry bitflags 1.0.0");
                std::fs::write(&manifest, bumped).expect("write Cargo.toml");
            }
            _ => unreachable!(),
        }
        let graded = grade_gate(Some(gate), &root, None, true)
            .await
            .expect("gated");
        assert_eq!(
            graded["bug_fixed"], true,
            "{}: the canonical solve must pass its gate: {graded}",
            task.id
        );
        seen += 1;
    }
    assert_eq!(seen, 2, "FPATH3 and FPATH4 must both exist");
}

/// The invariant under test (V3, Invariant 10): replaying recorded
/// responses through the real engine + real tools reproduces the recorded
/// assistant turns verbatim in the transcript.
async fn assert_replay_equivalence(task: &EvalTask, model: &str, exchanges: &[Exchange]) {
    let replay = ReplayProvider::new(model, exchanges);
    let overruns = replay.overruns_handle();
    let tools = task.tools;
    let open = |root: PathBuf, system_prompt: Option<String>| async move {
        let overrides = EngineOverrides::default()
            .with_provider(Box::new(replay))
            .with_registry(registry_for(tools))
            .with_compiler(eval_compiler("replay"));
        Engine::open_with(eval_config(root, "replay", model, system_prompt), overrides)
    };
    let (_report, _recorded, session) = run_once(task, model, "replay", None, open).await;
    // No success assert, by design: recordings capture failed runs too
    // (max-turns grind, unfixed bugs), and replaying a failure to the
    // same failure IS fidelity. The invariants that catch real drift are
    // below — the replayed assistant transcript must reproduce the
    // recording verbatim, the turn count must match, and the overrun
    // counter must stay within the one completion-gate beat. A replay
    // that ends for a different reason than the recording ran out of
    // script and tripped one of those.

    let replayed: Vec<&AssistantMessage> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(message) => Some(message),
            _ => None,
        })
        .collect();
    // One beat past the recording is the completion gate's refusal on
    // old recordings; the gate lands in new recordings natively.
    let expected_turns = exchanges.len() + overruns.load(Ordering::SeqCst);
    assert_eq!(
        replayed.len(),
        expected_turns,
        "{}: replay produced {} assistant turns, recording has {} (+{} gate beats)",
        task.id,
        replayed.len(),
        exchanges.len(),
        overruns.load(Ordering::SeqCst)
    );
    for (i, (got, recorded)) in replayed.iter().zip(exchanges).enumerate() {
        assert_eq!(
            got, &&recorded.response.message,
            "{}: assistant turn {i} diverged from the recording",
            task.id
        );
    }
}

/// Runs one offline replay with request capture, so tests can inspect what
/// the compiler produced on a deterministic path (no network, no clock).
async fn replay_capturing(task: &EvalTask, model: &str, exchanges: &[Exchange]) -> Vec<Exchange> {
    let replay = ReplayProvider::new(model, exchanges);
    let (provider, captured) = RecordingProvider::new(Box::new(replay));
    let tools = task.tools;
    let open = |root: PathBuf, system_prompt: Option<String>| async move {
        let overrides = EngineOverrides::default()
            .with_provider(Box::new(provider))
            .with_registry(registry_for(tools))
            .with_compiler(eval_compiler("replay"));
        Engine::open_with(eval_config(root, "replay", model, system_prompt), overrides)
    };
    let (_report, recorded, _session) = run_once(task, model, "replay", Some(captured), open).await;
    recorded
}

/// Replays every recording on disk. Self-skips when no recordings exist
/// (fresh checkout); `eval_live` (ignored test) creates them. Each
/// recording replays under the model it was recorded with — the stem
/// carries it — so mixed-era directories (provider pivots, free-tier
/// outages) stay replayable without any environment default.
#[tokio::test]
async fn eval_replay_recordings() {
    let records_dir = eval_dir().join("records");
    let Ok(entries) = std::fs::read_dir(&records_dir) else {
        eprintln!(
            "no eval recordings at {} — run eval_live first",
            records_dir.display()
        );
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!(
            "no eval recordings in {} — run eval_live first",
            records_dir.display()
        );
        return;
    }
    let all_tasks = tasks();
    for file in files {
        let Some((task, model)) = task_and_model_for(&file, &all_tasks) else {
            eprintln!(
                "{}: no pinned task matches the stem — skipped",
                file.display()
            );
            continue;
        };
        let exchanges = read_exchanges(&file);
        if exchanges.is_empty() {
            // The 429 era wrote one-byte shells: the request never
            // completed, so nothing is replayable by construction.
            eprintln!(
                "{}: empty recording (a failed run) — skipped",
                file.display()
            );
            continue;
        }
        eprintln!(
            "replaying {} [{}] ({} exchanges)",
            file.display(),
            model,
            exchanges.len()
        );
        assert_replay_equivalence(task, &model, &exchanges).await;
    }
}

/// Splits a recording stem `<task>-<model>-<run>` into its task and
/// model. Task ids may themselves contain dashes (`EXT-…`), so the task
/// is the longest known id prefixing the stem and the model is the
/// remainder — recordings from different providers and eras coexist in
/// one directory, each replaying under the model it was recorded with.
fn task_and_model_for<'a>(path: &Path, tasks: &'a [EvalTask]) -> Option<(&'a EvalTask, String)> {
    let stem = path.file_stem()?.to_str()?;
    // Drop the trailing `-<run>` counter; anything non-numeric stays
    // (defensive against future naming).
    let stem = stem
        .rsplit_once('-')
        .filter(|(_, run)| !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()))
        .map_or(stem, |(head, _)| head);
    let task = tasks
        .iter()
        .filter(|t| {
            stem.strip_prefix(&t.id)
                .is_some_and(|rest| rest.starts_with('-'))
        })
        .max_by_key(|t| t.id.len())?;
    let model = stem
        .strip_prefix(&task.id)
        .and_then(|rest| rest.strip_prefix('-'))?
        .to_owned();
    Some((task, model))
}

/// Offline micro-eval: recompute call quality from the session snapshots
/// `eval_live` persists under `records/sessions`, and cross-check each
/// against the `micro` block the live run embedded in its report — the
/// same function over the same transcript must agree exactly. Self-skips
/// when no snapshots exist (fresh checkout); with a corpus present this
/// is the token-free half of the release-gate numbers, and `loom stats`
/// computes the same shape from the store.
#[test]
fn eval_micro_walks_recorded_sessions() {
    let sessions_dir = eval_dir().join("records").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        eprintln!(
            "no eval sessions at {} — run eval_live first",
            sessions_dir.display()
        );
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".session.json"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!(
            "no eval sessions in {} — run eval_live first",
            sessions_dir.display()
        );
        return;
    }
    let analytics = loom_core::kernel::analytics::session_stats;
    // model -> (per-session entries, summed counters)
    let mut by_model: std::collections::BTreeMap<
        String,
        (Vec<serde_json::Value>, u64, u64, u64, u64),
    > = std::collections::BTreeMap::new();
    for file in &files {
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parts: Vec<&str> = stem.split('-').collect();
        // {task}-{model...}-{run}.session
        let (Some(task), Ok(run), Some(model)) = (
            parts.first().copied(),
            parts.last().copied().unwrap_or("").parse::<usize>(),
            parts
                .get(1..parts.len().saturating_sub(1))
                .map(|m| m.join("-")),
        ) else {
            eprintln!("{}: unparsable session name — skipped", file.display());
            continue;
        };
        let text = std::fs::read_to_string(file).unwrap();
        let session: Session = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: session snapshot does not parse: {e}", file.display()));
        let stats = analytics(&session);
        let recomputed = serde_json::to_value(&stats).unwrap();

        // Cross-check against the live run's embedded micro block: same
        // function, same transcript — exact equality or something is
        // lying about what ran.
        let report_path = eval_dir().join(format!("report-{task}-{model}.json"));
        if let Ok(report_text) = std::fs::read_to_string(&report_path)
            && let Ok(report) = serde_json::from_str::<serde_json::Value>(&report_text)
            && let Some(live_micro) = report["runs"]
                .as_array()
                .and_then(|runs| runs.get(run - 1))
                .map(|r| &r["micro"])
        {
            assert_eq!(
                live_micro,
                &recomputed,
                "{} run {run}: live micro block disagrees with the recomputed stats",
                file.display()
            );
        }

        let entry = serde_json::json!({
            "session": stem,
            "turns": stats.turns,
            "calls": stats.totals.calls,
            "failed": stats.totals.failed,
            "malformed_calls": stats.malformed_calls,
            "identical_repeats": stats.identical_repeats,
            "edit_apply": stats.edit_apply,
            "spills": stats.spills.len(),
        });
        let slot = by_model
            .entry(model)
            .or_insert_with(|| (Vec::new(), 0, 0, 0, 0));
        slot.0.push(entry);
        slot.1 += stats.turns;
        slot.2 += stats.totals.calls;
        slot.3 += stats.totals.failed;
        slot.4 += stats.malformed_calls;
    }
    for (model, (entries, turns, calls, failed, malformed)) in &by_model {
        let report = serde_json::json!({
            "harness": "eval 2A micro",
            "model": model,
            "sessions": entries,
            "totals": {
                "sessions": entries.len(),
                "turns": turns,
                "calls": calls,
                "failed": failed,
                "malformed_calls": malformed,
            },
            "note": "recomputed offline from records/sessions; equals each live run's micro block",
        });
        let path = eval_dir().join(format!("report-micro-{model}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!(
            "micro: {} sessions, {calls} calls, {failed} failed — wrote {}",
            entries.len(),
            path.display()
        );
    }
    // The per-model comparison dimension (roadmap item 1), micro half:
    // one artifact with every model side by side — the same totals the
    // per-model files carry, keyed by model id — so cross-model deltas
    // read without diffing files. Absent model data never reaches here
    // (the walker self-skips without session snapshots).
    let comparison = serde_json::json!({
        "harness": "eval 2A micro",
        "models": by_model
            .iter()
            .map(|(model, (entries, turns, calls, failed, malformed))| {
                (
                    model.clone(),
                    serde_json::json!({
                        "sessions": entries.len(),
                        "turns": turns,
                        "calls": calls,
                        "failed": failed,
                        "malformed_calls": malformed,
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>(),
        "note": "per-model totals recomputed offline from records/sessions; per-model detail in report-micro-<model>.json",
    });
    let path = eval_dir().join("report-micro.json");
    std::fs::write(&path, serde_json::to_string_pretty(&comparison).unwrap()).unwrap();
    eprintln!(
        "micro comparison across {} models — wrote {}",
        by_model.len(),
        path.display()
    );
}

/// Synthetic two-turn script (no network, no recordings): a `read_file`
/// tool-call turn, then a final answer. Proves the replay path itself —
/// provider, loop, real tool execution, transcript equality — stays green
/// in every plain `cargo test` run.
#[tokio::test]
async fn eval_replay_synthetic() {
    let model = "deepseek-flash";
    let tool_turn = AssistantMessage {
        content: String::new(),
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: ToolCallId::try_from(String::from("synthetic-1")).unwrap(),
            name: String::from("read_file"),
            arguments: ToolArguments::try_from(serde_json::json!({
                "path": "README.md"
            }))
            .unwrap(),
        }],
        stop_reason: StopReason::ToolCalls,
    };
    let final_turn = AssistantMessage {
        content: String::from("README read successfully. Done."),
        reasoning: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
    };
    let exchanges: Vec<Exchange> = [tool_turn, final_turn]
        .into_iter()
        .map(|message| Exchange {
            request: ModelRequest {
                model: model.to_owned(),
                messages: Vec::new(),
                tools: Vec::new(),
                max_tokens: None,
                temperature: None,
                stable_prefix_messages: None,
            },
            response: ModelResponse {
                message,
                usage: Usage::default(),
            },
        })
        .collect();

    let task = tasks().into_iter().find(|t| t.id == "B1").unwrap();
    assert_replay_equivalence(&task, model, &exchanges).await;

    // Determinism of the measurement itself: the same scripted run must
    // compile byte-identical requests every time. This pins the property the
    // byte census relies on — request bytes are a pure function of
    // (task, tools, transcript), with no clock, provider, or map-ordering
    // nondeterminism (serde_json's sorted-key maps are part of the contract;
    // enabling `preserve_order` would fail here first). Live-recorded
    // requests are deliberately NOT compared against these: B1 reads this
    // repository, so repo drift legitimately changes tool-result bytes
    // between record and replay — assistant-turn equality above already
    // covers that path.
    let first = replay_capturing(&task, model, &exchanges).await;
    let second = replay_capturing(&task, model, &exchanges).await;
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "replayed runs must compile byte-identical requests"
    );
    assert!(
        first.iter().all(|e| !e.request.messages.is_empty()),
        "capture must include the compiler-produced requests"
    );

    // The scripted tool call must have executed against the real workspace.
    // (assert_replay_equivalence already proved turn equality; this checks
    // the tool result landed next to it.)
}

/// Replay's capabilities are derived from the recording itself, and the
/// provider says so: `capability_source() == "recording"` is what the
/// engine's `CapabilitiesResolved` event surfaces, keeping the trace
/// honest about which authority fixed the window (v8).
#[test]
fn replay_capabilities_derive_from_the_recording() {
    let model = "deepseek-flash";
    let request = ModelRequest {
        model: model.to_owned(),
        messages: Vec::new(),
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
    };
    let exchange = |message: AssistantMessage| Exchange {
        request: request.clone(),
        response: ModelResponse {
            message,
            usage: Usage::default(),
        },
    };
    let exchanges = vec![
        // One turn proving tools, parallel calls, and reasoning.
        exchange(AssistantMessage {
            content: String::new(),
            reasoning: Some(ReasoningTrace {
                text: String::from("thinking"),
                provider_data: None,
            }),
            tool_calls: vec![
                ToolCall {
                    id: ToolCallId::try_from(String::from("synthetic-1")).unwrap(),
                    name: String::from("read_file"),
                    arguments: ToolArguments::try_from(serde_json::json!({
                        "path": "README.md"
                    }))
                    .unwrap(),
                },
                ToolCall {
                    id: ToolCallId::try_from(String::from("synthetic-2")).unwrap(),
                    name: String::from("list_dir"),
                    arguments: ToolArguments::try_from(serde_json::json!({
                        "path": "src"
                    }))
                    .unwrap(),
                },
            ],
            stop_reason: StopReason::ToolCalls,
        }),
        // One plain turn: capabilities are the union of the recording.
        exchange(AssistantMessage {
            content: String::from("done"),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        }),
    ];

    let provider = ReplayProvider::new(model, &exchanges);
    assert_eq!(provider.capability_source(), "recording");
    let capabilities = provider.capabilities(model).expect("model matches");
    assert!(
        capabilities.tool_calls,
        "a recorded tool call proves support"
    );
    assert!(
        capabilities.parallel_tool_calls,
        "a recorded multi-call turn proves parallel support"
    );
    assert!(
        capabilities.reasoning,
        "a recorded reasoning trace proves support"
    );
    // Tiny requests: the 256K floor, not the request-derived bound, wins.
    assert_eq!(capabilities.max_context_tokens, 262_144);
    assert!(
        provider.capabilities("some-other-model").is_none(),
        "capabilities are per-model"
    );
}

#[test]
fn run_roots_are_pruned_by_default_and_kept_on_request() {
    assert!(!run_roots_kept(None));
    assert!(!run_roots_kept(Some("")));
    assert!(!run_roots_kept(Some("  ")));
    assert!(!run_roots_kept(Some("0")));
    assert!(!run_roots_kept(Some("false")));
    assert!(!run_roots_kept(Some("OFF")));
    assert!(run_roots_kept(Some("1")));
    assert!(run_roots_kept(Some("true")));

    let base = std::env::temp_dir().join(format!("loom-eval-prune-{}", std::process::id()));
    let kept = base.join("kept");
    let pruned = base.join("pruned");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::create_dir_all(&pruned).unwrap();
    std::fs::write(kept.join("target-artifact"), "build tree").unwrap();
    std::fs::write(pruned.join("target-artifact"), "build tree").unwrap();

    prune_run_root_when(&kept, true);
    prune_run_root_when(&pruned, false);
    // A root from a finished or crashed run that is already gone is fine.
    prune_run_root_when(&base.join("missing"), false);

    assert!(
        kept.join("target-artifact").exists(),
        "a kept run root survives"
    );
    assert!(!pruned.exists(), "the default is to delete the run root");

    std::fs::remove_dir_all(&base).unwrap();
}

/// Builds the report map the retry pass reads: one entry per task, each a
/// run vector in scheduler order.
fn retry_map(
    runs: &[(&str, Vec<serde_json::Value>)],
) -> Arc<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>> {
    Arc::new(Mutex::new(
        runs.iter()
            .map(|(id, reports)| ((*id).to_owned(), reports.clone()))
            .collect(),
    ))
}

/// A run lost on the wire, graded anyway (the shape that made 7 NIM
/// failures count as solves before the pass-rate fix).
fn wire_death() -> serde_json::Value {
    serde_json::json!({
        "task": "B2", "model": "z-ai/glm-5.3", "provider": "nvidia",
        "success": false, "tier": "s", "end_reason": "model_error",
        "error": "request failed", "gate": {"bug_fixed": true},
    })
}

/// The agent's own negative: the run completed and the work was wrong.
/// Re-rolling this until it passed would grade the harness's luck, not the
/// agent's capability.
fn agent_negative() -> serde_json::Value {
    serde_json::json!({
        "task": "B2", "model": "z-ai/glm-5.3", "provider": "openrouter",
        "success": false, "end_reason": "max_turns",
        "error": "turn budget exhausted", "gate": {"bug_fixed": false},
    })
}

#[test]
fn the_retry_pass_re_measures_only_infrastructure_losses() {
    let task = dummy_task("B2", false, true);
    let other = dummy_task("B3", false, true);
    let map = retry_map(&[
        (
            "B2",
            vec![
                serde_json::json!({"task": "B2", "success": true}),
                wire_death(),
                agent_negative(),
                serde_json::json!({
                    "task": "B2", "success": false,
                    "end_reason": "watchdog_timeout", "error": "run watchdog timeout",
                }),
            ],
        ),
        (
            "B3",
            vec![serde_json::json!({"task": "B3", "success": true})],
        ),
    ]);

    let slots = infra_retry_slots(&[&task, &other], &map);
    let selected: Vec<_> = slots
        .iter()
        .map(|(id, index, reason)| (id.as_str(), *index, reason.as_str()))
        .collect();

    // Exactly the two infrastructure losses on B2. The max_turns run is the
    // agent's answer, not a broken wire, and a success is never retried.
    assert_eq!(
        selected,
        vec![("B2", 1, "model_error"), ("B2", 3, "watchdog_timeout")],
        "only infrastructure is re-measured"
    );
    // The slot index is the position in the run vector; the caller turns it
    // back into a 1-based run number, so run 2 must be re-run as run 2 and
    // not as "the second infra failure".
    assert_eq!(
        slots
            .iter()
            .map(|(_, index, _)| index + 1)
            .collect::<Vec<_>>(),
        vec![2, 4],
        "the retry reuses the original run number, which names its artifact"
    );
}

#[test]
fn the_retry_pass_leaves_a_fully_measured_sweep_alone() {
    let task = dummy_task("B2", false, true);
    let map = retry_map(&[(
        "B2",
        vec![
            serde_json::json!({"task": "B2", "success": true, "gate": {"bug_fixed": true}}),
            serde_json::json!({"task": "B2", "success": true, "gate": {"bug_fixed": false}}),
        ],
    )]);

    assert!(
        infra_retry_slots(&[&task], &map).is_empty(),
        "a clean sweep spends nothing on retries; a gate failure is a measurement"
    );
}

#[test]
fn infra_retries_default_to_one_and_respect_the_knob() {
    // The env is process-global and other tests read it, so this asserts the
    // parse contract rather than mutating shared state.
    assert_eq!(infra_retries_from(None), 1, "one re-measurement by default");
    assert_eq!(infra_retries_from(Some("0")), 0, "0 disables the pass");
    assert_eq!(infra_retries_from(Some("3")), 3);
    assert_eq!(
        infra_retries_from(Some("not-a-number")),
        1,
        "an unparseable value falls back to the default rather than skipping retries"
    );
}
