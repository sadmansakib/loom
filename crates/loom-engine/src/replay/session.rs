//! The replay itself: every assistant-item prefix of the session is
//! compiled under the record's prune config, measuring input tokens,
//! census, prune freezes, and prefix stability
//! (docs/replay-context-design.md).

use loom_kernel::context::{
    AgentConfig, ContextBudget, ContextCompiler, ContextError, ContextInput,
    DefaultContextCompiler, KnownFacts, PruneCheckpoint, PruneConfig, PruningContextCompiler,
};
use loom_kernel::model::ModelCapabilities;
use loom_kernel::session::{Session, TaskRecord};
use loom_kernel::tool::ToolRegistry;
use loom_kernel::transcript::Item;

use crate::estimate;

use super::options::{ReplayError, ReplayOptions, ReplaySummary, TurnReplay};
use super::overlay::median;

/// Replays one session under one config. The session is compiled at every
/// prefix that ends right before an assistant item — exactly where the
/// live loop compiles. Fast-path tasks (zero model turns) contribute no
/// compile. A projection that overruns past rescue stops the arm with
/// [`ReplaySummary::handoff_stopped_at`] set: the live loop's handoff
/// needs one model call, which replay refuses to fake.
///
/// # Errors
///
/// Returns [`ReplayError::Budget`] when the replay registry or the
/// context budget cannot be constructed, and
/// [`ReplayError::InvalidTrace`] when a prefix compile fails other than
/// by exceeding the budget.
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
    let compiler = PruningContextCompiler::new(
        Box::new(DefaultContextCompiler::with_estimator(estimator.clone())),
        config,
    )
    .with_estimator(estimator);

    // Fast-path sessions never compile; the booked assistant item is not
    // a model turn.
    let skip_all = session.tasks.last().is_some_and(|task| task.turns == 0);
    let (turns, handoff_stopped_at) = replay_turns(
        session,
        task,
        &registry,
        &compiler,
        budget,
        &capabilities,
        &agent_config,
        skip_all,
    )?;

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

/// The replay sweep behind [`replay_session`]: compiles the session at
/// every assistant-item prefix (the same points the live loop compiles
/// at), measures census / freeze / prefix stability per turn, and stops
/// honestly at a budget overrun with the would-be handoff turn.
#[allow(clippy::too_many_arguments)] // mirrors the live loop's compile inputs one-for-one
fn replay_turns(
    session: &Session,
    task: &TaskRecord,
    registry: &ToolRegistry,
    compiler: &PruningContextCompiler,
    budget: ContextBudget,
    capabilities: &ModelCapabilities,
    agent_config: &AgentConfig,
    skip_all: bool,
) -> Result<(Vec<TurnReplay>, Option<u32>), ReplayError> {
    let facts = KnownFacts::default();
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
                    registry,
                    task,
                    budget,
                    capabilities,
                    "replay",
                    agent_config,
                    &facts,
                    checkpoint.as_ref(),
                );
                match compiler.compile(input) {
                    Ok(projected) => {
                        checkpoint = projected.prune_checkpoint;
                        // Serialize each message separately and concatenate:
                        // the whole-array JSON form ends in `]`, which would
                        // break the byte-prefix property on every append.
                        let bytes = projected
                            .request
                            .messages
                            .iter()
                            .filter_map(|message| serde_json::to_vec(message).ok())
                            .collect::<Vec<_>>()
                            .concat();
                        #[allow(clippy::cast_precision_loss)]
                        // advisory metric; f64 precision intended
                        let prefix_stability = previous_bytes.as_ref().map(|prev| {
                            common_prefix_len(prev, &bytes) as f64 / prev.len().max(1) as f64
                        });
                        previous_bytes = Some(bytes);
                        #[allow(clippy::cast_possible_truncation)]
                        // turn counts stay far below u32::MAX
                        let turn_number = turns.len() as u32 + 1;
                        turns.push(TurnReplay {
                            turn: turn_number,
                            input_tokens: projected.census.total,
                            census: projected.census,
                            froze: projected.prune_report,
                            prefix_stability,
                            stubbed_calls: projected.stubbed_calls.len(),
                        });
                    }
                    Err(ContextError::BudgetExceeded { .. }) => {
                        // The live loop would attempt an LLM handoff here;
                        // the handoff summary is a model call replay cannot
                        // make. Stop honestly.
                        #[allow(clippy::cast_possible_truncation)]
                        // turn counts stay far below u32::MAX
                        let stopped_at = turns.len() as u32 + 1;
                        handoff_stopped_at = Some(stopped_at);
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
    Ok((turns, handoff_stopped_at))
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}
