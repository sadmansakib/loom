//! Evaluation harness (plan-iteration-2.md §1, step 2A).
//!
//! Precedes all optimization work. Three pieces:
//!
//! 1. **Pinned task set B1–B4 (plus heavy B5).** B1 inspects the Loom
//!    repo with the read-only tools; B2–B4 exercise the mutating tools
//!    against a generated fixture crate under the eval dir. B5 is a
//!    read-then-edit long-context task over a generated corpus (each read
//!    is superseded by the following edit, so pruning is safe); it is far
//!    slower and costlier, so it runs only when named in
//!    `LOOM_EVAL_TASKS`. Prompts are frozen.
//! 2. **N-run protocol.** B1 showed 1.85× input-token variance on a pinned
//!    task (thinking mode ignores temperature), so live evaluation always
//!    runs N ≥ 5 times and reports mean/median/min/max — never a single
//!    run. Per-run records follow the 1H schema.
//! 3. **Record/replay provider.** Live runs record every
//!    `(ModelRequest, ModelResponse)` exchange as JSONL. Replays re-run the
//!    agent offline (canned responses, real tools) and require the
//!    transcript's assistant items to equal the recordings verbatim — the
//!    V3 / Invariant-10 replayability claim, tested without a network.
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use loom_core::engine::repo::FsFactsSource;
use loom_core::kernel::agent::Agent;
use loom_core::kernel::context::{
    AgentConfig, ContextCompiler, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
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

/// Which workspace a task runs against.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Workspace {
    /// The Loom repository itself (read-only inspection, B1).
    Repo,
    /// A generated throwaway crate under the eval dir (B2–B4).
    Fixture,
    /// A generated long-context corpus of ~40 KB markdown documents
    /// (B5); sized so a sequential read pushes the projection past the
    /// 64 K pruning trigger.
    Corpus,
}

/// The tools a task may call. Deliberately coarse: the point is to pin
/// which capabilities a baseline exercised, not to configure the agent.
#[derive(Debug, Clone, Copy)]
enum ToolSet {
    /// `read_file`, `list_dir`, `search` (B1's 2026-10-06 baseline).
    ReadOnly,
    /// Read-only set plus `write_file`, `edit_file`, `run_command`,
    /// `verify` (B2–B4).
    Coding,
    /// `read_file` + `list_dir` + `edit_file` (B5). The task reads each file
    /// and then edits it, so every read becomes stale (superseded by the
    /// write) and pruning can drop it without the model needing to recover
    /// the content — the pruning-appropriate long-context shape.
    ReadEdit,
}

/// A pinned evaluation task. Prompts are frozen: changing one invalidates
/// every baseline recorded against it.
struct EvalTask {
    id: &'static str,
    prompt: &'static str,
    workspace: Workspace,
    /// Capabilities available to the task; B1 keeps the read-only set so its
    /// 2026-10-06 baseline stays comparable.
    tools: ToolSet,
    /// Long-context task: skipped unless explicitly named in
    /// `LOOM_EVAL_TASKS` (it is far slower and costlier than B1–B4).
    heavy: bool,
    system_prompt: &'static str,
}

const TASKS: &[EvalTask] = &[
    EvalTask {
        id: "B1",
        prompt: "Inspect this repository, find the implementation of the \
                 retry and cancellation handling in the agent loop, explain \
                 how it works, and identify the relevant files.",
        workspace: Workspace::Repo,
        tools: ToolSet::ReadOnly,
        heavy: false,
        system_prompt: READ_ONLY_SYSTEM_PROMPT,
    },
    EvalTask {
        id: "B2",
        prompt: "Create the file eval-notes/greeting.txt containing exactly \
                 the text 'hello eval', then read it back and verify its \
                 content.",
        workspace: Workspace::Fixture,
        tools: ToolSet::Coding,
        heavy: false,
        system_prompt: CODING_SYSTEM_PROMPT,
    },
    EvalTask {
        id: "B3",
        prompt: "Add a function `sub(a: i64, b: i64)` to src/calc.rs \
                 mirroring the existing `add` function, then run the tests \
                 covering src/calc.rs and report any failures.",
        workspace: Workspace::Fixture,
        tools: ToolSet::Coding,
        heavy: false,
        system_prompt: CODING_SYSTEM_PROMPT,
    },
    EvalTask {
        id: "B4",
        prompt: "Read every markdown file under docs-notes/, then write a \
                 one-paragraph summary of each file into \
                 docs-notes/summaries.md, in file order.",
        workspace: Workspace::Fixture,
        tools: ToolSet::Coding,
        heavy: false,
        system_prompt: CODING_SYSTEM_PROMPT,
    },
    EvalTask {
        id: "B5",
        prompt: "There are 8 markdown files in corpus/, named doc-01.md \
                 through doc-08.md. Process them in ascending filename \
                 order. For each file: read it with read_file, then append a \
                 line to it with edit_file by setting old_string to the \
                 file's current last line and new_string to that same last \
                 line followed by a newline and the text 'Reviewed.'. Issue \
                 exactly one tool call per turn and stop after that single \
                 call so the next turn continues with the next file. Do not \
                 search or read partial ranges, do not skip any file, and do \
                 not stop before all 8 files are processed: stopping early \
                 fails the task. After the last file is edited, reply with a \
                 single line DONE.",
        workspace: Workspace::Corpus,
        tools: ToolSet::ReadEdit,
        heavy: true,
        system_prompt: LONG_CONTEXT_SYSTEM_PROMPT,
    },
];

/// B1's original read-only prompt, kept byte-identical to the 2026-10-06
/// baseline (and to `benchmark.rs`).
const READ_ONLY_SYSTEM_PROMPT: &str = "You are Loom, a minimal coding agent working read-only inside one workspace. \
     Use the provided tools to inspect the workspace: list directories and search \
     before reading large files, and prefer reading the smallest relevant range. \
     When you answer, be concise and name the workspace-relative paths you relied on.";

/// B5's prompt: the opposite of the read-only default. A long-context task
/// must read whole files without searching, so the model cannot satisfy it
/// with a few small ranges and never exercises the pruning trigger.
const LONG_CONTEXT_SYSTEM_PROMPT: &str = "You are Loom, a minimal coding agent working inside one workspace. \
     This task requires reading whole files and then editing them, one after another. \
     Use read_file to read whole files and edit_file to change them; do not search, \
     sample, or read partial ranges. Keep going until every requested file has been \
     processed, then answer exactly as instructed. When you answer, be concise and name \
     the workspace-relative paths you relied on.";

/// Prompt for tasks that may modify the workspace; identical to `loom-cli`'s.
const CODING_SYSTEM_PROMPT: &str = "You are Loom, a coding agent working inside one workspace. Use the provided \
     tools to inspect and change the workspace: list and search before reading, \
     read before editing, write or edit files to make changes, and run allow-listed \
     commands (cargo) to verify. Prefer the smallest change that works. When you \
     answer, be concise and name the workspace-relative paths you relied on.";

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

/// The generated fixture workspace the mutating tasks run against.
fn fixture_root() -> PathBuf {
    eval_dir().join("fixture")
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

/// (Re)creates the fixture crate. Idempotent: wiped and rewritten so every
/// run starts from identical bytes.
fn ensure_fixture() {
    let root = fixture_root();
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let write = |rel: &str, body: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    };
    write(
        "Cargo.toml",
        "[package]\nname = \"loom-eval-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         \n[workspace]\n",
    );
    write("src/lib.rs", "pub mod calc;\n");
    write(
        "src/calc.rs",
        "pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    \
         use super::*;\n\n    #[test]\n    fn add_works() {\n        assert_eq!(add(2, 3), 5);\n    \
         }\n}\n",
    );
    write(
        "docs-notes/alpha.md",
        "# Alpha\n\nThe alpha subsystem handles the first half of the pipeline.\n",
    );
    write(
        "docs-notes/beta.md",
        "# Beta\n\nThe beta subsystem consumes alpha's output and reports results.\n",
    );
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
    let root = match task.workspace {
        Workspace::Repo => workspace_root(),
        Workspace::Fixture => {
            ensure_fixture();
            fixture_root()
        }
        Workspace::Corpus => {
            ensure_corpus();
            corpus_root()
        }
    };
    // 2L/ADR-011: pruning is OFF by default so baseline numbers stay
    // comparable; set LOOM_EVAL_PRUNE=1 (or on/true) for the pruning arm.
    let pruning = std::env::var("LOOM_EVAL_PRUNE").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value != "0" && value != "off" && value != "false"
    });
    let compiler: Box<dyn ContextCompiler> = if pruning {
        Box::new(PruningContextCompiler::new(
            Box::new(DefaultContextCompiler),
            PruneConfig::default(),
        ))
    } else {
        Box::new(DefaultContextCompiler)
    };
    let agent = Agent::new(
        provider,
        compiler,
        registry(task),
        AgentConfig::new(root.clone(), task.system_prompt),
        model.to_owned(),
    )
    .with_facts_source(Box::new(FsFactsSource::new(root)));
    let mut session = Session::new(SessionId::new());
    let cancellation = CancellationToken::new();
    // V21: strict evaluation sink — a run that loses events fails rather than
    // silently producing a truncated trace. The tally sink cannot drop, so
    // this only bites if the harness ever swaps in a lossy sink.
    let tally_state = Arc::new(Mutex::new(Tally::default()));
    let sink = StrictSink::new(CountingSink(Arc::clone(&tally_state)), cancellation.clone());

    let started = Instant::now();
    let outcome = agent
        .run_task(&mut session, task.prompt, &sink, &cancellation)
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
    let census_turns: Vec<serde_json::Value> = recorded
        .iter()
        .map(|e| {
            let c = census(&e.request);
            serde_json::json!({
                "system_bytes": c.system,
                "user_bytes": c.user,
                "assistant_bytes": c.assistant,
                "tool_result_bytes": c.tool_result,
                "tool_specs_bytes": c.tool_specs,
                "est_tokens": c.est_tokens(),
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

    let report = match &outcome {
        Ok(summary) => {
            assert_eq!(
                summary.events_dropped, 0,
                "{} ({model}): dropped {} events; evaluation fails on loss",
                task.id, summary.events_dropped
            );
            let hit = summary.cached_input_tokens.unwrap_or(0);
            let miss = summary.input_tokens.saturating_sub(hit);
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

    let dir = eval_dir();
    let records_dir = dir.join("records");
    std::fs::create_dir_all(&records_dir).unwrap();

    let only: Option<Vec<String>> = std::env::var("LOOM_EVAL_TASKS")
        .ok()
        .map(|raw| raw.split(',').map(|id| id.trim().to_owned()).collect());

    for task in TASKS {
        if let Some(only) = &only
            && !only.iter().any(|id| id == task.id)
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
                "{} run {}/{}: success={}",
                task.id, run, runs, report["success"]
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
        });
        let path = dir.join(format!("report-{}-{model}.json", task.id));
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        eprintln!("wrote {}", path.display());
        assert!(
            reports.iter().all(|r| r["success"].as_bool().unwrap()),
            "{}: at least one live run failed",
            task.id
        );
    }
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
    serde_json::json!({
        "turns": aggregate(&pick("turns")),
        "input_tokens": aggregate(&pick("input_tokens")),
        "cache_hit_tokens": aggregate(&pick("cache_hit_tokens")),
        "cache_miss_tokens": aggregate(&pick("cache_miss_tokens")),
        "output_tokens": aggregate(&pick("output_tokens")),
        "wall_time_ms": aggregate(&pick("wall_time_ms")),
        "cost_usd": aggregate_f64(&pick_f64("cost_usd")),
    })
}

// ------------------------------------------------- replay equivalence ----

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
    for file in files {
        let task_id = file
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split('-').next())
            .unwrap_or("B1");
        let Some(task) = TASKS.iter().find(|t| t.id == task_id) else {
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

    let task = TASKS.iter().find(|t| t.id == "B1").unwrap();
    assert_replay_equivalence(task, model, &exchanges).await;

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
    let first = replay_capturing(task, model, &exchanges).await;
    let second = replay_capturing(task, model, &exchanges).await;
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
