mod decisions;
mod elision;
mod handoff;
mod reasoning;
mod reclaim;

use super::*;
use crate::context::{
    AgentConfig, ContextBudget, ContextInput, DefaultContextCompiler, PruneCheckpoint,
};
use crate::model::{ModelCapabilities, StopReason};
use crate::session::{Session, TaskRecord};
use crate::tool::ToolRegistry;
use crate::transcript::{
    AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use serde_json::json;

fn config() -> AgentConfig {
    AgentConfig::new(std::path::PathBuf::from("/w"), "system")
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities::new(500_000)
        .with_tool_calls(true)
        .with_max_output_tokens(4_096)
}

fn budget() -> ContextBudget {
    ContextBudget {
        max_input_tokens: 400_000,
        reserved_output_tokens: 4_096,
    }
}

fn registry() -> ToolRegistry {
    // A read-only tool declaring its path, and a mutating one.
    struct ReadTool;
    impl crate::tool::Tool for ReadTool {
        fn spec(&self) -> &crate::tool::ToolSpec {
            static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| crate::tool::ToolSpec {
                name: "read_file".to_owned(),
                description: "Reads a file.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<crate::tool::PathTouch>> {
            Some(vec![crate::tool::PathTouch::read(
                input.get("path").and_then(|v| v.as_str()).unwrap_or("x"),
            )])
        }
        fn call<'a>(
            &'a self,
            _input: serde_json::Value,
            _ctx: &'a crate::tool::ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::ToolOutput, crate::tool::ToolError>>
        {
            unreachable!("call not needed for pruning tests")
        }
    }
    struct WriteTool;
    impl crate::tool::Tool for WriteTool {
        fn spec(&self) -> &crate::tool::ToolSpec {
            static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> = std::sync::OnceLock::new();
            SPEC.get_or_init(|| crate::tool::ToolSpec {
                name: "write_file".to_owned(),
                description: "Writes a file.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn call<'a>(
            &'a self,
            _input: serde_json::Value,
            _ctx: &'a crate::tool::ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::ToolOutput, crate::tool::ToolError>>
        {
            unreachable!("call not needed for pruning tests")
        }
    }
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(ReadTool)).expect("register");
    registry.register(Box::new(WriteTool)).expect("register");
    registry
}

fn session_with(items: Vec<Item>) -> Session {
    let mut session = Session::new(crate::id::SessionId::new());
    for item in items {
        session.transcript.push(item);
    }
    session
}

fn assistant_text(content: &str) -> Item {
    Item::Assistant(AssistantMessage {
        content: content.to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        stop_reason: StopReason::EndTurn,
    })
}

fn assistant_calls(content: &str, calls: Vec<ToolCall>) -> Item {
    Item::Assistant(AssistantMessage {
        content: content.to_owned(),
        reasoning: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolCalls,
    })
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).expect("call id"),
        name: "read_file".to_owned(),
        arguments: ToolArguments::try_from(json!({ "path": path })).expect("args"),
    }
}

fn write_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).expect("call id"),
        name: "write_file".to_owned(),
        arguments: ToolArguments::try_from(json!({ "path": path, "content": "x" })).expect("args"),
    }
}

fn result(id: &str, content: &str) -> Item {
    Item::ToolResult(ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Success,
        content,
        false,
    ))
}

fn error_result(id: &str) -> Item {
    Item::ToolResult(ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Error(crate::transcript::ToolFailure::Execution),
        big("error", 70),
        false,
    ))
}

fn written_result(id: &str, path: &str) -> Item {
    let mut result = ToolResult::new(
        ToolCallId::try_from(id.to_owned()).expect("call id"),
        ToolStatus::Success,
        big("written", 25),
        false,
    );
    result.effects = crate::tool::ToolEffects::none().with_file_written(path);
    Item::ToolResult(result)
}

fn input<'a>(session: &'a Session, prune: Option<&'a PruneCheckpoint>) -> ContextInput<'a> {
    input_with(session, prune, budget())
}

fn input_with<'a>(
    session: &'a Session,
    prune: Option<&'a PruneCheckpoint>,
    budget: ContextBudget,
) -> ContextInput<'a> {
    shared();
    static FACTS: std::sync::OnceLock<crate::context::KnownFacts> = std::sync::OnceLock::new();
    let facts = FACTS.get_or_init(crate::context::KnownFacts::default);
    ContextInput {
        session,
        tools: READ_REGISTRY.get().expect("registry"),
        task: TASK.get().expect("task"),
        budget,
        capabilities: CAPABILITIES.get().expect("capabilities"),
        model: "deepseek-flash",
        config: AGENT_CONFIG.get().expect("config"),
        facts,
        prune_checkpoint: prune,
    }
}

static READ_REGISTRY: std::sync::OnceLock<ToolRegistry> = std::sync::OnceLock::new();
static TASK: std::sync::OnceLock<TaskRecord> = std::sync::OnceLock::new();
static CAPABILITIES: std::sync::OnceLock<ModelCapabilities> = std::sync::OnceLock::new();
static AGENT_CONFIG: std::sync::OnceLock<AgentConfig> = std::sync::OnceLock::new();

fn shared() {
    READ_REGISTRY.get_or_init(registry);
    TASK.get_or_init(|| TaskRecord::new(crate::id::TaskId::new(), 0));
    CAPABILITIES.get_or_init(capabilities);
    AGENT_CONFIG.get_or_init(config);
}

fn compiler(config: PruneConfig) -> PruningContextCompiler {
    shared();
    PruningContextCompiler::new(Box::new(DefaultContextCompiler::default()), config)
}

/// A config that fires at the old flat threshold, so the strategy tests
/// keep their shape regardless of the window-relative default (the 400 K
/// test budget would otherwise push the default trigger to 320 K).
fn firing() -> PruneConfig {
    PruneConfig {
        trigger_percent: 0,
        floor_tokens: 64_000,
        ..PruneConfig::default()
    }
}

fn big(prefix: &str, kb: usize) -> String {
    prefix.to_owned() + &"x".repeat(kb * 1024)
}
