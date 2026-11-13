use std::sync::Arc;

use super::census::census;
use super::effort::effort_for_turn;
use super::projection::{exposed_tools, project_messages};
use crate::context::{
    CompiledContext, ContextError, ContextInput, HeuristicEstimator, TokenEstimator,
};
use crate::model::ModelRequest;

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

impl crate::context::ContextCompiler for DefaultContextCompiler {
    fn compile(&self, input: ContextInput<'_>) -> Result<CompiledContext, ContextError> {
        let (messages, facts_tokens) =
            project_messages(input.session, input.config, input.facts, &*self.estimator);
        let tools = exposed_tools(input.tools, input.capabilities);
        let stable_prefix_messages = Some(messages.len());
        let request = ModelRequest {
            model: input.model.to_owned(),
            messages,
            tools,
            max_tokens: input
                .capabilities
                .max_output_tokens
                .map(|tokens| tokens.min(input.budget.reserved_output_tokens)),
            temperature: None,
            stable_prefix_messages,
            // Copied, never interpreted: server-side editing changes
            // what the provider sends the model, not this projection —
            // the census and budget check below stay hint-blind.
            context_editing: input.config.context_editing.clone(),
            reasoning_effort: effort_for_turn(&input),
            session_id: Some(input.session.id.to_string()),
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
            // The default compiler never decides stubs, so it never
            // freezes a checkpoint and has nothing to attribute.
            prune_report: None,
            // This compiler never stubs: every projected result carries
            // its full content, so no call id is pruned away.
            stubbed_calls: Vec::new(),
        })
    }
}
