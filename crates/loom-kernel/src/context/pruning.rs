//! Trigger-gated deterministic context pruning (2L, ADR-011).
//!
//! [`PruningContextCompiler`] wraps another compiler (the shipped
//! [`DefaultContextCompiler`](super::DefaultContextCompiler)) and, once the
//! projected input crosses an absolute threshold, replaces *older* tool
//! results with informative stubs so the model keeps working inside its
//! budget. The transcript is never mutated — only this request's projection
//! changes, and every pruned result keeps its `call_id` plus a recovery
//! instruction, so the model can re-run the call if it needs the content
//! again.
//!
//! Determinism and cache behavior:
//!
//! * Decisions are computed only when the trigger fires, then **frozen** in
//!   a [`PruneCheckpoint`] that the caller passes back in on later turns.
//!   Between triggers the projection is byte-stable, so the provider's
//!   prefix cache holds; each trigger costs one cache miss (ADR-011).
//! * Everything is derived from the session transcript with fixed rules —
//!   no I/O, no clocks, no model calls.
//!
//! Never pruned: user items, assistant messages (text and reasoning), and
//! the protected tail — the last `protected_tail_turns` turns *and* at
//! least `protected_tail_tokens` estimated tokens of trailing items
//! (whichever reaches further back).
//!
//! Known limitation: an inner `Err(ContextError::BudgetExceeded)` carries
//! no request to prune, so the wrapper cannot rescue it. The trigger
//! threshold must therefore sit below the model's input budget, which the
//! defaults (64 K trigger vs ≥ 64 K windows) assume.

use serde::{Deserialize, Serialize};

use crate::model::Message;
use crate::session::Session;
use crate::tool::{PathAccess, ToolRegistry, paths_overlap};
use crate::transcript::{Item, ToolCallId, ToolResult, ToolStatus};

use super::default::{census, serialized_size};
use super::{CompiledContext, ContextCompiler, ContextError, ContextInput, PruneCheckpoint};

/// Pruning thresholds; the defaults are ADR-011's hypotheses, to be
/// revisited against the 2L long-context benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneConfig {
    /// Estimated input tokens above which a recompute is triggered.
    pub trigger_tokens: u32,
    /// A prune only proceeds when at least this many tokens are
    /// reclaimable; below it the churn is not worth a cache miss.
    pub min_reclaim_tokens: u32,
    /// The trailing context is never pruned down to less than this many
    /// estimated tokens.
    pub protected_tail_tokens: u32,
    /// …and never fewer than this many whole turns.
    pub protected_tail_turns: usize,
    /// Errors older than this many turns are stale.
    pub stale_error_turns: usize,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: 64_000,
            min_reclaim_tokens: 20_000,
            protected_tail_tokens: 20_000,
            protected_tail_turns: 3,
            stale_error_turns: 4,
        }
    }
}

/// Why a tool result was stubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StubReason {
    /// A later identical read-only call supersedes this one.
    DuplicateRead,
    /// A later recorded write touched a path this call read.
    SupersededRead,
    /// An error result older than `stale_error_turns`.
    StaleError,
    /// A large read-only result aged out of the unprotected region.
    AgedOut,
}

impl StubReason {
    /// The deterministic stub body replacing the pruned content.
    #[must_use]
    pub fn stub_text(self) -> &'static str {
        match self {
            Self::DuplicateRead => {
                "[pruned] duplicate of a later identical read-only call; re-run the tool to recover the content"
            }
            Self::SupersededRead => {
                "[pruned] the file changed after this read; re-run the tool for the current content"
            }
            Self::StaleError => {
                "[pruned] stale error from an earlier turn; re-run the tool for a fresh result"
            }
            Self::AgedOut => {
                "[pruned] aged out by context pruning; re-run the tool to recover the content"
            }
        }
    }
}

/// One frozen stub decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubDecision {
    /// The call whose result is stubbed.
    pub call_id: String,
    /// Why — decides the stub text.
    pub reason: StubReason,
}

/// A [`ContextCompiler`] that prunes projected tool results once the
/// estimated input crosses `config.trigger_tokens`.
pub struct PruningContextCompiler {
    inner: Box<dyn ContextCompiler>,
    config: PruneConfig,
}

impl PruningContextCompiler {
    /// Wraps `inner` (the shipped
    /// [`DefaultContextCompiler`](super::DefaultContextCompiler)) with the
    /// given thresholds.
    #[must_use]
    pub fn new(inner: Box<dyn ContextCompiler>, config: PruneConfig) -> Self {
        Self { inner, config }
    }
}

impl ContextCompiler for PruningContextCompiler {
    fn compile(&self, input: ContextInput<'_>) -> Result<CompiledContext, ContextError> {
        // The inner compiler takes `input` by value, so copy out everything
        // the wrapper still needs. These are references and `Copy` values —
        // no transcript is cloned.
        let budget = input.budget;
        let session = input.session;
        let tools = input.tools;
        let facts_text = input.facts.render();
        let mut checkpoint = input.prune_checkpoint.cloned();
        let mut compiled = self.inner.compile(input)?;

        // `census` needs the facts-message size the inner projection used;
        // recompute it the same way `project_messages` does.
        let facts_bytes = if facts_text.is_empty() {
            0
        } else {
            serialized_size(&Message::System { text: facts_text })
        };

        // Apply the frozen decisions first: between triggers the projection
        // must stay byte-stable (ADR-011).
        if let Some(frozen) = checkpoint
            .as_ref()
            .map(|c| c.stubs.clone())
            .filter(|stubs| !stubs.is_empty())
        {
            apply_stubs(&mut compiled.request, &frozen);
            compiled.census = census(&compiled.request, facts_bytes);
        }

        // Absolute trigger: recompute decisions over the whole transcript.
        if compiled.census.total > self.config.trigger_tokens {
            let (decisions, reclaim) = decide(session, tools, self.config);
            if reclaim >= u64::from(self.config.min_reclaim_tokens) {
                apply_stubs(&mut compiled.request, &decisions);
                compiled.census = census(&compiled.request, facts_bytes);
                checkpoint = Some(PruneCheckpoint {
                    upto_item: session.transcript.len(),
                    stubs: decisions,
                });
            }
        }

        // Budget still applies after pruning.
        if compiled.census.total > budget.max_input_tokens {
            return Err(ContextError::BudgetExceeded {
                estimated: compiled.census.total,
                max_input_tokens: budget.max_input_tokens,
            });
        }

        compiled.prune_checkpoint = checkpoint;
        Ok(compiled)
    }
}

/// Rewrites projected `ToolResult` messages whose `call_id` is stubbed,
/// leaving every other message (and the call/result pairing) untouched.
fn apply_stubs(request: &mut crate::model::ModelRequest, stubs: &[StubDecision]) {
    for message in &mut request.messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        let Some(reason) = stubs
            .iter()
            .find(|stub| stub.call_id == result.call_id.as_str())
            .map(|stub| stub.reason)
        else {
            continue;
        };
        result.content = reason.stub_text().to_owned();
        result.truncated = false;
    }
}

/// Facts gathered from the transcript for the decision pass.
struct ResultFacts {
    index: usize,
    turn: usize,
    call_id: ToolCallId,
    status: ToolStatus,
    bytes: u64,
    /// Tool name + canonical arguments, when the call is known and
    /// read-only; `None` otherwise (unknown or mutating — never a dedupe,
    /// supersede or age-out candidate).
    read_call: Option<(String, String, Vec<String>)>,
}

/// Computes the stub set from the transcript under ADR-011's fixed
/// strategy order, plus the reclaim estimate (in tokens) against the
/// *unpruned* projection.
fn decide(
    session: &Session,
    registry: &ToolRegistry,
    config: PruneConfig,
) -> (Vec<StubDecision>, u64) {
    let transcript: &[Item] = &session.transcript;

    // Pass 1 — call metadata (name, canonical args, declared reads) from
    // assistant tool_calls, in call order.
    let mut calls: Vec<(String, String, String, Vec<String>)> = Vec::new(); // (id, name, args, reads)
    for item in transcript {
        if let Item::Assistant(assistant) = item {
            for call in &assistant.tool_calls {
                let reads = read_paths(registry, &call.name, &call.arguments);
                calls.push((
                    call.id.as_str().to_owned(),
                    call.name.clone(),
                    call.arguments.to_string(),
                    reads,
                ));
            }
        }
    }
    let is_read_only = |name: &str| registry.get(name).is_some_and(|tool| tool.is_read_only());

    // Pass 2 — tool-result facts, assistant turn boundaries, later writes.
    let mut assistant_at: Vec<usize> = Vec::new();
    let mut results: Vec<ResultFacts> = Vec::new();
    let mut writes: Vec<(usize, Vec<String>)> = Vec::new();
    let mut turn = 0usize;
    for (index, item) in transcript.iter().enumerate() {
        match item {
            Item::Assistant(_) => {
                turn += 1;
                assistant_at.push(index);
            }
            Item::ToolResult(result) => {
                let meta = calls
                    .iter()
                    .find(|(id, ..)| *id == result.call_id.as_str())
                    .and_then(|(id, name, args, reads)| {
                        is_read_only(name)
                            .then(|| (name.clone(), args.clone(), reads.clone()))
                            .map(|meta| (id.clone(), meta))
                    });
                let read_call = meta.map(|(_id, meta)| meta);
                if !result.effects.files_written.is_empty()
                    || !result.effects.files_deleted.is_empty()
                {
                    let mut changed = result.effects.files_written.clone();
                    changed.extend(result.effects.files_deleted.iter().cloned());
                    writes.push((index, changed));
                }
                results.push(ResultFacts {
                    index,
                    turn,
                    call_id: result.call_id.clone(),
                    status: result.status,
                    bytes: serialized_size(item),
                    read_call,
                });
            }
            Item::User { .. } => {}
        }
    }

    // Protected tail: last `protected_tail_turns` turns AND trailing
    // `protected_tail_tokens` tokens — whichever reaches further back.
    let turn_cut = assistant_at
        .len()
        .checked_sub(config.protected_tail_turns)
        .and_then(|from| assistant_at.get(from))
        .copied()
        .unwrap_or(0);
    let mut trailing = 0u64;
    let mut token_cut = transcript.len();
    for (index, item) in transcript.iter().enumerate().rev() {
        trailing += serialized_size(item);
        if trailing >= u64::from(config.protected_tail_tokens) * u64::from(super::BYTES_PER_TOKEN) {
            token_cut = index;
            break;
        }
    }
    let protected_from = turn_cut.min(token_cut);

    let total_turns = assistant_at.len();
    let mut stubbed: Vec<StubDecision> = Vec::new();
    let mut reclaim_bytes = 0u64;
    let stub = |facts: &ResultFacts,
                reason: StubReason,
                stubbed: &mut Vec<StubDecision>,
                reclaim_bytes: &mut u64| {
        *reclaim_bytes +=
            facts
                .bytes
                .saturating_sub(stub_bytes(&facts.call_id, &facts.status, reason));
        stubbed.push(StubDecision {
            call_id: facts.call_id.as_str().to_owned(),
            reason,
        });
    };

    // (1) Dedupe identical read-only calls, keeping the newest.
    for earlier in &results {
        if earlier.index >= protected_from {
            continue;
        }
        let Some((name, args, _)) = earlier.read_call.as_ref() else {
            continue;
        };
        let is_newest = !results.iter().any(|later| {
            later.index > earlier.index
                && later
                    .read_call
                    .as_ref()
                    .is_some_and(|(n, a, _)| n == name && a == args)
        });
        if !is_newest {
            stub(
                earlier,
                StubReason::DuplicateRead,
                &mut stubbed,
                &mut reclaim_bytes,
            );
        }
    }

    // (2) Supersede reads of later-modified paths.
    for earlier in &results {
        if earlier.index >= protected_from
            || stubbed
                .iter()
                .any(|s| s.call_id == earlier.call_id.as_str())
        {
            continue;
        }
        let Some((_, _, reads)) = earlier.read_call.as_ref() else {
            continue;
        };
        let superseded = writes.iter().any(|(index, changed)| {
            *index > earlier.index
                && reads
                    .iter()
                    .any(|read| changed.iter().any(|path| paths_overlap(read, path)))
        });
        if superseded {
            stub(
                earlier,
                StubReason::SupersededRead,
                &mut stubbed,
                &mut reclaim_bytes,
            );
        }
    }

    // (3) Stale errors.
    for earlier in &results {
        if earlier.index >= protected_from
            || stubbed
                .iter()
                .any(|s| s.call_id == earlier.call_id.as_str())
        {
            continue;
        }
        if earlier.status != ToolStatus::Success
            && total_turns.saturating_sub(earlier.turn) > config.stale_error_turns
        {
            stub(
                earlier,
                StubReason::StaleError,
                &mut stubbed,
                &mut reclaim_bytes,
            );
        }
    }

    // (4) Age out large read-only results outside the protected tail.
    // `min_reclaim_tokens` is a *gate* on whether the prune runs at all
    // (checked in `compile`), never a cap: every unprotected read-only
    // result is stubbed, so the projection cannot grow without bound once
    // the trigger is crossed. Sorted largest-first for a deterministic,
    // size-prioritised order.
    let mut aged: Vec<&ResultFacts> = results
        .iter()
        .filter(|facts| {
            facts.index < protected_from
                && facts.status == ToolStatus::Success
                && facts.read_call.is_some()
                && !stubbed.iter().any(|s| s.call_id == facts.call_id.as_str())
        })
        .collect();
    aged.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.index.cmp(&b.index)));
    for facts in aged {
        stub(facts, StubReason::AgedOut, &mut stubbed, &mut reclaim_bytes);
    }

    (stubbed, reclaim_bytes / u64::from(super::BYTES_PER_TOKEN))
}

/// Serialized size of the stub that would replace this result.
fn stub_bytes(call_id: &ToolCallId, status: &ToolStatus, reason: StubReason) -> u64 {
    let probe = Item::ToolResult(ToolResult {
        call_id: call_id.clone(),
        status: *status,
        content: reason.stub_text().to_owned(),
        truncated: false,
        effects: crate::tool::ToolEffects::none(),
        artifact: None,
    });
    serialized_size(&probe)
}

/// The declared read paths of a call, when the tool is registered,
/// read-only, and describes its paths.
fn read_paths(registry: &ToolRegistry, name: &str, arguments: &serde_json::Value) -> Vec<String> {
    let Some(tool) = registry.get(name) else {
        return Vec::new();
    };
    tool.touched_paths(arguments)
        .unwrap_or_default()
        .into_iter()
        .filter(|touch| touch.access == PathAccess::Read)
        .map(|touch| touch.path)
        .collect()
}

// `ContextCensus` is reconstructed via `census` in `compile`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{
        AgentConfig, ContextBudget, ContextInput, DefaultContextCompiler, messages_serialized,
    };
    use crate::model::{ModelCapabilities, StopReason};
    use crate::session::{Session, TaskRecord};
    use crate::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId};
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
                static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> =
                    std::sync::OnceLock::new();
                SPEC.get_or_init(|| crate::tool::ToolSpec {
                    name: "read_file".to_owned(),
                    description: "Reads a file.".to_owned(),
                    parameters: serde_json::json!({"type": "object"}),
                })
            }
            fn is_read_only(&self) -> bool {
                true
            }
            fn touched_paths(
                &self,
                input: &serde_json::Value,
            ) -> Option<Vec<crate::tool::PathTouch>> {
                Some(vec![crate::tool::PathTouch::read(
                    input.get("path").and_then(|v| v.as_str()).unwrap_or("x"),
                )])
            }
            fn call<'a>(
                &'a self,
                _input: serde_json::Value,
                _ctx: &'a crate::tool::ToolContext,
            ) -> futures::future::BoxFuture<
                'a,
                Result<crate::tool::ToolOutput, crate::tool::ToolError>,
            > {
                unreachable!("call not needed for pruning tests")
            }
        }
        struct WriteTool;
        impl crate::tool::Tool for WriteTool {
            fn spec(&self) -> &crate::tool::ToolSpec {
                static SPEC: std::sync::OnceLock<crate::tool::ToolSpec> =
                    std::sync::OnceLock::new();
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
            ) -> futures::future::BoxFuture<
                'a,
                Result<crate::tool::ToolOutput, crate::tool::ToolError>,
            > {
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
            arguments: ToolArguments::try_from(json!({ "path": path, "content": "x" }))
                .expect("args"),
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
        shared();
        static FACTS: std::sync::OnceLock<crate::context::KnownFacts> = std::sync::OnceLock::new();
        let facts = FACTS.get_or_init(crate::context::KnownFacts::default);
        ContextInput {
            session,
            tools: READ_REGISTRY.get().expect("registry"),
            task: TASK.get().expect("task"),
            budget: budget(),
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
        PruningContextCompiler::new(Box::new(DefaultContextCompiler), config)
    }

    fn big(prefix: &str, kb: usize) -> String {
        prefix.to_owned() + &"x".repeat(kb * 1024)
    }

    #[test]
    fn below_trigger_no_stubs_and_no_checkpoint() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
            result("c1", &big("content", 30)),
            assistant_text("done"),
        ];
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
        assert!(compiled.census.total < 64_000);
    }

    #[test]
    fn duplicate_readonly_calls_are_deduped_keeping_newest() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..8 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("old{n}"), "src/a.rs")],
            ));
            items.push(result(&format!("old{n}"), &big("dup", 25)));
        }
        // The newest duplicate sits inside the protected tail.
        items.push(assistant_calls(
            "reading again",
            vec![read_call("new", "src/a.rs")],
        ));
        items.push(result("new", &big("dup", 25)));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "old0" && stub.reason == StubReason::DuplicateRead)
        );
        assert!(!checkpoint.stubs.iter().any(|stub| stub.call_id == "new"));
        // The projected content is the stub text.
        assert!(messages_contain(
            &compiled.request,
            StubReason::DuplicateRead.stub_text()
        ));
    }

    #[test]
    fn reads_superseded_by_later_writes_are_stubbed() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("read", vec![read_call("r1", "src/a.rs")]),
            result("r1", &big("read", 70)),
        ];
        for n in 0..8 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        // A write to the read path, still outside the protected tail.
        items.push(assistant_calls(
            "write target",
            vec![write_call("wt", "src/a.rs")],
        ));
        items.push(written_result("wt", "src/a.rs"));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "r1" && stub.reason == StubReason::SupersededRead)
        );
    }

    #[test]
    fn stale_errors_are_stubbed_after_four_turns() {
        let mut items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls(
                "try",
                vec![ToolCall {
                    id: ToolCallId::try_from("e1".to_owned()).expect("call id"),
                    name: "write_file".to_owned(),
                    arguments: ToolArguments::try_from(
                        json!({ "path": "src/a.rs", "content": "x" }),
                    )
                    .expect("args"),
                }],
            ),
            error_result("e1"),
        ];
        for n in 0..8 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "e1" && stub.reason == StubReason::StaleError)
        );
    }

    #[test]
    fn protected_tail_resists_ageout() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        // Old, large, unique read — an age-out candidate.
        items.push(assistant_calls(
            "read",
            vec![read_call("old", "src/old.rs")],
        ));
        items.push(result("old", &big("old", 70)));
        // Enough later bulk to push the total over the trigger while the
        // protected tail covers only the recent turns.
        for n in 0..6 {
            items.push(assistant_calls(
                "write",
                vec![write_call(&format!("w{n}"), &format!("src/file{n}.rs"))],
            ));
            items.push(written_result(&format!("w{n}"), &format!("src/file{n}.rs")));
        }
        // A recent large read inside the tail — must survive.
        items.push(assistant_calls(
            "read recent",
            vec![read_call("recent", "src/new.rs")],
        ));
        items.push(result("recent", &big("recent", 70)));
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        assert!(
            checkpoint
                .stubs
                .iter()
                .any(|stub| stub.call_id == "old" && stub.reason == StubReason::AgedOut)
        );
        assert!(!checkpoint.stubs.iter().any(|stub| stub.call_id == "recent"));
    }

    #[test]
    fn min_reclaim_gate_blocks_small_prunes() {
        let items = vec![
            Item::User {
                text: "go".to_owned(),
            },
            assistant_calls("read", vec![read_call("r1", "src/a.rs")]),
            result("r1", "small"),
            assistant_text("done"),
        ];
        let session = session_with(items);
        // A trigger low enough to fire but a reclaim floor the small
        // transcript cannot meet.
        let config = PruneConfig {
            trigger_tokens: 10,
            min_reclaim_tokens: 20_000,
            ..PruneConfig::default()
        };
        let compiled = compiler(config)
            .compile(input(&session, None))
            .expect("compiles");
        assert!(compiled.prune_checkpoint.is_none());
    }

    #[test]
    fn frozen_stubs_stay_byte_stable_across_appends() {
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..8 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("old{n}"), "src/a.rs")],
            ));
            items.push(result(&format!("old{n}"), &big("dup", 25)));
        }
        items.push(assistant_text("done"));
        let session = session_with(items.clone());
        let first = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = first.prune_checkpoint.clone().expect("triggered");

        // Append turns; pass the checkpoint back in (as the loop does).
        items.push(assistant_calls("more", vec![read_call("late", "src/b.rs")]));
        items.push(result("late", &big("late", 5)));
        items.push(assistant_text("done again"));
        let grown = session_with(items);
        let second = compiler(PruneConfig::default())
            .compile(input(&grown, Some(&checkpoint)))
            .expect("compiles");
        // Every frozen stub is still applied verbatim.
        for message in &second.request.messages {
            let Message::ToolResult(result) = message else {
                continue;
            };
            let Some(stub) = checkpoint
                .stubs
                .iter()
                .find(|stub| stub.call_id == result.call_id.as_str())
            else {
                continue;
            };
            assert_eq!(result.content, stub.reason.stub_text());
        }
    }

    #[test]
    fn ageout_stubs_every_unprotected_read_not_just_the_floor() {
        // Regression (2L live run): age-out must prune *all* read-only
        // results outside the protected tail, not merely enough to clear
        // `min_reclaim_tokens`. Stopping at the floor re-stubbed the same
        // oldest reads every trigger while newer bulk stayed, so the
        // projection grew without bound.
        let mut items = vec![Item::User {
            text: "go".to_owned(),
        }];
        for n in 0..10 {
            items.push(assistant_calls(
                "reading",
                vec![read_call(&format!("r{n}"), &format!("src/f{n}.rs"))],
            ));
            items.push(result(&format!("r{n}"), &big("read", 30)));
        }
        items.push(assistant_text("done"));
        let session = session_with(items);
        let compiled = compiler(PruneConfig::default())
            .compile(input(&session, None))
            .expect("compiles");
        let checkpoint = compiled.prune_checkpoint.expect("triggered");
        let aged = checkpoint
            .stubs
            .iter()
            .filter(|stub| stub.reason == StubReason::AgedOut)
            .count();
        // Ten 30 KB reads: only the protected tail (last few) may survive,
        // and the old floor-limited behaviour would have stubbed ~2.
        assert!(
            aged >= 5,
            "expected most old reads aged out, only {aged} stubbed"
        );
        assert!(
            !checkpoint.stubs.iter().any(|stub| stub.call_id == "r9"),
            "the newest read is inside the protected tail"
        );
    }

    fn messages_contain(request: &crate::model::ModelRequest, needle: &str) -> bool {
        request
            .messages
            .iter()
            .any(|message| messages_serialized(std::slice::from_ref(message)).contains(needle))
    }
}
