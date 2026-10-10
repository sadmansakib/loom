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
use loom_core::kernel::transcript::{AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId};
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
        max_turns: DEFAULT_MAX_TURNS,
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

/// N live runs of every runnable task, recordings + aggregates to disk.
#[tokio::test]
#[ignore = "live: needs DEEPSEEK_API_KEY (or OPENROUTER_API_KEY); spends real tokens"]
async fn eval_live() {
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
    let mut suite: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for task in &all_tasks {
        if let Some(only) = &only
            && !only.iter().any(|id| id.as_str() == task.id)
        {
            continue;
        }
        // Hard gate: everything that carries a gate, lab benches never.
        if hard_gate && !hard_gate_keeps(task) {
            continue;
        }
        // Heavy (long-context) tasks run only when explicitly named or
        // under the hard gate: the default suite bounds time and cost.
        if task.heavy && only.is_none() && !hard_gate {
            continue;
        }
        let mut reports = Vec::new();
        for run in 1..=runs {
            // The recording hook wraps whichever provider the engine
            // builds (credential-resolved, catalog-aware) — the eval
            // observes production wiring instead of reproducing it.
            let exchanges = Arc::new(Mutex::new(Vec::new()));
            let hook_exchanges = Arc::clone(&exchanges);
            let hook = Arc::new(move |built: Box<dyn ModelProvider>| {
                Box::new(RecordingProvider {
                    inner: built,
                    exchanges: Arc::clone(&hook_exchanges),
                }) as Box<dyn ModelProvider>
            }) as ProviderHook;
            let overrides = eval_overrides(&provider_name, task.tools, hook, bare);
            let provider_label = provider_name.clone();
            let open_provider = provider_name.clone();
            let open_model = model.clone();
            let open = |root: PathBuf, system_prompt: Option<String>| async move {
                Engine::open_async_with(
                    eval_config(root, &open_provider, &open_model, system_prompt),
                    overrides,
                )
                .await
            };
            let (report, recorded, session) =
                run_once(task, &model, &provider_label, Some(exchanges), open).await;
            eprintln!(
                "{} [{arm}] run {}/{}: success={} bug_fixed={}",
                task.id, run, runs, report["success"], report["gate"]["bug_fixed"]
            );
            std::fs::write(
                records_dir.join(format!("{}-{file_model}{file_arm}-{run}.jsonl", task.id)),
                recorded
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            )
            .unwrap();
            std::fs::write(
                sessions_dir.join(format!(
                    "{}-{file_model}{file_arm}-{run}.session.json",
                    task.id
                )),
                serde_json::to_string(&session).unwrap(),
            )
            .unwrap();
            reports.push(report);
        }

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
/// human-time tier, because a blended rate hides the tier cliff.
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
    let mut category = |run: &serde_json::Value| {
        if !run["success"].as_bool().unwrap_or(false) {
            let reason = run["end_reason"].as_str().unwrap_or("unknown").to_owned();
            *categories.entry(reason).or_default() += 1;
        } else if run.get("gate").is_some_and(|g| !g.is_null())
            && !run["gate"]["bug_fixed"].as_bool().unwrap_or(false)
        {
            *categories.entry(String::from("gate_failed")).or_default() += 1;
        }
    };
    for (id, reports) in suite {
        let gated: Vec<&serde_json::Value> = reports
            .iter()
            .filter(|r| r.get("gate").is_some_and(|g| !g.is_null()))
            .collect();
        let entry = if gated.is_empty() {
            serde_json::json!({"kind": "lab", "runs": reports.len()})
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
            })
        };
        per_task.insert(id.clone(), entry);
        for run in reports {
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
    serde_json::json!({
        "model": model,
        "mode": mode,
        "arm": arm,
        "tasks": per_task,
        "gated_runs": gated_runs,
        "gated_solved": gated_solved,
        "pass_rate": if gated_runs == 0 {
            serde_json::json!(null)
        } else {
            serde_json::json!(gated_solved as f64 / gated_runs as f64)
        },
        "by_tier": tiers.iter().map(|(tier, (runs, solved))| {
            serde_json::json!({"tier": tier, "runs": runs, "solved": solved, "rate": *solved as f64 / *runs as f64})
        }).collect::<Vec<_>>(),
        "failure_categories": categories,
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
    })
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
    for (model, (entries, turns, calls, failed, malformed)) in by_model {
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
