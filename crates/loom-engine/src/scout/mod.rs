//! The read-only scout (roadmap item 19, iteration 3).
//!
//! One delegation level, bounded hard: a cheap model answers a capsule
//! question over the workspace with read-only tools and returns a
//! ≤1 KB finding. The parent's context pays only the capsule and the
//! answer — the exploration the scout performed never enters it.
//!
//! Multi-model collaboration is layered on that spine
//! (docs/subagents-design.md): a deterministic weak-answer gate
//! escalates once to a stronger model (FrugalGPT-shaped cascade, code
//! heuristics instead of a learned scorer); `cross_check` runs both
//! models and reports citation agreement; `questions` fans out up to
//! three capsules in parallel. Every answer carries a model/turns
//! provenance footer — delegation spend stays visible. Depth stays at
//! one level: nested subagents compound lossy compression (Cognition
//! 2025; Anthropic's research system keeps one level too).
//!
//! The runtime ships the seam and the bounds; the delegation economics
//! (when answering locally would cost more parent context than the
//! child spend) are measured offline by the eval, per the roadmap.
//! Recursion is structurally impossible: the child registry is the
//! read-only subset of the parent's, and `scout` itself is not
//! read-only-exempt — it is simply never registered for the child.

mod capsule;
mod config;
mod gate;
mod run;
mod tool;

pub use capsule::{capsule, final_answer};
pub use config::{ANSWER_BUDGET_BYTES, SCOUT_CONCURRENCY, ScoutConfig, ScoutEscalation};
pub use tool::ScoutTool;

#[cfg(test)]
use capsule::truncate_answer;
#[cfg(test)]
use gate::{agreement_note, cited_pointers, has_file_line_pointer, weak_reason};
#[cfg(test)]
use loom_kernel::agent::Agent;
#[cfg(test)]
use loom_kernel::tool::Tool;
#[cfg(test)]
use loom_kernel::tool::ToolContext;
#[cfg(test)]
use run::{ScoutOutcome, Status};
#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use loom_kernel::context::{AgentConfig, DefaultContextCompiler};
    use loom_kernel::model::{
        ModelCapabilities, ModelEvent, ModelProvider, ModelRequest, ModelResponse, StopReason,
        Usage,
    };
    use loom_kernel::transcript::AssistantMessage;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A provider that answers once with fixed text and records every
    /// request it saw into a shared log, so tests can assert the
    /// capsule reached the child.
    struct AnsweringProvider {
        answer: String,
        requests: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl ModelProvider for AnsweringProvider {
        fn name(&self) -> &'static str {
            "answering"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, request: &'a ModelRequest) -> loom_kernel::model::ModelStream<'a> {
            let seen = request
                .messages
                .iter()
                .filter_map(|m| match m {
                    loom_kernel::model::Message::User { text } => Some(text.clone()),
                    loom_kernel::model::Message::System { .. }
                    | loom_kernel::model::Message::Assistant(_)
                    | loom_kernel::model::Message::ToolResult(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.requests.lock().unwrap().push(seen);
            let event = ModelEvent::Completed(ModelResponse {
                message: AssistantMessage {
                    content: self.answer.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                },
                usage: Usage::default(),
            });
            Box::pin(stream::iter(vec![Ok(event)]))
        }
    }

    /// A provider that never yields an event: forces the wall-clock and
    /// cancellation races to decide.
    struct HangingProvider;

    impl ModelProvider for HangingProvider {
        fn name(&self) -> &'static str {
            "hanging"
        }

        fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::new(8192).with_tool_calls(true))
        }

        fn stream<'a>(&'a self, _request: &'a ModelRequest) -> loom_kernel::model::ModelStream<'a> {
            Box::pin(stream::pending())
        }
    }

    fn scout_tool(provider: impl ModelProvider + 'static, timeout_secs: u64) -> ScoutTool {
        let agent = Agent::new(
            Box::new(provider),
            Box::new(DefaultContextCompiler::default()),
            loom_kernel::tool::ToolRegistry::new(),
            AgentConfig::new(std::path::PathBuf::from("."), "scout test"),
            "cheap-model",
        )
        .with_loop_shaping(false);
        ScoutTool::new(std::sync::Arc::new(agent), timeout_secs)
    }

    /// A scout with a strong escalation agent wired (`with_escalation`).
    fn escalation_tool(
        primary: impl ModelProvider + 'static,
        strong: impl ModelProvider + 'static,
        timeout_secs: u64,
    ) -> ScoutTool {
        let make = |provider: Box<dyn ModelProvider>| {
            Agent::new(
                provider,
                Box::new(DefaultContextCompiler::default()),
                loom_kernel::tool::ToolRegistry::new(),
                AgentConfig::new(std::path::PathBuf::from("."), "scout test"),
                "model",
            )
            .with_loop_shaping(false)
        };
        ScoutTool::new(std::sync::Arc::new(make(Box::new(primary))), timeout_secs)
            .with_model_label("cheap-model")
            .with_escalation(std::sync::Arc::new(make(Box::new(strong))), "strong-model")
    }

    fn tool_ctx(cancellation: &tokio_util::sync::CancellationToken) -> ToolContext {
        ToolContext::new(
            std::path::PathBuf::from("."),
            cancellation.clone(),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn capsule_carries_question_and_focus() {
        let prompt = capsule("where is parse_opts?", Some("crates/loom-kernel"));
        assert!(prompt.contains("where is parse_opts?"));
        assert!(prompt.contains("crates/loom-kernel"));
    }

    #[test]
    fn truncate_answer_bounds_long_answers() {
        let short = "src/lib.rs:12";
        assert_eq!(truncate_answer(short), short);
        let long = "x".repeat(5_000);
        let bounded = truncate_answer(&long);
        assert!(bounded.contains("truncated at 1 KB"));
        assert!(bounded.len() <= ANSWER_BUDGET_BYTES + 40);
        assert!(bounded.starts_with('x'));
    }

    #[tokio::test]
    async fn scout_returns_the_child_answer_and_the_capsule_reaches_it() {
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let provider = AnsweringProvider {
            answer: "parse_opts lives in src/lib.rs:12".to_owned(),
            requests: requests.clone(),
        };
        let tool = scout_tool(provider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "where is parse_opts?"}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("src/lib.rs:12"));
        let log = requests.lock().unwrap();
        assert!(
            log.iter().any(|r| r.contains("where is parse_opts?")),
            "the capsule must reach the child model"
        );
    }

    #[tokio::test]
    async fn scout_times_out_and_says_so() {
        let tool = scout_tool(HangingProvider, 1);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "anything"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("timed out"), "{}", output);
    }

    #[tokio::test]
    async fn scout_stops_when_the_parent_cancels() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        token.cancel();
        let output = tool
            .call(serde_json::json!({"question": "anything"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("cancel"), "{}", output);
    }

    #[tokio::test]
    async fn scout_rejects_an_empty_question() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "  "}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("non-empty"), "{}", output);
    }

    #[tokio::test]
    async fn scout_fails_soft_beyond_the_concurrency_cap() {
        // One permit: the in-flight scout holds it until its 2 s timeout.
        let tool = std::sync::Arc::new(scout_tool(HangingProvider, 2).with_concurrency(1));
        let first_tool = tool.clone();
        let first = tokio::spawn(async move {
            let token = tokio_util::sync::CancellationToken::new();
            let ctx = tool_ctx(&token);
            first_tool
                .call(serde_json::json!({"question": "slow"}), &ctx)
                .await
                .expect("scout call")
                .content
        });
        // Let the first scout acquire its permit before the second asks.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let second = tool
            .call(serde_json::json!({"question": "blocked"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(second.contains("at capacity"), "{}", second);
        let first = first.await.expect("join");
        assert!(first.contains("timed out"), "{}", first);
    }

    #[test]
    fn pointer_detection_and_extraction() {
        assert!(has_file_line_pointer("see src/lib.rs:12 for it"));
        assert!(has_file_line_pointer("engine.rs:40 says so."));
        assert!(!has_file_line_pointer("no pointers here"));
        assert!(!has_file_line_pointer("time 12:30 was odd")); // colon not path-shaped
        let pointers = cited_pointers("a.rs:1 and b/c.rs:22, plus d.rs:333.");
        assert_eq!(pointers, vec!["a.rs:1", "b/c.rs:22", "d.rs:333"]);
    }

    #[test]
    fn agreement_note_reports_shared_and_divergent_citations() {
        let note = agreement_note(
            "core in src/lib.rs:12 and extra.rs:9",
            "core in src/lib.rs:12, other.rs:4",
        );
        assert!(note.contains("both cite src/lib.rs:12"), "{note}");
        assert!(
            note.contains("only the primary model cites extra.rs:9"),
            "{note}"
        );
        assert!(
            note.contains("only the strong model cites other.rs:4"),
            "{note}"
        );
    }

    #[test]
    fn weak_reason_flags_the_contract_violations() {
        let answered = |answer: &str| ScoutOutcome {
            answer: answer.to_owned(),
            status: Status::Answered,
            turns: 1,
        };
        assert!(weak_reason(&answered("it is in src/lib.rs:12")).is_none());
        assert_eq!(
            weak_reason(&answered("no idea, sorry")),
            Some("missing the file:line pointers the capsule contract requires")
        );
        assert_eq!(weak_reason(&answered("")), Some("an empty answer"));
        let truncated = ScoutOutcome {
            answer: format!("{}\n[…scout answer truncated at 1 KB]", "x".repeat(1_100)),
            status: Status::Answered,
            turns: 1,
        };
        assert_eq!(
            weak_reason(&truncated),
            Some("truncated at the answer budget")
        );
        let timed_out = ScoutOutcome {
            answer: String::new(),
            status: Status::TimedOut,
            turns: 0,
        };
        assert_eq!(weak_reason(&timed_out), Some("timed out"));
    }

    #[tokio::test]
    async fn weak_answers_escalate_to_the_strong_model() {
        let primary_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let strong_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "i could not find it".to_owned(), // no pointers → weak
                requests: primary_requests.clone(),
            },
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: strong_requests.clone(),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "where is parse_opts?"}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("escalated to strong-model"), "{}", output);
        assert!(
            output.contains("missing the file:line pointers"),
            "{}",
            output
        );
        assert!(output.contains("src/lib.rs:12"), "{}", output);
        // Both models saw the same capsule.
        assert!(
            strong_requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("where is parse_opts?"))
        );
    }

    #[tokio::test]
    async fn strong_answers_do_not_escalate() {
        let strong_requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            AnsweringProvider {
                answer: "should never run".to_owned(),
                requests: strong_requests.clone(),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "where?"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(!output.contains("escalated"), "{}", output);
        assert!(strong_requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cross_check_runs_both_models_and_reports_agreement() {
        let tool = escalation_tool(
            AnsweringProvider {
                answer: "entry at src/main.rs:10".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            AnsweringProvider {
                answer: "entry at src/main.rs:10, config in src/config.rs:5".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "entry point?", "cross_check": true}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("cross-check: cheap-model vs strong-model"),
            "{}",
            output
        );
        assert!(output.contains("both cite src/main.rs:10"), "{}", output);
        assert!(
            output.contains("only the strong model cites src/config.rs:5"),
            "{}",
            output
        );
    }

    #[tokio::test]
    async fn cross_check_without_an_escalation_model_says_so() {
        let tool = scout_tool(
            AnsweringProvider {
                answer: "entry at src/main.rs:10".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"question": "entry?", "cross_check": true}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("no escalation model is configured"),
            "{}",
            output
        );
    }

    #[tokio::test]
    async fn questions_fan_out_in_parallel_with_labels() {
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = scout_tool(
            AnsweringProvider {
                answer: "found it at src/lib.rs:12".to_owned(),
                requests: requests.clone(),
            },
            60,
        );
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"questions": ["where is parse_opts?", "where is main?"]}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("[scout 1/2]"), "{}", output);
        assert!(output.contains("[scout 2/2]"), "{}", output);
        assert_eq!(output.matches("found it at src/lib.rs:12").count(), 2);
        // Both capsules reached the child.
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("where is main?"))
        );
    }

    #[tokio::test]
    async fn questions_over_the_cap_fail_soft() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(
                serde_json::json!({"questions": ["a?", "b?", "c?", "d?"]}),
                &ctx,
            )
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("at most 3"), "{}", output);
    }

    #[tokio::test]
    async fn missing_question_input_fails_soft() {
        let tool = scout_tool(HangingProvider, 60);
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"focus": "src"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(output.contains("`question`"), "{}", output);
    }

    #[tokio::test]
    async fn answers_carry_a_provenance_footer() {
        let tool = scout_tool(
            AnsweringProvider {
                answer: "parse_opts lives in src/lib.rs:12".to_owned(),
                requests: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
            60,
        )
        .with_model_label("cheap-model");
        let token = tokio_util::sync::CancellationToken::new();
        let ctx = tool_ctx(&token);
        let output = tool
            .call(serde_json::json!({"question": "where?"}), &ctx)
            .await
            .expect("scout call")
            .content;
        assert!(
            output.contains("(model: cheap-model, 1 turn)"),
            "{}",
            output
        );
    }
}
