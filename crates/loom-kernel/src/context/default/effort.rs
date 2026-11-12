use crate::model::ReasoningEffort;
use crate::transcript::Item;

use crate::context::{ContextInput, ReasoningSchedule};

/// Resolves this request's reasoning-effort intent (roadmap item 9):
/// `None` whenever the model cannot reason or no level applies.
///
/// [`ReasoningSchedule::Fixed`] copies the configured level (default
/// `None` — byte-off). [`ReasoningSchedule::Progressive`] runs High for
/// the session's first `open_turns` assistant turns and on the first
/// request after a `verify` tool call (the post-verification
/// accept/iterate decision), Low in between. The level rides the
/// request, never the message list, so the stable prefix and the
/// prompt-cache decisions stay blind to it.
pub(super) fn effort_for_turn(input: &ContextInput<'_>) -> Option<ReasoningEffort> {
    if !input.capabilities.reasoning {
        return None;
    }
    match input.config.reasoning_schedule {
        ReasoningSchedule::Fixed => input.config.reasoning_effort,
        ReasoningSchedule::Progressive { open_turns } => {
            let assistant_turns = input
                .session
                .transcript
                .iter()
                .filter(|item| matches!(item, Item::Assistant(_)))
                .count();
            if assistant_turns < usize::from(open_turns) {
                return Some(ReasoningEffort::High);
            }
            // The last assistant turn decides: if it called `verify`,
            // this request is the post-verification decision turn.
            let verify_pending = input.session.transcript.iter().rev().find_map(|item| {
                if let Item::Assistant(assistant) = item {
                    Some(
                        assistant
                            .tool_calls
                            .iter()
                            .any(|call| &*call.name == "verify"),
                    )
                } else {
                    None
                }
            });
            if verify_pending == Some(true) {
                Some(ReasoningEffort::High)
            } else {
                Some(ReasoningEffort::Low)
            }
        }
    }
}
