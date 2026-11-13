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

mod options;
mod overlay;
mod session;
mod trace;
mod validity;

pub use options::{
    DEFAULT_RESERVED_OUTPUT_TOKENS, DEFAULT_WINDOW_TOKENS, ReplayError, ReplayOptions,
    ReplaySummary, TurnReplay,
};
pub use overlay::{
    median, overlay_affects_projection, overlay_config, projected_improvement_percent,
};
pub use session::replay_session;
pub use trace::TraceInput;
pub use validity::ValidityReport;

#[cfg(test)]
use crate::estimate;
#[cfg(test)]
use loom_kernel::context::{
    AgentConfig, DefaultContextCompiler, PruneConfig, PruningContextCompiler,
};
#[cfg(test)]
use loom_kernel::model::ModelCapabilities;
#[cfg(test)]
use loom_kernel::session::{Session, TaskRecord};
#[cfg(test)]
use loom_kernel::transcript::Item;
#[cfg(test)]
use std::path::{Path, PathBuf};
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
        fn name(&self) -> &'static str {
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

    #[allow(clippy::cast_possible_truncation)] // u128 millis fit u64 until the year ~584 million
    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64)
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
        #[allow(clippy::cast_possible_truncation)] // turn counts stay far below u32::MAX
        let replay_turns = replay.turns.len() as u32;
        assert_eq!(
            replay_turns, summary.turns,
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
        assert_eq!(replay.turns.len(), 0);
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
            task,
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
            // Documented contract: skip (not fail) when the checkout
            // carries no trace fixture.
            println!("skipped: no smoke trace dir at {}", smoke_dir.display());
            return;
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
                #[allow(clippy::cast_possible_truncation)]
                // recorded values are loom-written counts
                let recorded = summary["turns"].as_u64().unwrap_or_default() as u32;
                #[allow(clippy::cast_possible_truncation)] // turn counts stay far below u32::MAX
                let replay_turns = replay.turns.len() as u32;
                assert_eq!(replay_turns, recorded, "smoke trace turns must line up");
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
