//! The capsule contract and answer extraction: what the child receives
//! and what the parent reads back.

use loom_kernel::session::Session;
use loom_kernel::transcript::Item;

use super::config::ANSWER_BUDGET_BYTES;

/// The capsule task the child receives: goal, scope, constraints, and
/// the output contract — everything a bounded delegation needs.
pub fn capsule(question: &str, focus: Option<&str>) -> String {
    let focus = focus
        .map(|hint| format!("Focus hint from the caller: {hint}\n"))
        .unwrap_or_default();
    format!(
        "You are a read-only scout. Answer ONE question about this \
         workspace so a parent agent can proceed without reading the \
         files itself.\n\n\
         {focus}\
         Question: {question}\n\n\
         Constraints: you may read, list, and search files only — no \
         edits, no shell commands. Explore efficiently; you have a \
         small turn budget.\n\n\
         Output contract: a concise answer (at most ~150 words) with \
         file:line pointers for every claim. No preamble, no plan \
         narration — just the answer."
    )
}

/// The scout's answer: the last committed assistant text, truncated to
/// the byte budget on a char boundary with an explicit marker so a
/// clipped answer is always legible as clipped.
pub fn final_answer(session: &Session) -> String {
    let text = session
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Assistant(message) => Some(message.content.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    truncate_answer(text)
}

/// Byte-bounds a scout answer on a char boundary.
pub(super) fn truncate_answer(text: &str) -> String {
    if text.len() <= ANSWER_BUDGET_BYTES {
        return text.to_owned();
    }
    let mut end = ANSWER_BUDGET_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[…scout answer truncated at 1 KB]", &text[..end])
}
