//! Offline counterfactual context replay (`loom replay-context`).
//!
//! Re-compiles a recorded session through the *real* compilers —
//! [`DefaultContextCompiler`] inside [`PruningContextCompiler`] — turn by
//! turn, carrying the pruning checkpoint exactly like the live loop, under
//! a baseline `PruneConfig` and optional `--set` overlays. Zero model
//! calls, zero I/O beyond reading the recorded input.
//!
//! What this measures: estimated input tokens per turn (the same census
//! and estimator family the live run used), checkpoint freezes and
//! reclaimed tokens, and cache-prefix stability (the common byte prefix
//! between consecutive compiled requests — a drop is a cache-bust proxy).
//! What it cannot measure: task success — the transcript is fixed, so a
//! counterfactual stub that would have changed the model's behavior is
//! invisible. Callers must say so wherever these numbers are shown.
//!
//! Design: `docs/replay-context-design.md`.

use std::io;
use std::path::{Path, PathBuf};

use loom_kernel::context::{
    AgentConfig, ContextBudget, ContextCensus, ContextCompiler, ContextError, ContextInput,
    DefaultContextCompiler, KnownFacts, PruneCheckpoint, PruneConfig, PruneReport,
    PruningContextCompiler,
};
use loom_kernel::model::ModelCapabilities;
use loom_kernel::session::{Session, TaskRecord};
use loom_kernel::transcript::Item;

use crate::estimate;
use crate::trace::{EVENTS_FILE_NAME, MANIFEST_FILE_NAME, TRANSCRIPT_FILE_NAME};

/// Assumed window when neither the input nor the caller names one
/// (`deepseek-flash`, the eval model, has 1M).
pub const DEFAULT_WINDOW_TOKENS: u32 = 1_000_000;
/// Assumed output reservation (the engine's automatic reservation cap).
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 32_768;

/// Reconstruction constants shared by both replay arms.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Provider adapter name; picks the estimator family the original run
    /// used (`anthropic` scales o200k, everything else is o200k).
    pub provider: String,
    /// Full context window assumed for the budget (`--window`).
    pub window_tokens: u32,
    /// Output reservation assumed for the budget (`--reserved-output`).
    pub reserved_output_tokens: u32,
    /// System prompt used for the reconstruction (not recorded in traces).
    pub system_prompt: String,
    /// Workspace root for [`AgentConfig`]; compile never touches it.
    pub workspace_root: PathBuf,
}

impl ReplayOptions {
    /// Defaults for a provider; see the field docs for what is assumed.
    #[must_use]
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            window_tokens: DEFAULT_WINDOW_TOKENS,
            reserved_output_tokens: DEFAULT_RESERVED_OUTPUT_TOKENS,
            system_prompt: crate::engine::default_system_prompt(),
            workspace_root: PathBuf::from("."),
        }
    }
}

/// One replayed compile (turn numbers are 1-based compile ordinals).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReplay {
    /// 1-based turn ordinal, aligned with `TurnStarted` numbering.
    pub turn: u32,
    /// `census.total` — the authoritative estimate the budget check sees.
    pub input_tokens: u32,
    /// Per-source census split.
    pub census: ContextCensus,
    /// Set only on the compile that froze a new checkpoint.
    pub froze: Option<PruneReport>,
    /// Common byte prefix with the previous compiled request ÷ previous
    /// request length. `None` on the first turn.
    pub prefix_stability: Option<f64>,
    /// Calls stubbed in this compiled projection.
    pub stubbed_calls: usize,
}

/// Aggregate over one replay arm.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaySummary {
    /// Per-turn results, in order.
    pub turns: Vec<TurnReplay>,
    /// Sum of per-turn `census.total` (projected input spend).
    pub total_input_tokens: u64,
    /// Token estimate reclaimed by freezes.
    pub reclaimed_tokens: u64,
    /// Number of freezes.
    pub freezes: u64,
    /// Median per-turn input tokens (the selfopt gate statistic).
    pub median_input_tokens: f64,
    /// Smallest prefix-stability ratio across the arm.
    pub min_prefix_stability: Option<f64>,
    /// Set when a projection overran past rescue: the live loop would
    /// spend one model call on an LLM handoff, which replay cannot
    /// synthesize — the arm stops here.
    pub handoff_stopped_at: Option<u32>,
}

/// Replay failures that are input problems, not measurement results.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The recorded input is malformed (bad manifest/transcript shape).
    #[error("invalid trace: {0}")]
    InvalidTrace(String),
    /// Reading the trace directory failed.
    #[error("reading trace: {0}")]
    Io(#[from] io::Error),
    /// The budget constants are inconsistent (reservation > window).
    #[error("{0}")]
    Budget(String),
}

/// A trace directory's parsed contents (see [`TraceInput::read`]).
#[derive(Debug, Clone)]
pub struct TraceInput {
    /// The recorded session, rebuilt from `transcript.json`.
    pub session: Session,
    /// The manifest's task record when present, else the session's last.
    pub task: TaskRecord,
    /// Manifest harness provider (estimator family).
    pub provider: String,
    /// Manifest harness model.
    pub model: String,
    /// The trace's `event_schema_version`, for skew detection: a census
    /// recorded by an older schema came from an older compiler
    /// generation and is not comparable to this binary's projection.
    pub schema_version: u16,
    /// Manifest summary ground truth, when the manifest carries one.
    pub summary: Option<serde_json::Value>,
    /// `context_pruned` events from `events.ndjson` (turn, report), when
    /// the events file exists and parses.
    pub prune_events: Vec<(u32, PruneReport)>,
}

impl TraceInput {
    /// Loads and parses a task directory written by `loom run --trace`:
    /// `manifest.json` + `transcript.json` (events optional).
    pub fn read(dir: &Path) -> Result<Self, ReplayError> {
        let manifest_text =
            std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).map_err(|error| {
                ReplayError::InvalidTrace(format!(
                    "no {MANIFEST_FILE_NAME} in {}: {error}",
                    dir.display()
                ))
            })?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
            .map_err(|error| ReplayError::InvalidTrace(format!("manifest: {error}")))?;
        let provider = manifest["harness"]["provider"]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidTrace("manifest.harness.provider missing".into()))?
            .to_owned();
        let model = manifest["harness"]["model"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        let transcript_text =
            std::fs::read_to_string(dir.join(TRANSCRIPT_FILE_NAME)).map_err(|error| {
                ReplayError::InvalidTrace(format!(
                    "no {TRANSCRIPT_FILE_NAME} in {}: {error}",
                    dir.display()
                ))
            })?;
        let transcript: serde_json::Value = serde_json::from_str(&transcript_text)
            .map_err(|error| ReplayError::InvalidTrace(format!("transcript: {error}")))?;
        let session_id = transcript["session_id"]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidTrace("transcript.session_id missing".into()))?;
        let session_id: loom_kernel::id::SessionId = session_id.parse().map_err(|error| {
            ReplayError::InvalidTrace(format!("transcript session_id: {error}"))
        })?;
        let mut session = Session::new(session_id);
        for wrapper in transcript["items"].as_array().unwrap_or(&Vec::new()) {
            let item: Item = serde_json::from_value(wrapper["item"].clone())
                .map_err(|error| ReplayError::InvalidTrace(format!("transcript item: {error}")))?;
            session.transcript.push(item);
        }
        for task in transcript["tasks"].as_array().unwrap_or(&Vec::new()) {
            let task: TaskRecord = serde_json::from_value(task.clone())
                .map_err(|error| ReplayError::InvalidTrace(format!("transcript task: {error}")))?;
            session.tasks.push(task);
        }
        let task = manifest["task_id"]
            .as_str()
            .and_then(|id| session.tasks.iter().find(|task| task.id.to_string() == id))
            .or_else(|| session.tasks.last())
            .cloned()
            .ok_or_else(|| ReplayError::InvalidTrace("no task record in transcript".into()))?;

        let mut prune_events = Vec::new();
        if let Ok(events) = std::fs::read_to_string(dir.join(EVENTS_FILE_NAME)) {
            for line in events.lines() {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if event["type"] == "context_pruned"
                    && let Ok(report) =
                        serde_json::from_value::<PruneReport>(event["report"].clone())
                {
                    let turn = event["turn"].as_u64().unwrap_or_default() as u32;
                    prune_events.push((turn, report));
                }
            }
        }

        Ok(Self {
            session,
            task,
            provider,
            model,
            schema_version: manifest["event_schema_version"]
                .as_u64()
                .unwrap_or_default() as u16,
            summary: manifest["summary"]
                .as_object()
                .cloned()
                .map(serde_json::Value::Object),
            prune_events,
        })
    }
}

/// Replays one session under one config. The session is compiled at every
/// prefix that ends right before an assistant item — exactly where the
/// live loop compiles. Fast-path tasks (zero model turns) contribute no
/// compile. A projection that overruns past rescue stops the arm with
/// [`ReplaySummary::handoff_stopped_at`] set: the live loop's handoff
/// needs one model call, which replay refuses to fake.
pub fn replay_session(
    session: &Session,
    task: &TaskRecord,
    options: &ReplayOptions,
    config: PruneConfig,
) -> Result<ReplaySummary, ReplayError> {
    let estimator = estimate::estimator_for(&options.provider);
    let registry =
        crate::engine::replay_registry().map_err(|error| ReplayError::Budget(error.to_string()))?;
    let capabilities = ModelCapabilities::new(options.window_tokens)
        .with_tool_calls(true)
        .with_max_output_tokens(options.reserved_output_tokens);
    let budget = ContextBudget::from_window(options.window_tokens, options.reserved_output_tokens)
        .map_err(|error| ReplayError::Budget(error.to_string()))?;
    let agent_config = AgentConfig::new(
        options.workspace_root.clone(),
        options.system_prompt.clone(),
    );
    let facts = KnownFacts::default();
    let compiler = PruningContextCompiler::new(
        Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
        config,
    )
    .with_estimator(estimator);

    // Fast-path sessions never compile; the booked assistant item is not
    // a model turn.
    let skip_all = session.tasks.last().is_some_and(|task| task.turns == 0);

    let mut prefix = Session::new(session.id);
    prefix.tasks = vec![task.clone()];
    let mut checkpoint: Option<PruneCheckpoint> = None;
    let mut turns = Vec::new();
    let mut previous_bytes: Option<Vec<u8>> = None;
    let mut handoff_stopped_at = None;

    if !skip_all {
        for item in &session.transcript {
            if let Item::Assistant(_) = item {
                let input = ContextInput::new(
                    &prefix,
                    &registry,
                    task,
                    budget,
                    &capabilities,
                    "replay",
                    &agent_config,
                    &facts,
                    checkpoint.as_ref(),
                );
                match compiler.compile(input) {
                    Ok(compiled) => {
                        checkpoint = compiled.prune_checkpoint;
                        // Serialize each message separately and concatenate:
                        // the whole-array JSON form ends in `]`, which would
                        // break the byte-prefix property on every append.
                        let bytes = compiled
                            .request
                            .messages
                            .iter()
                            .filter_map(|message| serde_json::to_vec(message).ok())
                            .collect::<Vec<_>>()
                            .concat();
                        let prefix_stability = previous_bytes.as_ref().map(|prev| {
                            common_prefix_len(prev, &bytes) as f64 / prev.len().max(1) as f64
                        });
                        previous_bytes = Some(bytes);
                        turns.push(TurnReplay {
                            turn: turns.len() as u32 + 1,
                            input_tokens: compiled.census.total,
                            census: compiled.census,
                            froze: compiled.prune_report,
                            prefix_stability,
                            stubbed_calls: compiled.stubbed_calls.len(),
                        });
                    }
                    Err(ContextError::BudgetExceeded { .. }) => {
                        // The live loop would attempt an LLM handoff here;
                        // the handoff summary is a model call replay cannot
                        // make. Stop honestly.
                        handoff_stopped_at = Some(turns.len() as u32 + 1);
                        break;
                    }
                    Err(error) => {
                        return Err(ReplayError::InvalidTrace(format!(
                            "compile failed at turn {}: {error}",
                            turns.len() + 1
                        )));
                    }
                }
            }
            // Prefix building bypasses `push_item` validation on purpose:
            // every item comes from a stored (already validated) session,
            // and intermediate prefixes legitimately end mid-turn.
            prefix.transcript.push(item.clone());
        }
    }

    let total_input_tokens: u64 = turns.iter().map(|turn| u64::from(turn.input_tokens)).sum();
    let reclaimed_tokens: u64 = turns
        .iter()
        .filter_map(|turn| turn.froze.as_ref())
        .map(|report| report.reclaimed_tokens)
        .sum();
    let freezes =
        u64::try_from(turns.iter().filter(|turn| turn.froze.is_some()).count()).unwrap_or(u64::MAX);
    let median_input_tokens = median(
        &turns
            .iter()
            .map(|turn| f64::from(turn.input_tokens))
            .collect::<Vec<_>>(),
    );
    let min_prefix_stability =
        turns
            .iter()
            .filter_map(|turn| turn.prefix_stability)
            .fold(None::<f64>, |acc, value| match acc {
                Some(current) => Some(current.min(value)),
                None => Some(value),
            });
    Ok(ReplaySummary {
        turns,
        total_input_tokens,
        reclaimed_tokens,
        freezes,
        median_input_tokens,
        min_prefix_stability,
        handoff_stopped_at,
    })
}

/// The self-validity check: replaying recorded ground truth under its own
/// original config must reproduce it, or the tool says so.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidityReport {
    /// Set when the trace's event schema predates this binary: the
    /// recorded census came from an older compiler generation, so the
    /// census and prune comparisons are not assertable (turn structure
    /// still is — the transcript shape is stable across schemas).
    pub schema_skew: Option<(u16, u16)>,
    /// `None` when no ground truth was available to compare.
    pub turns_match: Option<bool>,
    /// Per-source census comparison (exact equality required on
    /// transcript-derived sources).
    pub census_match: Option<bool>,
    /// Prune-event comparison (freeze turns, triggers, reclaimed totals).
    pub prunes_match: Option<bool>,
    /// Overall verdict; `false` makes every downstream number untrusted.
    pub valid: bool,
    /// Human-readable deltas for anything that did not match.
    pub notes: Vec<String>,
}

impl ValidityReport {
    /// Compares a replay arm against a [`TraceInput`]'s ground truth.
    ///
    /// Transcript-derived census sources must match exactly; `total` and
    /// the reconstructed prefix sources (system prompt, facts, tool
    /// specs) are informational because traces do not record them.
    #[must_use]
    pub fn check(trace: &TraceInput, replay: &ReplaySummary) -> Self {
        let mut notes = Vec::new();
        let binary_schema = loom_kernel::event::EVENT_SCHEMA_VERSION;
        let schema_skew = if trace.schema_version == binary_schema {
            None
        } else {
            notes.push(format!(
                "trace recorded under event schema {} but this binary is {}: the \
                 recorded census came from an older compiler generation, so census \
                 and prune comparisons are not assertable (record a fresh trace to \
                 validate); turn structure is still compared",
                trace.schema_version, binary_schema
            ));
            Some((trace.schema_version, binary_schema))
        };
        let turns_match = trace.summary.as_ref().map(|summary| {
            let recorded = summary["turns"].as_u64().unwrap_or_default() as u32;
            let ok = replay.turns.len() as u32 == recorded;
            if !ok {
                notes.push(format!(
                    "turns: replay {} vs recorded {recorded}",
                    replay.turns.len()
                ));
            }
            ok
        });
        let census_match = if schema_skew.is_some() {
            None
        } else {
            trace.summary.as_ref().map(|summary| {
                let recorded = &summary["last_context_census"];
                let Some(last) = replay.turns.last() else {
                    notes.push("census: replay produced no turns".into());
                    return false;
                };
                let fields = [
                    ("user_items", last.census.user_items),
                    ("assistant_history", last.census.assistant_history),
                    ("reasoning_replay", last.census.reasoning_replay),
                    ("tool_results", last.census.tool_results),
                ];
                let mut ok = true;
                for (name, value) in fields {
                    let recorded_value = recorded[name].as_u64().unwrap_or_default() as u32;
                    if recorded_value != value {
                        ok = false;
                        notes.push(format!(
                            "census.{name}: replay {value} vs recorded {recorded_value}"
                        ));
                    }
                }
                ok
            })
        };
        let prunes_match =
            if trace.prune_events.is_empty() && replay.freezes == 0 || schema_skew.is_some() {
                None
            } else {
                let mut ok = true;
                let freezes: Vec<(u32, &PruneReport)> = replay
                    .turns
                    .iter()
                    .filter_map(|turn| turn.froze.as_ref().map(|report| (turn.turn, report)))
                    .collect();
                if freezes.len() != trace.prune_events.len() {
                    ok = false;
                    notes.push(format!(
                        "prune events: replay {} vs recorded {}",
                        freezes.len(),
                        trace.prune_events.len()
                    ));
                } else {
                    for ((replay_turn, replay_report), (event_turn, event_report)) in
                        freezes.iter().zip(&trace.prune_events)
                    {
                        if replay_turn != event_turn
                            || replay_report.trigger != event_report.trigger
                            || replay_report.reclaimed_tokens != event_report.reclaimed_tokens
                        {
                            ok = false;
                            notes.push(format!(
                                "prune at turn {event_turn}: replay (turn {replay_turn}, {:?}, \
                                 {} tok) vs recorded ({:?}, {} tok)",
                                replay_report.trigger,
                                replay_report.reclaimed_tokens,
                                event_report.trigger,
                                event_report.reclaimed_tokens
                            ));
                        }
                    }
                }
                Some(ok)
            };
        let valid = turns_match.unwrap_or(true)
            && census_match.unwrap_or(true)
            && prunes_match.unwrap_or(true);
        Self {
            schema_skew,
            turns_match,
            census_match,
            prunes_match,
            valid,
            notes,
        }
    }
}

/// Parses `KEY=VAL` overlay pairs into a config on top of `base`.
///
/// Accepted keys are the five selfopt prune seams (by `LOOM_*` env name or
/// bare name) plus replay-only compiler knobs selfopt does not env-sweep
/// (`age_mask_turns`, `elide_turns`, `elide_reasoning`,
/// `elide_assistant_text`). Optional knobs take `off` (or `0`) to
/// disable. Unknown keys are an error — a silently ignored key would make
/// every delta a lie.
pub fn overlay_config(
    base: PruneConfig,
    pairs: &[String],
) -> Result<(PruneConfig, Vec<String>), String> {
    let mut config = base;
    let mut applied = Vec::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("overlay `{pair}` is not KEY=VAL"));
        };
        let key = key.trim();
        let value = value.trim();
        let bare = key.strip_prefix("LOOM_").unwrap_or(key).to_lowercase();
        match bare.as_str() {
            "prune_trigger_percent" => config.trigger_percent = positive(value, key)?,
            "prune_floor_tokens" => config.floor_tokens = positive(value, key)?,
            "prune_min_reclaim_tokens" => config.min_reclaim_tokens = positive(value, key)?,
            "prune_tail_tokens" => config.protected_tail_tokens = positive(value, key)?,
            "phase_mask_turns" => config.phase_mask_turns = optional(value, key)?,
            "age_mask_turns" => config.age_mask_turns = optional(value, key)?,
            "elide_turns" => config.elide_turns = optional(value, key)?,
            "elide_reasoning" => config.elide_reasoning = optional(value, key)?,
            "elide_assistant_text" => config.elide_assistant_text = optional(value, key)?,
            _ => {
                return Err(format!(
                    "unknown overlay key `{key}` (known: LOOM_PRUNE_TRIGGER_PERCENT, \
                     LOOM_PRUNE_FLOOR_TOKENS, LOOM_PRUNE_MIN_RECLAIM_TOKENS, \
                     LOOM_PRUNE_TAIL_TOKENS, LOOM_PHASE_MASK_TURNS, LOOM_AGE_MASK_TURNS, \
                     LOOM_ELIDE_TURNS, LOOM_ELIDE_REASONING, LOOM_ELIDE_ASSISTANT_TEXT)"
                ));
            }
        }
        applied.push(format!("{key}={value}"));
    }
    Ok((config, applied))
}

/// Whether an overlay touches any key that changes the compiled
/// projection (selfopt budget knobs do not — the recorded transcript is
/// fixed, so budgets cannot change a replay).
#[must_use]
pub fn overlay_affects_projection(pairs: &[String]) -> bool {
    pairs.iter().any(|pair| {
        let key = pair.split_once('=').map_or(pair.as_str(), |(key, _)| key);
        let bare = key
            .trim()
            .strip_prefix("LOOM_")
            .unwrap_or(key.trim())
            .to_lowercase();
        matches!(
            bare.as_str(),
            "prune_trigger_percent"
                | "prune_floor_tokens"
                | "prune_min_reclaim_tokens"
                | "prune_tail_tokens"
                | "phase_mask_turns"
                | "age_mask_turns"
                | "elide_turns"
                | "elide_reasoning"
                | "elide_assistant_text"
        )
    })
}

/// The projected median input-token improvement of an overlay over a
/// baseline, in percent — the same statistic the selfopt adoption gate
/// computes on billed usage. Positive means the overlay is cheaper.
#[must_use]
pub fn projected_improvement_percent(baseline: &ReplaySummary, overlay: &ReplaySummary) -> f64 {
    if baseline.median_input_tokens == 0.0 {
        return 0.0;
    }
    (baseline.median_input_tokens - overlay.median_input_tokens) / baseline.median_input_tokens
        * 100.0
}

/// Median of a slice (0.0 empty); mirrors the selfopt helper without
/// taking a loom-eval dependency (dependency direction is one-way).
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Parses a strictly positive integer overlay value.
fn positive(value: &str, key: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|error| format!("overlay `{key}`: `{value}` is not a number ({error})"))
        .and_then(|parsed| {
            if parsed == 0 {
                Err(format!("overlay `{key}` must be > 0"))
            } else {
                Ok(parsed)
            }
        })
}

/// Parses an optional-window overlay value: a number turns the window on
/// at that size, `off` (or `0`) disables it.
fn optional(value: &str, key: &str) -> Result<Option<usize>, String> {
    if value.eq_ignore_ascii_case("off") || value == "0" {
        return Ok(None);
    }
    value.parse::<usize>().map(Some).map_err(|error| {
        format!("overlay `{key}`: `{value}` is neither a number nor `off` ({error})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use loom_kernel::agent::{Agent, TaskControl, TaskSummary};
    use loom_kernel::event::FanoutSink;
    use loom_kernel::id::{SessionId, TaskId};
    use loom_kernel::model::{
        ModelError, ModelEvent, ModelProvider, ModelStream, StopReason, Usage,
    };
    use loom_kernel::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Replays one scripted event list per `stream()` call, same shape as
    /// the worktrees tests' fake.
    struct ScriptedProvider {
        scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    }

    impl ScriptedProvider {
        fn scripted(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
            Self {
                scripts: Mutex::new(VecDeque::from(scripts)),
            }
        }

        fn read_call(call_id: &str, path: &str) -> Vec<Result<ModelEvent, ModelError>> {
            let arguments = ToolArguments::try_from(serde_json::json!({ "path": path }))
                .expect("object arguments");
            vec![Ok(ModelEvent::Completed(
                loom_kernel::model::ModelResponse {
                    message: AssistantMessage {
                        content: format!("reading {path}"),
                        reasoning: None,
                        tool_calls: vec![ToolCall {
                            id: ToolCallId::try_from(call_id.to_owned()).expect("non-empty"),
                            name: "read_file".to_owned(),
                            arguments,
                        }],
                        stop_reason: StopReason::ToolCalls,
                    },
                    usage: Usage::default(),
                },
            ))]
        }

        fn end_turn(text: &str) -> Vec<Result<ModelEvent, ModelError>> {
            vec![Ok(ModelEvent::Completed(
                loom_kernel::model::ModelResponse {
                    message: AssistantMessage {
                        content: text.to_owned(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        stop_reason: StopReason::EndTurn,
                    },
                    usage: Usage::default(),
                },
            ))]
        }
    }

    impl ModelProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(16_384).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, _request: &'a loom_kernel::model::ModelRequest) -> ModelStream<'a> {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(stream::iter(script))
        }
    }

    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    /// A workspace with a ~30 KB text file: ~10K heuristic tokens, enough
    /// to cross a small-floor trigger under an 8192 window.
    fn big_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-replay-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "lorem ipsum dolor sit amet\n".repeat(400);
        std::fs::write(dir.join("big.txt"), body).unwrap();
        dir
    }

    /// Drives a real Agent (real registry, real read tool) over a
    /// scripted provider; returns the advanced session and summary.
    async fn scripted_session(
        workspace: &Path,
        script: Vec<Vec<Result<ModelEvent, ModelError>>>,
        config: PruneConfig,
    ) -> (Session, TaskSummary) {
        let provider = ScriptedProvider::scripted(script);
        let estimator = estimate::estimator_for("scripted");
        let compiler: Box<dyn loom_kernel::context::ContextCompiler> = Box::new(
            PruningContextCompiler::new(
                Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
                config,
            )
            .with_estimator(estimator),
        );
        let agent = Agent::new(
            Box::new(provider),
            compiler,
            crate::engine::replay_registry().expect("registry builds"),
            AgentConfig::new(workspace.to_path_buf(), "system".to_owned()),
            "test-model",
        )
        .with_max_turns(10)
        .with_reserved_output_tokens(1_024);
        let mut session = Session::new(SessionId::new());
        let sink = FanoutSink::new();
        let summary = agent
            .run_task(
                &mut session,
                "read big.txt and report the first line",
                &sink,
                &TaskControl::default(),
            )
            .await
            .expect("scripted task runs");
        (session, summary)
    }

    fn replay_options(workspace: &Path) -> ReplayOptions {
        ReplayOptions {
            provider: "scripted".to_owned(),
            window_tokens: 16_384,
            reserved_output_tokens: 1_024,
            system_prompt: "system".to_owned(),
            workspace_root: workspace.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn replay_reproduces_recorded_census_and_prunes() {
        let workspace = big_workspace("main");
        let config = PruneConfig {
            trigger_percent: 30,
            floor_tokens: 5_000,
            min_reclaim_tokens: 100,
            protected_tail_tokens: 200,
            protected_tail_turns: 1,
            ..PruneConfig::default()
        };
        let script = vec![
            ScriptedProvider::read_call("c1", "big.txt"),
            ScriptedProvider::read_call("c2", "big.txt"),
            ScriptedProvider::read_call("c3", "big.txt"),
            ScriptedProvider::end_turn("done"),
        ];
        let (session, summary) = scripted_session(&workspace, script, config).await;
        assert!(summary.turns >= 3, "fixture must run several turns");
        let options = replay_options(&workspace);
        let replay = replay_session(&session, session.tasks.last().unwrap(), &options, config)
            .expect("replay runs");
        assert_eq!(
            replay.turns.len() as u32,
            summary.turns,
            "one compile per live model turn"
        );
        let last = replay.turns.last().expect("turns exist");
        assert_eq!(
            last.census.user_items,
            summary.last_context_census.user_items
        );
        assert_eq!(
            last.census.assistant_history,
            summary.last_context_census.assistant_history
        );
        assert_eq!(
            last.census.tool_results,
            summary.last_context_census.tool_results
        );
        assert!(replay.freezes >= 1, "fixture config should freeze");
        // Stability dips may not exceed freezes: one cache miss per
        // trigger, byte-stable between events.
        let dips: Vec<(u32, f64)> = replay
            .turns
            .iter()
            .filter_map(|turn| turn.prefix_stability.map(|ratio| (turn.turn, ratio)))
            .filter(|(_, ratio)| *ratio < 0.999)
            .collect();
        assert!(
            dips.len() as u64 <= replay.freezes,
            "stability dips ({dips:?}) may not exceed freezes ({})\nfull arm: {:?}",
            replay.freezes,
            replay
                .turns
                .iter()
                .map(|turn| (turn.turn, turn.input_tokens, turn.prefix_stability))
                .collect::<Vec<_>>()
        );
        // Deterministic: same input, same output.
        let replay2 = replay_session(&session, session.tasks.last().unwrap(), &options, config)
            .expect("replay reruns");
        assert_eq!(replay, replay2, "replay is deterministic");
        // Full validity: a same-schema trace carrying the LIVE run's
        // recorded summary must check out end to end — the required
        // self-validity contract (census from the live TaskSummary, not
        // from the replay itself).
        let census = summary.last_context_census;
        let trace = TraceInput {
            session: session.clone(),
            task: session.tasks.last().unwrap().clone(),
            provider: "scripted".into(),
            model: "test".into(),
            schema_version: loom_kernel::event::EVENT_SCHEMA_VERSION,
            summary: Some(serde_json::json!({
                "turns": summary.turns,
                "last_context_census": {
                    "user_items": census.user_items,
                    "assistant_history": census.assistant_history,
                    "reasoning_replay": census.reasoning_replay,
                    "tool_results": census.tool_results,
                },
            })),
            prune_events: replay
                .turns
                .iter()
                .filter_map(|turn| {
                    turn.froze
                        .as_ref()
                        .map(|report| (turn.turn, report.clone()))
                })
                .collect(),
        };
        let report = ValidityReport::check(&trace, &replay);
        assert!(report.valid, "notes: {:?}", report.notes);
        assert_eq!(report.schema_skew, None);
        assert_eq!(report.census_match, Some(true));
        assert_eq!(report.prunes_match, Some(true));
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn overlay_stubs_more_than_baseline() {
        let workspace = big_workspace("overlay");
        // The live run's own config: default trigger, but a tail short
        // enough that rescue can actually stub when the window fills —
        // the shipped 20K-token tail would protect this whole fixture
        // and the live run would (correctly) fail closed.
        let live = PruneConfig {
            protected_tail_turns: 1,
            protected_tail_tokens: 200,
            ..PruneConfig::default()
        };
        let script = vec![
            ScriptedProvider::read_call("c1", "big.txt"),
            ScriptedProvider::read_call("c2", "big.txt"),
            ScriptedProvider::read_call("c3", "big.txt"),
            ScriptedProvider::end_turn("done"),
        ];
        let (session, _summary) = scripted_session(&workspace, script, live).await;
        let options = replay_options(&workspace);
        let baseline = replay_session(&session, session.tasks.last().unwrap(), &options, live)
            .expect("baseline replay");
        let (aggressive, applied) = overlay_config(
            live,
            &[
                "LOOM_PRUNE_TRIGGER_PERCENT=10".to_owned(),
                "LOOM_PRUNE_FLOOR_TOKENS=100".to_owned(),
                "LOOM_PRUNE_MIN_RECLAIM_TOKENS=1".to_owned(),
                "LOOM_AGE_MASK_TURNS=1".to_owned(),
            ],
        )
        .expect("valid overlay");
        assert_eq!(applied.len(), 4);
        let overlay = replay_session(
            &session,
            session.tasks.last().unwrap(),
            &options,
            aggressive,
        )
        .expect("overlay replay");
        assert!(
            overlay.total_input_tokens < baseline.total_input_tokens,
            "aggressive overlay ({}) must project cheaper than baseline ({})",
            overlay.total_input_tokens,
            baseline.total_input_tokens
        );
        assert!(projected_improvement_percent(&baseline, &overlay) > 0.0);
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[tokio::test]
    async fn handoff_boundary_stops_the_arm() {
        let workspace = std::env::temp_dir().join(format!("loom-replay-ho-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        // Non-prunable history: the *last* user item is exempt from
        // elision by construction, so one huge task text under a tiny
        // window overruns past rescue.
        let mut session = Session::new(SessionId::new());
        let mut task = TaskRecord::new(TaskId::new(), now_millis());
        task.turns = 1; // a model ran; only fast-path tasks have zero
        session.tasks = vec![task.clone()];
        session
            .push_item(Item::User {
                text: format!("task: {}", "details ".repeat(6_000)),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(AssistantMessage {
                content: "ok".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            }))
            .unwrap();
        let options = ReplayOptions {
            provider: "scripted".into(),
            window_tokens: 4_096,
            reserved_output_tokens: 512,
            system_prompt: "system".into(),
            workspace_root: workspace.clone(),
        };
        let replay =
            replay_session(&session, &task, &options, PruneConfig::default()).expect("runs");
        assert!(
            replay.handoff_stopped_at.is_some(),
            "unfittable history must stop the arm at a handoff boundary"
        );
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn fast_path_session_replays_zero_turns() {
        let mut session = Session::new(SessionId::new());
        let mut task = TaskRecord::new(TaskId::new(), now_millis());
        task.turns = 0;
        task.finish(
            loom_kernel::session::TerminalTaskStatus::Completed,
            now_millis(),
            "fast_path".to_owned(),
        );
        session.tasks = vec![task.clone()];
        session
            .push_item(Item::User {
                text: "rename x to y".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(AssistantMessage {
                content: "renamed".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            }))
            .unwrap();
        let options = ReplayOptions::new("scripted");
        let replay = replay_session(&session, &task, &options, PruneConfig::default()).unwrap();
        assert!(replay.turns.is_empty());
        assert_eq!(replay.total_input_tokens, 0);
    }

    #[test]
    fn overlay_keys_validate() {
        let (config, applied) = overlay_config(
            PruneConfig::default(),
            &[
                "LOOM_PRUNE_TRIGGER_PERCENT=50".to_owned(),
                "age_mask_turns=off".to_owned(),
            ],
        )
        .expect("valid");
        assert_eq!(config.trigger_percent, 50);
        assert_eq!(config.age_mask_turns, None);
        assert_eq!(applied.len(), 2);
        assert!(overlay_config(PruneConfig::default(), &["LOOM_BOGUS=1".into()]).is_err());
        assert!(overlay_config(PruneConfig::default(), &["noequalsign".into()]).is_err());
        assert!(
            overlay_config(
                PruneConfig::default(),
                &["LOOM_PRUNE_TRIGGER_PERCENT=zero".into()]
            )
            .is_err()
        );
        assert!(overlay_affects_projection(&[
            "LOOM_PRUNE_FLOOR_TOKENS=1".into()
        ]));
        assert!(!overlay_affects_projection(&[
            "LOOM_READ_HEAD_BYTES=4096".into()
        ]));
    }

    #[test]
    fn validity_flags_mismatch() {
        let mut session = Session::new(SessionId::new());
        let task = TaskRecord::new(TaskId::new(), now_millis());
        session.tasks = vec![task.clone()];
        session
            .push_item(Item::User {
                text: "hello".into(),
            })
            .unwrap();
        session
            .push_item(Item::Assistant(AssistantMessage {
                content: "hi".into(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            }))
            .unwrap();
        let options = ReplayOptions::new("scripted");
        let replay = replay_session(&session, &task, &options, PruneConfig::default()).unwrap();
        let trace = TraceInput {
            session: session.clone(),
            task: task.clone(),
            provider: "scripted".into(),
            model: "m".into(),
            schema_version: loom_kernel::event::EVENT_SCHEMA_VERSION,
            summary: Some(serde_json::json!({
                "turns": 7, // wrong on purpose
                "last_context_census": {},
            })),
            prune_events: Vec::new(),
        };
        let report = ValidityReport::check(&trace, &replay);
        assert!(!report.valid);
        assert_eq!(report.turns_match, Some(false));
        assert!(report.notes.iter().any(|note| note.contains("turns")));
    }

    /// The real smoke trace recorded under `traces/smoke/`: replay must
    /// reproduce the recorded turns and pass self-validity. Skipped
    /// silently when the checkout carries no trace.
    #[test]
    fn smoke_trace_parses_and_validates() {
        // Absolute from CARGO_MANIFEST_DIR: a relative path silently
        // skipped the whole test whenever the harness CWD moved.
        let smoke_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../traces/smoke");
        let Ok(entries) = std::fs::read_dir(&smoke_dir) else {
            panic!("smoke trace dir missing: {}", smoke_dir.display());
        };
        for entry in entries.flatten() {
            let trace = TraceInput::read(&entry.path()).expect("smoke trace parses");
            let options = ReplayOptions::new(&trace.provider);
            let replay = replay_session(
                &trace.session,
                &trace.task,
                &options,
                PruneConfig::default(),
            )
            .expect("smoke trace replays");
            if let Some(summary) = &trace.summary {
                let recorded = summary["turns"].as_u64().unwrap_or_default() as u32;
                assert_eq!(
                    replay.turns.len() as u32,
                    recorded,
                    "smoke trace turns must line up"
                );
            }
            let validity = ValidityReport::check(&trace, &replay);
            if validity.schema_skew.is_some() {
                // The checked-in trace predates this binary's schema, so
                // census equality is not assertable — but turns must still
                // line up and the skew must be named, never silent.
                assert!(validity.census_match.is_none());
                assert!(validity.prunes_match.is_none());
                assert!(
                    validity.notes.iter().any(|note| note.contains("schema")),
                    "skew must be explained in the notes: {:?}",
                    validity.notes
                );
            } else {
                assert!(validity.valid, "validity notes: {:?}", validity.notes);
            }
        }
    }
}
