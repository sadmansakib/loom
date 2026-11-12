use crate::model::{Message, ModelCapabilities};
use crate::tool::{ToolEffects, ToolRegistry};
use crate::transcript::{Item, ToolResult};

use crate::context::{AgentConfig, KnownFacts, TokenEstimator, estimate_serialized};

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
pub(super) fn project_messages(
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
    // Todo blocks (roadmap item 8 phase B) project like any other user
    // item: every block, in append order. Eliding superseded blocks
    // would rewrite the prefix mid-task and forfeit provider cache hits
    // on everything past the elision point (§8.1.1) — far more
    // expensive than the few hundred mostly-cached tokens a stale
    // block costs. The newest block stays authoritative by recency.
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
pub(super) fn exposed_tools(
    registry: &ToolRegistry,
    capabilities: &ModelCapabilities,
) -> Vec<crate::tool::ToolSpec> {
    if !capabilities.tool_calls {
        return Vec::new();
    }
    registry.specs().cloned().collect()
}
