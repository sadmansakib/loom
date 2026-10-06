//! Projection rules (§8.3) — the only omissions a [`DefaultContextCompiler`]
//! makes:
//!
//! 1. **`ContentFilter` assistant turns are never replayed.** The item stays
//!    in the transcript (inspectable) but is omitted from every later
//!    projection, alone: the `Item::Assistant` with
//!    `stop_reason = ContentFilter` is dropped, and every other item —
//!    including any tool results that might follow it in the transcript —
//!    projects normally. In practice a refused turn carries no tool calls
//!    (§5.1 normalization), so nothing follows it.
//! 2. **`MaxTokens` assistant turns project verbatim, as text.** They never
//!    carry tool calls (§5.1 normalization), so the projection is exactly
//!    the stored message; the §8.3 rule is documented here because a future
//!    normalization change could otherwise re-introduce tool calls on a
//!    `MaxTokens` item, which must project as text only.
//!
//! Everything else — including all reasoning (§4.2) — is projected verbatim
//! in transcript order. Reasoning is replayed byte-exact because the
//! DeepSeek thinking-mode contract demands the complete, unmodified
//! reasoning of every prior turn once tools are sent (ADR-006).

use super::{
    AgentConfig, CompiledContext, ContextCensus, ContextError, ContextInput, HeuristicEstimator,
    KnownFacts, TokenEstimator, estimate_serialized,
};
use crate::model::{Message, ModelCapabilities, ModelRequest};
use crate::tool::{ToolEffects, ToolRegistry};
use crate::transcript::{Item, ToolResult};
use std::sync::Arc;

/// Full-transcript, all-tools context compiler (§8.3).
///
/// Invariants it upholds (§8.1): the static system prompt is the stable
/// prefix (cached by providers); dynamic history follows it in append
/// order; tool definitions appear once per request in deterministic
/// registration order; and the estimated token count is checked against
/// the budget so a known-overflow request is never sent.
///
/// Token estimates come from the injected [`TokenEstimator`]; the default
/// is the kernel's conservative bytes ÷ 3 heuristic.
pub struct DefaultContextCompiler {
    estimator: Arc<dyn TokenEstimator>,
}

impl Default for DefaultContextCompiler {
    fn default() -> Self {
        Self {
            estimator: Arc::new(HeuristicEstimator),
        }
    }
}

impl DefaultContextCompiler {
    /// Uses `estimator` for every token estimate (census totals and the
    /// budget check that consumes them).
    #[must_use]
    pub fn with_estimator(estimator: Arc<dyn TokenEstimator>) -> Self {
        Self { estimator }
    }
}

impl super::ContextCompiler for DefaultContextCompiler {
    fn compile(&self, input: ContextInput<'_>) -> Result<CompiledContext, ContextError> {
        let (messages, facts_tokens) =
            project_messages(input.session, input.config, input.facts, &*self.estimator);
        let tools = exposed_tools(input.tools, input.capabilities);
        let request = ModelRequest {
            model: input.model.to_owned(),
            messages,
            tools,
            max_tokens: input
                .capabilities
                .max_output_tokens
                .map(|tokens| tokens.min(input.budget.reserved_output_tokens)),
            temperature: None,
        };
        let census = census(&request, facts_tokens, &*self.estimator);
        if census.total > input.budget.max_input_tokens {
            return Err(ContextError::BudgetExceeded {
                estimated: census.total,
                max_input_tokens: input.budget.max_input_tokens,
            });
        }
        Ok(CompiledContext {
            request,
            census,
            prune_checkpoint: None,
        })
    }
}

/// Projects the session transcript into model messages (§8.3).
///
/// The system prompt leads (the stable prefix), then the repository facts
/// block when one was captured (2E), then every transcript item in append
/// order except `ContentFilter` assistant turns, which stay in the transcript
/// but are never replayed.
///
/// Returns the messages and the estimated tokens of the facts message
/// (0 when absent), so the census can attribute it apart from the system
/// prompt without re-parsing the projection.
fn project_messages(
    session: &crate::session::Session,
    config: &AgentConfig,
    facts: &KnownFacts,
    estimator: &dyn TokenEstimator,
) -> (Vec<Message>, u32) {
    let mut messages = vec![Message::System {
        text: config.system_prompt.clone(),
    }];
    let facts_text = facts.render();
    let mut facts_tokens = 0u32;
    if !facts_text.is_empty() {
        let message = Message::System { text: facts_text };
        facts_tokens = estimate_serialized(estimator, &message);
        messages.push(message);
    }
    for item in &session.transcript {
        let message = match item {
            Item::User { text } => Some(Message::User { text: text.clone() }),
            Item::Assistant(assistant) => {
                if assistant.stop_reason == crate::model::StopReason::ContentFilter {
                    None
                } else {
                    Some(Message::Assistant(assistant.clone()))
                }
            }
            Item::ToolResult(result) => Some(Message::ToolResult(ToolResult {
                // Effects and artifact refs are Loom-side state (ADR-012,
                // ADR-014): they persist in the transcript but are never
                // model-visible, so the projection strips them. This also
                // keeps request recordings, the provider wire shape, and the
                // byte census independent of effect/artifact bookkeeping. The
                // model only learns an artifact exists from the note the tool
                // engine appended to the content.
                effects: ToolEffects::none(),
                artifact: None,
                ..result.clone()
            })),
        };
        if let Some(message) = message {
            messages.push(message);
        }
    }
    (messages, facts_tokens)
}

/// The tools exposed to the model (§8.2): all registered tools, in
/// registration order (deterministic, stable for the task), unless the
/// model cannot take tools at all.
///
/// When the model reports no `tool_calls` capability the tool list is
/// empty: exposing definitions the model cannot act on wastes context and
/// invites hallucinated calls the engine would reject.
fn exposed_tools(
    registry: &ToolRegistry,
    capabilities: &ModelCapabilities,
) -> Vec<crate::tool::ToolSpec> {
    if !capabilities.tool_calls {
        return Vec::new();
    }
    registry.specs().cloned().collect()
}

/// Serialized byte length of a value, or 0 if serialization fails (it cannot
/// for the types in this crate; the fallback keeps the census panic-free).
pub(super) fn serialized_size<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_string(value)
        .map(|json| json.len() as u64)
        .unwrap_or(0)
}

/// Attributes the request's estimated context to its sources (ADR-016, V18).
///
/// `facts_tokens` is the estimated tokens of the repository facts
/// message, subtracted from the system-prompt attribution so the two system
/// messages in the prefix never double-count.
///
/// Per-source figures are the estimator's tokens over each serialized
/// message (or tool spec); `total` is the authoritative estimate: the
/// whole message list plus tool list, serialized once and estimated,
/// plus one token per message for framing (§8). The two are computed
/// from the same payload, so `total` is exactly the number the budget
/// check compares.
pub(super) fn census(
    request: &ModelRequest,
    facts_tokens: u32,
    estimator: &dyn TokenEstimator,
) -> ContextCensus {
    let mut system_total = 0u32;
    let mut user_items = 0u32;
    let mut assistant_history = 0u32;
    let mut reasoning_replay = 0u32;
    let mut tool_results = 0u32;
    for message in &request.messages {
        match message {
            Message::System { .. } => {
                system_total = system_total.saturating_add(estimate_serialized(estimator, message));
            }
            Message::User { .. } => {
                user_items = user_items.saturating_add(estimate_serialized(estimator, message));
            }
            Message::Assistant(assistant) => {
                // Reasoning is attributed on its own; the rest of the
                // assistant message (text and tool-call arguments) stays in
                // `assistant_history`, so the two never double-count.
                let reasoning = assistant
                    .reasoning
                    .as_ref()
                    .map_or(0, |trace| estimate_serialized(estimator, trace));
                reasoning_replay = reasoning_replay.saturating_add(reasoning);
                assistant_history = assistant_history.saturating_add(
                    estimate_serialized(estimator, message).saturating_sub(reasoning),
                );
            }
            Message::ToolResult(_) => {
                tool_results = tool_results.saturating_add(estimate_serialized(estimator, message));
            }
        }
    }
    let tool_specs: u32 = request
        .tools
        .iter()
        .map(|spec| estimate_serialized(estimator, spec))
        .fold(0u32, u32::saturating_add);

    let messages = super::messages_serialized(&request.messages);
    let tools = serde_json::to_string(&request.tools);
    debug_assert!(tools.is_ok(), "tool-spec serialization must not fail");
    // Saturating on each half: a compiler that reports `u32::MAX`
    // estimated tokens will simply exceed any budget.
    let total = estimator
        .estimate(&messages)
        .saturating_add(estimator.estimate(&tools.unwrap_or_default()))
        .saturating_add(u32::try_from(request.messages.len()).unwrap_or(u32::MAX));

    ContextCensus {
        // Everything in a `System` message that is not the facts block is the
        // system prompt; the two are the only system messages projected.
        system_prompt: system_total.saturating_sub(facts_tokens),
        repo_facts: facts_tokens,
        user_items,
        assistant_history,
        reasoning_replay,
        tool_results,
        tool_specs,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextBudget, ContextCompiler};
    use crate::id::TaskId;
    use crate::model::StopReason;
    use crate::session::{Session, TaskRecord};
    use crate::tool::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
    use crate::transcript::{
        AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult,
        ToolStatus,
    };
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    const SYS: &str = "You are loom.";
    const TASK: &str = "find the implementation";

    fn config() -> AgentConfig {
        AgentConfig::new(PathBuf::from("/ws"), SYS)
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities::new(100_000)
            .with_tool_calls(true)
            .with_max_output_tokens(4_096)
            .with_reasoning(true)
    }

    fn budget() -> ContextBudget {
        ContextBudget {
            max_input_tokens: 50_000,
            reserved_output_tokens: 4_096,
        }
    }

    fn session() -> Session {
        Session::new(crate::id::SessionId::new())
    }

    fn task_record() -> TaskRecord {
        TaskRecord::new(TaskId::new(), 0)
    }

    fn assistant(content: &str, stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: stop,
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).unwrap(),
            name: name.into(),
            arguments: ToolArguments::try_from(json!({"path": "x"})).unwrap(),
        }
    }

    fn result_for(id: &str) -> ToolResult {
        ToolResult::new(
            ToolCallId::try_from(id.to_owned()).unwrap(),
            ToolStatus::Success,
            "ok",
            false,
        )
    }

    fn empty_facts() -> &'static KnownFacts {
        static EMPTY: OnceLock<KnownFacts> = OnceLock::new();
        EMPTY.get_or_init(KnownFacts::default)
    }

    fn input<'a>(
        session: &'a Session,
        tools: &'a ToolRegistry,
        task: &'a TaskRecord,
        budget: ContextBudget,
        capabilities: &'a ModelCapabilities,
        config: &'a AgentConfig,
    ) -> ContextInput<'a> {
        input_with_facts(
            session,
            tools,
            task,
            budget,
            capabilities,
            config,
            empty_facts(),
        )
    }

    fn input_with_facts<'a>(
        session: &'a Session,
        tools: &'a ToolRegistry,
        task: &'a TaskRecord,
        budget: ContextBudget,
        capabilities: &'a ModelCapabilities,
        config: &'a AgentConfig,
        facts: &'a KnownFacts,
    ) -> ContextInput<'a> {
        ContextInput {
            session,
            tools,
            task,
            budget,
            capabilities,
            model: "deepseek-flash",
            config,
            facts,
            prune_checkpoint: None,
        }
    }

    fn compile(
        session: &Session,
        tools: &ToolRegistry,
        budget: ContextBudget,
    ) -> Result<CompiledContext, ContextError> {
        DefaultContextCompiler::default().compile(input(
            session,
            tools,
            &task_record(),
            budget,
            &capabilities(),
            &config(),
        ))
    }

    impl DefaultContextCompiler {
        /// Compiles a task with a specific repository-facts snapshot.
        fn compile_with_facts(
            &self,
            session: &Session,
            facts: &KnownFacts,
        ) -> Result<CompiledContext, ContextError> {
            self.compile(input_with_facts(
                session,
                &registry(),
                &task_record(),
                budget(),
                &capabilities(),
                &config(),
                facts,
            ))
        }
    }

    /// A registered tool (dummy — the compiler only reads specs).
    struct DummyTool;
    impl Tool for DummyTool {
        fn spec(&self) -> &ToolSpec {
            static SPEC: OnceLock<ToolSpec> = OnceLock::new();
            SPEC.get_or_init(|| ToolSpec {
                name: "read_file".into(),
                description: "Reads a file.".into(),
                parameters: json!({"type": "object"}),
            })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn call<'a>(
            &'a self,
            _input: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async { Ok(ToolOutput::new("")) })
        }
    }

    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::default();
        reg.register(Box::new(DummyTool)).expect("register");
        reg
    }

    #[test]
    fn system_prompt_leads_and_history_follows() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        s.append_turn(assistant("working", StopReason::ToolCalls), vec![])
            .unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let messages = &compiled.request.messages;
        assert!(matches!(messages[0], Message::System { .. }));
        assert!(matches!(&messages[1], Message::User { text } if text == TASK));
        assert!(matches!(messages[2], Message::Assistant(_)));
    }

    #[test]
    fn reasoning_is_replayed_byte_exact() {
        let mut s = session();
        let reasoning = ReasoningTrace {
            text: "  odd  whitespace \n\t and ünicode ✓ ".into(),
            provider_data: None,
        };
        let mut msg = assistant("done", StopReason::EndTurn);
        msg.reasoning = Some(reasoning.clone());
        s.push_item(Item::Assistant(msg)).unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let Message::Assistant(projected) = &compiled.request.messages[1] else {
            panic!("expected assistant message");
        };
        assert_eq!(projected.reasoning, Some(reasoning));
    }

    #[test]
    fn content_filter_turn_is_omitted_from_projection() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        s.append_turn(assistant("blocked", StopReason::ContentFilter), vec![])
            .unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let messages = &compiled.request.messages;
        // System + user; the refused assistant turn is never replayed.
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[1], Message::User { .. }));
    }

    #[test]
    fn tool_calls_project_with_results_in_order() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let mut msg = assistant("calling", StopReason::ToolCalls);
        msg.tool_calls = vec![call("c1", "read_file"), call("c2", "list_dir")];
        s.append_turn(msg, vec![result_for("c1"), result_for("c2")])
            .unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let messages = &compiled.request.messages;
        assert!(matches!(messages[2], Message::Assistant(_)));
        assert!(matches!(&messages[3], Message::ToolResult(r) if r.call_id.as_str() == "c1"));
        assert!(matches!(&messages[4], Message::ToolResult(r) if r.call_id.as_str() == "c2"));
    }

    #[test]
    fn tool_result_effects_stay_out_of_the_model_projection() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let mut msg = assistant("calling", StopReason::ToolCalls);
        msg.tool_calls = vec![call("c1", "read_file")];
        let mut result = result_for("c1");
        result.effects = ToolEffects::none().with_file_written("src/lib.rs");
        s.append_turn(msg, vec![result]).unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let Some(Message::ToolResult(projected)) = compiled.request.messages.last() else {
            panic!("expected a tool result last");
        };
        assert!(
            projected.effects.is_empty(),
            "effects are Loom-side state and must not reach the model"
        );
    }

    #[test]
    fn tools_exposed_in_registration_order() {
        let compiled = compile(&session(), &registry(), budget()).expect("within budget");
        let tools = &compiled.request.tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[test]
    fn no_tools_when_model_lacks_tool_calls() {
        let caps = ModelCapabilities::new(100_000); // tool_calls: false
        let compiled = DefaultContextCompiler::default()
            .compile(input(
                &session(),
                &registry(),
                &task_record(),
                budget(),
                &caps,
                &config(),
            ))
            .expect("within budget");
        assert!(compiled.request.tools.is_empty());
    }

    #[test]
    fn budget_exceeded_is_reported() {
        // A huge transcript blows past the (tiny) budget.
        let mut s = session();
        for i in 0..2000 {
            s.push_item(Item::User {
                text: format!("line {i} of a very long conversation …").repeat(3),
            })
            .unwrap();
        }
        let tiny = ContextBudget {
            max_input_tokens: 1_000,
            reserved_output_tokens: 100,
        };
        let err = compile(&s, &registry(), tiny).expect_err("must exceed");
        assert!(matches!(err, ContextError::BudgetExceeded { .. }));
    }

    #[test]
    fn compile_is_deterministic() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        s.append_turn(assistant("working", StopReason::ToolCalls), vec![])
            .unwrap();
        s.push_item(Item::ToolResult(result_for("c1"))).unwrap();

        let tools = registry();
        let first = compile(&s, &tools, budget()).expect("within budget");
        let second = compile(&s, &tools, budget()).expect("within budget");
        assert_eq!(first.request, second.request);
    }

    #[test]
    fn prefix_stays_stable_across_turns() {
        // §8.1.1: static material (system prompt, tools, and the history
        // accumulated so far) must stay byte-identical as a task grows, so
        // provider-side prefix caching keeps working. Only the tail may
        // change between compilations.
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let tools = registry();

        let first = compile(&s, &tools, budget()).expect("within budget");
        assert_eq!(first.request.messages.len(), 2); // system + user

        // Two more turns: a tool round-trip, then a final answer.
        let mut msg = assistant("working", StopReason::ToolCalls);
        msg.tool_calls = vec![call("c1", "read_file")];
        s.append_turn(msg, vec![result_for("c1")]).unwrap();
        s.append_turn(assistant("done", StopReason::EndTurn), vec![])
            .unwrap();

        let second = compile(&s, &tools, budget()).expect("within budget");
        assert_eq!(second.request.messages.len(), 5); // +3 appended

        // The prefix (system + the first user message) is unchanged.
        assert_eq!(first.request.messages[0], second.request.messages[0]);
        assert_eq!(first.request.messages[1], second.request.messages[1]);
        // Tools are the same deterministic list too.
        assert_eq!(first.request.tools, second.request.tools);
    }

    #[test]
    fn empty_transcript_under_budget() {
        let compiled = compile(&session(), &registry(), budget()).expect("within budget");
        // System message only.
        assert_eq!(compiled.request.messages.len(), 1);
    }

    #[test]
    fn census_attributes_every_source() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let mut msg = assistant("calling", StopReason::ToolCalls);
        msg.reasoning = Some(ReasoningTrace {
            text: "let me look at the file first".into(),
            provider_data: None,
        });
        msg.tool_calls = vec![call("c1", "read_file")];
        s.append_turn(msg, vec![result_for("c1")]).unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        let census = compiled.census;
        assert!(census.system_prompt > 0, "system prompt attributed");
        assert!(census.user_items > 0, "user item attributed");
        assert!(census.assistant_history > 0, "assistant text attributed");
        assert!(census.reasoning_replay > 0, "reasoning attributed");
        assert!(census.tool_results > 0, "tool result attributed");
        assert!(census.tool_specs > 0, "tool specs attributed");
        assert!(census.total > 0);
    }

    #[test]
    fn census_is_zero_sources_for_an_empty_transcript() {
        // No tools registered, so nothing but the system prompt is present.
        let compiled =
            compile(&session(), &ToolRegistry::default(), budget()).expect("within budget");
        let census = compiled.census;
        // System prompt is the only content; nothing else is attributed.
        assert!(census.system_prompt > 0);
        assert_eq!(census.user_items, 0);
        assert_eq!(census.assistant_history, 0);
        assert_eq!(census.reasoning_replay, 0);
        assert_eq!(census.tool_results, 0);
        assert_eq!(census.tool_specs, 0);
        assert!(census.total >= census.system_prompt);
    }

    #[test]
    fn reasoning_is_attributed_apart_from_assistant_text() {
        let mut s = session();
        let mut with_reasoning = assistant("answer", StopReason::EndTurn);
        with_reasoning.reasoning = Some(ReasoningTrace {
            text: "a long private chain of thought, replayed byte-exact".into(),
            provider_data: None,
        });
        s.push_item(Item::Assistant(with_reasoning)).unwrap();

        let compiled = compile(&s, &registry(), budget()).expect("within budget");
        assert!(compiled.census.reasoning_replay > 0);
        assert!(compiled.census.assistant_history > 0);
    }

    #[test]
    fn reserved_output_caps_max_tokens() {
        let compiled = compile(&session(), &registry(), budget()).expect("within budget");
        // min(max_output_tokens=4096, reserved=4096) — equal, both win.
        assert_eq!(compiled.request.max_tokens, Some(4_096));
    }

    #[test]
    fn max_tokens_reservation_wins_when_smaller() {
        let caps = capabilities().with_max_output_tokens(8_192); // reserved 4096
        let compiled = DefaultContextCompiler::default()
            .compile(input(
                &session(),
                &registry(),
                &task_record(),
                budget(),
                &caps,
                &config(),
            ))
            .expect("within budget");
        assert_eq!(
            compiled.request.max_tokens,
            Some(4_096),
            "reservation caps the request"
        );
    }

    #[test]
    fn max_tokens_model_limit_wins_when_smaller() {
        let caps = capabilities().with_max_output_tokens(1_000); // reserved 4096
        let compiled = DefaultContextCompiler::default()
            .compile(input(
                &session(),
                &registry(),
                &task_record(),
                budget(),
                &caps,
                &config(),
            ))
            .expect("within budget");
        assert_eq!(
            compiled.request.max_tokens,
            Some(1_000),
            "model limit caps the request"
        );
    }

    #[test]
    fn max_tokens_none_when_model_reports_no_limit() {
        let caps = ModelCapabilities::new(100_000)
            .with_tool_calls(true)
            .with_reasoning(true);
        let compiled = DefaultContextCompiler::default()
            .compile(input(
                &session(),
                &registry(),
                &task_record(),
                budget(),
                &caps,
                &config(),
            ))
            .expect("within budget");
        assert_eq!(
            compiled.request.max_tokens, None,
            "no output limit -> no cap"
        );
    }

    #[test]
    fn model_name_flows_into_request() {
        let compiled = compile(&session(), &registry(), budget()).expect("within budget");
        assert_eq!(compiled.request.model, "deepseek-flash");
    }

    fn facts() -> KnownFacts {
        KnownFacts {
            root: PathBuf::from("/ws"),
            languages: vec!["Rust".into()],
            build_systems: vec!["Cargo".into()],
            source_dirs: vec!["src".into()],
            test_dirs: vec!["tests".into()],
            config_files: vec!["Cargo.toml".into()],
            git: crate::context::GitState {
                branch: Some("main".into()),
                changed_files: vec!["src/lib.rs".into()],
            },
        }
    }

    #[test]
    fn facts_block_follows_the_system_prompt() {
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let f = facts();
        let compiled = DefaultContextCompiler::default()
            .compile_with_facts(&s, &f)
            .expect("within budget");
        let messages = &compiled.request.messages;
        assert!(matches!(&messages[0], Message::System { text } if text == SYS));
        let Message::System { text } = &messages[1] else {
            panic!("expected the facts system message second");
        };
        assert!(text.contains("Repository facts"), "{text}");
        assert!(matches!(&messages[2], Message::User { .. }));
        assert!(compiled.census.repo_facts > 0);
    }

    #[test]
    fn empty_facts_add_no_message() {
        // The default (no source) projection is unchanged: system + history.
        let compiled = compile(&session(), &registry(), budget()).expect("within budget");
        assert_eq!(compiled.request.messages.len(), 1);
        assert_eq!(compiled.census.repo_facts, 0);
    }

    #[test]
    fn facts_are_attributed_apart_from_the_system_prompt() {
        let f = facts();
        let with = DefaultContextCompiler::default()
            .compile_with_facts(&session(), &f)
            .expect("within budget");
        let without = compile(&session(), &registry(), budget()).expect("within budget");
        assert!(with.census.repo_facts > 0);
        // The system-prompt attribution is untouched by the extra block.
        assert_eq!(with.census.system_prompt, without.census.system_prompt);
        assert!(with.census.total > without.census.total);
    }

    #[test]
    fn facts_block_is_stable_across_turns() {
        // Prefix caching (§8.1.1): one snapshot per task, byte-identical in
        // every compile, so the cached prefix is never churned mid-task.
        let mut s = session();
        s.push_item(Item::User { text: TASK.into() }).unwrap();
        let f = facts();
        let first = DefaultContextCompiler::default()
            .compile_with_facts(&s, &f)
            .expect("within budget");
        let mut msg = assistant("working", StopReason::ToolCalls);
        msg.tool_calls = vec![call("c1", "read_file")];
        s.append_turn(msg, vec![result_for("c1")]).unwrap();
        let second = DefaultContextCompiler::default()
            .compile_with_facts(&s, &f)
            .expect("within budget");
        assert_eq!(first.request.messages[0], second.request.messages[0]);
        assert_eq!(first.request.messages[1], second.request.messages[1]);
    }

    #[test]
    fn budget_from_window_rejects_oversized_reservation() {
        assert!(ContextBudget::from_window(100, 200).is_err());
        let ok = ContextBudget::from_window(100, 30).expect("fits");
        assert_eq!(ok.max_input_tokens, 70);
    }
}
