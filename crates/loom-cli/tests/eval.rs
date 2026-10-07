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
//! # live (spends tokens, needs DEEPSEEK_API_KEY):
//! DEEPSEEK_API_KEY=... cargo test -p loom-cli --test eval \
//!     -- --ignored --nocapture
//! # offline replay of whatever recordings exist (always safe):
//! cargo test -p loom-cli --test eval -- --nocapture
//! ```
//!
//! Knobs: `LOOM_EVAL_MODEL` (default `deepseek-flash`), `LOOM_EVAL_RUNS`
//! (default 5), `LOOM_EVAL_DIR` (default `<workspace>/target/eval`),
//! `LOOM_EVAL_TASKS` (comma-separated task ids; default all).
//! Live output lands in `LOOM_EVAL_DIR`: `records/<task>-<model>-<run>.jsonl`
//! and `report-<task>-<model>.json`. When recording a baseline, copy the
//! report into `docs/benchmarks/`.
//!
//! **Never point this at the GLM Coding Plan** (research.md §4).

// A test harness: panicking on a setup failure is the intended behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use loom_core::engine::repo::FsFactsSource;
use loom_core::kernel::agent::Agent;
use loom_core::kernel::context::{
    AgentConfig, ContextCompiler, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
    TokenEstimator,
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
use loom_core::providers::{DeepSeekConfig, DeepSeekProvider, ReasoningEffort, model_capabilities};
use loom_core::tools::{
    EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool, VerifyTool, WriteFileTool,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- tasks ---

/// The tools a task may call. Deliberately coarse: the point is to pin
/// which capabilities a baseline exercised, not to configure the agent.
#[derive(Debug, Clone, Copy)]
enum ToolSet {
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
    /// in `LOOM_EVAL_TASKS` (B5, B7).
    heavy: bool,
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
        loaded.push(EvalTask {
            id: def.id,
            prompt: def.prompt,
            workspace,
            tools,
            heavy: def.heavy,
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
        if entry.file_type()?.is_dir() {
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

/// Wraps a live provider and records every completed exchange.
struct RecordingProvider<P: ModelProvider> {
    inner: Arc<P>,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
}

impl<P: ModelProvider> RecordingProvider<P> {
    fn new(inner: P) -> (Self, Arc<Mutex<Vec<Exchange>>>) {
        let exchanges = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Arc::new(inner),
                exchanges: Arc::clone(&exchanges),
            },
            exchanges,
        )
    }
}

impl<P: ModelProvider> ModelProvider for RecordingProvider<P> {
    fn name(&self) -> &str {
        "record"
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
    script: Mutex<VecDeque<ModelResponse>>,
    served: AtomicUsize,
}

impl ReplayProvider {
    fn new(model: &str, exchanges: &[Exchange]) -> Self {
        Self {
            model: model.to_owned(),
            script: Mutex::new(exchanges.iter().map(|e| e.response.clone()).collect()),
            served: AtomicUsize::new(0),
        }
    }
}

impl ModelProvider for ReplayProvider {
    fn name(&self) -> &str {
        "replay"
    }

    fn capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        // Mirror the live DeepSeek table so offline replay compiles
        // recordings produced against the real window (deepseek-flash is
        // 1M tokens, not the 128K this replay double used to assume).
        (model == self.model)
            .then(|| model_capabilities(model))
            .flatten()
    }

    fn stream<'a>(&'a self, request: &'a ModelRequest) -> ModelStream<'a> {
        assert_eq!(request.model, self.model, "replay: model drifted");
        let next = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
            panic!(
                "replay script exhausted after {} responses",
                self.served.load(Ordering::SeqCst)
            )
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

/// Peak prices in USD per 1M tokens (research.md §8); duplicates
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

fn cost_usd(model: &str, hit: u64, miss: u64, output: u64, multiplier: f64) -> f64 {
    let prices = match model {
        "deepseek-flash" => &FLASH,
        "deepseek-v4-pro" => &PRO,
        other => panic!("no price table for model `{other}`"),
    };
    let per_million = |tokens: u64, price: f64| tokens as f64 / 1_000_000.0 * price;
    multiplier
        * (per_million(miss, prices.input_miss)
            + per_million(hit, prices.input_hit)
            + per_million(output, prices.output))
}

/// Peak Mon–Fri 01:00–04:00 & 06:00–10:00 UTC; else off-peak (0.5×).
fn price_multiplier(now: SystemTime) -> (f64, &'static str) {
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

fn registry(task: &EvalTask) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool)).unwrap();
    registry.register(Box::new(ListDirTool)).unwrap();
    if !matches!(task.tools, ToolSet::ReadEdit) {
        registry.register(Box::new(SearchTool)).unwrap();
    }
    if matches!(task.tools, ToolSet::Coding | ToolSet::ReadEdit) {
        registry.register(Box::new(EditFileTool)).unwrap();
    }
    if matches!(task.tools, ToolSet::Coding) {
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
    let (code, output) = if needs_tests {
        let root = root.to_path_buf();
        match tokio::task::spawn_blocking(move || run_fixture_tests(&root)).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                return Some(serde_json::json!({
                    "bug_fixed": false,
                    "error": format!("cargo test failed to run: {error}"),
                }));
            }
            Err(join) => {
                return Some(serde_json::json!({
                    "bug_fixed": false,
                    "error": format!("gate task join error: {join}"),
                }));
            }
        }
    } else {
        (None, String::new())
    };
    match gate {
        TaskGate::Bugfix(spec) => {
            let outcomes = test_outcomes(&output);
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
            Some(serde_json::json!({
                "bug_fixed": bug_fixed,
                "cargo_exit": code,
                "fail_to_pass": fail_to_pass,
                "pass_to_pass": pass_to_pass,
                "tests_passed": outcomes.iter().filter(|(_, ok)| *ok).count(),
                "protected_unmodified": protected_unmodified,
            }))
        }
        TaskGate::Outcome(spec) => {
            let mut checks = Vec::new();
            for (rel, expected) in &spec.exact_files {
                // One optional trailing newline is tolerated (see OutcomeSpec).
                let ok = std::fs::read(root.join(rel)).is_ok_and(|body| {
                    body.strip_suffix(b"\n").unwrap_or(&body) == expected.as_bytes()
                });
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
                let passed = count_tests_passed(&output);
                checks.push(serde_json::json!({
                    "tests_green": true,
                    "ok": code == Some(0) && passed >= expected,
                }));
            }
            let bug_fixed = checks
                .iter()
                .all(|check| check["ok"].as_bool().unwrap_or(false));
            Some(serde_json::json!({
                "bug_fixed": bug_fixed,
                "checks": checks,
                "cargo_exit": code,
            }))
        }
    }
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

/// Runs one task against a provider and returns the 1H-schema report plus
/// the recorded exchanges and the finished session. `provider_label` names
/// the concrete backend the run used, so billed-token figures in the report
/// are attributable ("deepseek", "replay", …) and never read as
/// provider-agnostic.
async fn run_once(
    task: &EvalTask,
    model: &str,
    provider_label: &str,
    provider: Box<dyn ModelProvider>,
    exchanges: Option<Arc<Mutex<Vec<Exchange>>>>,
) -> (serde_json::Value, Vec<Exchange>, Session) {
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
    // 2L/ADR-011: pruning is OFF by default so baseline numbers stay
    // comparable; set LOOM_EVAL_PRUNE=1 (or on/true) for the pruning arm.
    let pruning = std::env::var("LOOM_EVAL_PRUNE").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value != "0" && value != "off" && value != "false"
    });
    // Parity with `Engine::open`: the eval counts tokens with the same
    // o200k estimator the engine injects, never the byte heuristic.
    let estimator: std::sync::Arc<dyn TokenEstimator> =
        std::sync::Arc::new(loom_core::engine::O200kEstimator);
    let compiler: Box<dyn ContextCompiler> = if pruning {
        Box::new(
            PruningContextCompiler::new(
                Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                prune_config(),
            )
            .with_estimator(estimator),
        )
    } else {
        Box::new(DefaultContextCompiler::with_estimator(estimator))
    };
    let system_prompt = task
        .system_prompt
        .clone()
        .unwrap_or_else(loom_core::engine::default_system_prompt);
    let agent = Agent::new(
        provider,
        compiler,
        registry(task),
        AgentConfig::new(root.clone(), system_prompt),
        model.to_owned(),
    )
    .with_facts_source(Box::new(FsFactsSource::new(root.clone())));
    let mut session = Session::new(SessionId::new());
    let cancellation = CancellationToken::new();
    // V21: strict evaluation sink — a run that loses events fails rather than
    // silently producing a truncated trace. The tally sink cannot drop, so
    // this only bites if the harness ever swaps in a lossy sink.
    let tally_state = Arc::new(Mutex::new(Tally::default()));
    let sink = StrictSink::new(CountingSink(Arc::clone(&tally_state)), cancellation.clone());

    let started = Instant::now();
    let outcome = agent
        .run_task(&mut session, task.prompt.clone(), &sink, &cancellation)
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
            serde_json::json!({
                "task": task.id,
                "model": model,
                "provider": provider_label,
                "success": true,
                "turns": summary.turns,
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
                "cost_usd": cost_usd(model, hit, miss, summary.output_tokens, multiplier),
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
            })
        }
        Err(error) => serde_json::json!({
            "task": task.id,
            "model": model,
            "provider": provider_label,
            "success": false,
            "end_reason": error.end_reason(),
            "error": error.to_string(),
            "retries": tally.retries,
            "observed_wall_time_ms": observed_ms,
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
#[ignore = "live: needs DEEPSEEK_API_KEY; spends real tokens"]
async fn eval_live() {
    let Ok(api_key) = std::env::var("DEEPSEEK_API_KEY") else {
        eprintln!("skipping eval_live: DEEPSEEK_API_KEY is not set");
        return;
    };
    let model = std::env::var("LOOM_EVAL_MODEL").unwrap_or_else(|_| String::from("deepseek-flash"));
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

    let only: Option<Vec<String>> = std::env::var("LOOM_EVAL_TASKS")
        .ok()
        .map(|raw| raw.split(',').map(|id| id.trim().to_owned()).collect());

    let all_tasks = tasks();
    let mut suite: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for task in &all_tasks {
        if let Some(only) = &only
            && !only.iter().any(|id| id.as_str() == task.id)
        {
            continue;
        }
        // Heavy (long-context) tasks run only when explicitly named: the
        // default suite stays at B1–B4 to bound time and cost.
        if task.heavy && only.is_none() {
            continue;
        }
        let mut reports = Vec::new();
        for run in 1..=runs {
            let deepseek = DeepSeekProvider::new(
                DeepSeekConfig::new(api_key.clone()).with_reasoning_effort(ReasoningEffort::High),
            )
            .unwrap();
            let provider_label = deepseek.name().to_owned();
            let (provider, exchanges) = RecordingProvider::new(deepseek);
            let (report, recorded, _session) = run_once(
                task,
                &model,
                &provider_label,
                Box::new(provider),
                Some(exchanges),
            )
            .await;
            eprintln!(
                "{} run {}/{}: success={} bug_fixed={}",
                task.id, run, runs, report["success"], report["gate"]["bug_fixed"]
            );
            std::fs::write(
                records_dir.join(format!("{}-{model}-{run}.jsonl", task.id)),
                recorded
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            )
            .unwrap();
            reports.push(report);
        }

        let report = serde_json::json!({
            "harness": "eval 2A",
            "task": task.id,
            "model": model,
            "provider": "deepseek",
            "units": {
                "census": "exact serialized bytes (provider-agnostic, deterministic)",
                "est_tokens": "bytes/3 budget heuristic",
                "tokens/cost": "provider-billed, per-model evidence only",
            },
            "runs": reports,
            "aggregates": aggregates_from(&reports),
            "notes": task.notes,
        });
        let path = dir.join(format!("report-{}-{model}.json", task.id));
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
    let mut per_task = serde_json::Map::new();
    let mut gated_runs = 0usize;
    let mut gated_solved = 0usize;
    for (id, reports) in &suite {
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
            serde_json::json!({
                "kind": "field",
                "runs": gated.len(),
                "solved": solved,
                "rate": solved as f64 / gated.len() as f64,
            })
        };
        per_task.insert(id.clone(), entry);
    }
    let headline = serde_json::json!({
        "model": model,
        "mode": if tolerate_failures { "tolerant" } else { "strict" },
        "tasks": per_task,
        "gated_runs": gated_runs,
        "gated_solved": gated_solved,
        "pass_rate": if gated_runs == 0 {
            serde_json::json!(null)
        } else {
            serde_json::json!(gated_solved as f64 / gated_runs as f64)
        },
    });
    let headline_path = dir.join(format!("report-suite-{model}.json"));
    std::fs::write(
        &headline_path,
        serde_json::to_string_pretty(&headline).unwrap(),
    )
    .unwrap();
    eprintln!(
        "suite pass rate: {gated_solved}/{gated_runs} gated runs ({}) — wrote {}",
        headline["pass_rate"],
        headline_path.display()
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
/// responses through the real agent + real tools reproduces the recorded
/// assistant turns verbatim in the transcript.
async fn assert_replay_equivalence(task: &EvalTask, model: &str, exchanges: &[Exchange]) {
    let replay = ReplayProvider::new(model, exchanges);
    let (report, _recorded, session) =
        run_once(task, model, "replay", Box::new(replay), None).await;
    assert!(
        report["success"].as_bool().unwrap(),
        "replay of {} failed: {report}",
        task.id
    );

    let replayed: Vec<&AssistantMessage> = session
        .transcript
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed.len(),
        exchanges.len(),
        "{}: replay produced {} assistant turns, recording has {}",
        task.id,
        replayed.len(),
        exchanges.len()
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
    let (provider, captured) = RecordingProvider::new(replay);
    let (_report, recorded, _session) =
        run_once(task, model, "replay", Box::new(provider), Some(captured)).await;
    recorded
}

/// Replays every recording on disk. Self-skips when no recordings exist
/// (fresh checkout); `eval_live` (ignored test) creates them.
#[tokio::test]
async fn eval_replay_recordings() {
    let model = std::env::var("LOOM_EVAL_MODEL").unwrap_or_else(|_| String::from("deepseek-flash"));
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
        let task_id = file
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split('-').next())
            .unwrap_or("B1");
        let Some(task) = all_tasks.iter().find(|t| t.id == task_id) else {
            eprintln!("{}: no pinned task `{task_id}` — skipped", file.display());
            continue;
        };
        let exchanges = read_exchanges(&file);
        eprintln!(
            "replaying {} ({} exchanges)",
            file.display(),
            exchanges.len()
        );
        assert_replay_equivalence(task, &model, &exchanges).await;
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
