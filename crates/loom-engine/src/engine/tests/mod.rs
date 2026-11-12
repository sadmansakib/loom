use super::*;
use futures::stream;

use super::assembly::{bare_registry, build_registry};
use super::budgets::prune_config_for;
use crate::auth;
use loom_kernel::agent::{TaskError, TurnBudget};
use loom_kernel::event::EventKind;
use loom_kernel::id::SessionId;
use loom_providers::OPENROUTER_PROVIDER_NAME;
use loom_tools::ToolBudgets;
use loom_tools::policy::CommandPolicy;
use std::path::PathBuf;

use loom_kernel::context::{AgentConfig, DefaultContextCompiler, PruneConfig};
use loom_kernel::model::{
    ModelCapabilities, ModelError, ModelErrorKind, ModelEvent, ModelProvider, ModelRequest,
    ModelResponse, ModelStream, StopReason, Usage,
};
use loom_kernel::session::TaskStatus;
use loom_kernel::store::MemorySessionStore;
use loom_kernel::transcript::AssistantMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
/// A provider that replays one scripted event list per `stream()` call.
/// Enough engine coverage for the driving layer: the kernel loop's own
/// behaviour is pinned in `loom-kernel`'s `agent_loop` tests.
struct FakeProvider {
    scripts: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
}

impl FakeProvider {
    /// A provider replaying the given scripts, one per `stream()` call.
    fn scripted(scripts: Vec<Vec<Result<ModelEvent, ModelError>>>) -> Self {
        Self {
            scripts: Mutex::new(VecDeque::from(scripts)),
        }
    }

    /// One script: a single response that answers with `text` and ends
    /// its turn.
    fn end_turn_script(text: &str) -> Vec<Result<ModelEvent, ModelError>> {
        vec![Ok(ModelEvent::Completed(ModelResponse {
            message: AssistantMessage {
                content: text.to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
            },
            usage: Usage::default(),
        }))]
    }

    /// Scripts for two tasks: the first ends its turn, the second fails
    /// hard with `error`.
    fn end_turn_then_error(text: &str, error: ModelError) -> Self {
        Self::scripted(vec![Self::end_turn_script(text), vec![Err(error)]])
    }
}

impl ModelProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> Option<ModelCapabilities> {
        // Tool-calling on: scripts emit tool calls, and the default
        // compiler exposes tools only to a model that can call them
        // (§8.2) — with this off, every scripted call would bounce
        // back `NotExposed` instead of running.
        Some(ModelCapabilities::new(8192).with_tool_calls(true))
    }

    fn stream<'a>(&'a self, _request: &'a ModelRequest) -> ModelStream<'a> {
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(stream::iter(script))
    }
}

/// An engine around a fake provider with an in-memory store — the
/// session-lifecycle seams without credentials, network, or files.
fn fake_engine(provider: FakeProvider) -> Engine {
    let agent = Agent::new(
        Box::new(provider),
        Box::new(DefaultContextCompiler::default()),
        loom_kernel::tool::ToolRegistry::new(),
        AgentConfig::new(PathBuf::from("."), "test agent"),
        "fake-model",
    )
    .with_store(Box::new(MemorySessionStore::new()));
    Engine {
        agent,
        routing_easy: None,
        background: None,
        stall_escalation: None,
        db_path: None,
        lsp_servers: Vec::new(),
        routing_hard: None,
        workspace: PathBuf::from("."),
        policy: CommandPolicy::default(),
        fast_paths: false,
        resolution: None,
        notices_flushed: std::sync::OnceLock::new(),
        events: None,
    }
}

/// A one-file cargo fixture the fast paths can rename inside.
fn rename_fixture(base: &std::path::Path) -> std::path::PathBuf {
    let workspace = base.join("ws");
    std::fs::create_dir_all(workspace.join("src")).expect("dirs");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"fp_rename\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse_opts() -> u32 {\n    7\n}\n\n#[cfg(test)]\nmod tests {\n    \
         use super::*;\n\n    #[test]\n    fn calls_the_helper() {\n        assert_eq!(parse_opts(), 7);\n    }\n}\n",
    )
    .expect("lib");
    workspace
}

/// Drains every pending event kind from the channel.
fn drain(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<loom_kernel::event::EventEnvelope>,
) -> Vec<EventKind> {
    let mut kinds = Vec::new();
    while let Ok(envelope) = receiver.try_recv() {
        kinds.push(envelope.kind);
    }
    kinds
}

mod background;
mod budgets;
mod escalation;
mod fast_paths;
mod git;
mod notices;
mod open;
mod persistence;
mod registries;
mod routing;
mod skills;
mod symbols;
