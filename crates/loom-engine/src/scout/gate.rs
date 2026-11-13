//! The deterministic weak-answer gate and the `file:line` citation
//! machinery it guards (FrugalGPT-shaped, docs/subagents-design.md §3).

use std::fmt::Write as _;

use super::run::{ScoutOutcome, Status};

/// The deterministic weak-answer gate (`FrugalGPT`'s quality check with
/// code heuristics instead of a learned scorer — conservative recall
/// by design, per docs/subagents-design.md §3): timeouts, failures,
/// empty answers, answers that hit the truncation budget, and answers
/// that violate the capsule's `file:line` contract all escalate once.
pub(super) fn weak_reason(outcome: &ScoutOutcome) -> Option<&'static str> {
    match &outcome.status {
        Status::TimedOut => Some("timed out"),
        Status::Failed(_) => Some("a failed run"),
        Status::Cancelled => None, // parent intent; a re-run would cancel too
        Status::Answered => {
            if outcome.answer.trim().is_empty() {
                Some("an empty answer")
            } else if outcome.answer.contains("[…scout answer truncated at 1 KB]") {
                Some("truncated at the answer budget")
            } else if !has_file_line_pointer(&outcome.answer) {
                Some("missing the file:line pointers the capsule contract requires")
            } else {
                None
            }
        }
    }
}

/// One whitespace token, stripped of common surrounding punctuation
/// (including trailing periods), judged as a `path:line` citation.
pub(super) fn pointer_token(token: &str) -> Option<&str> {
    let mut token = token.trim_matches(|c: char| {
        matches!(
            c,
            ',' | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '!'
                | '?'
                | '"'
                | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
        )
    });
    // Trailing periods are sentence punctuation; interior dots stay
    // (file extensions).
    while token.ends_with('.') {
        token = &token[..token.len() - 1];
    }
    match token.rsplit_once(':') {
        Some((path, line)) => {
            // The path half must look like a name, not a bare number —
            // keeps "12:30" (a time) and "3:16" (a verse) out.
            (!path.is_empty()
                && !path.chars().all(|c| c.is_ascii_digit())
                && !line.is_empty()
                && line.chars().all(|c| c.is_ascii_digit()))
            .then_some(token)
        }
        None => None,
    }
}

/// True when the text cites at least one `file:line`-shaped pointer —
/// a colon followed by ASCII digits inside a token. Deliberately
/// cheap: the capsule demands pointers for every claim, so zero
/// matches is a contract violation, not a style note.
pub(super) fn has_file_line_pointer(text: &str) -> bool {
    text.split_whitespace()
        .any(|token| pointer_token(token).is_some())
}

/// All `file:line` pointers an answer cites (deterministic extraction
/// for the cross-check agreement note).
pub(super) fn cited_pointers(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(pointer_token)
        .map(str::to_owned)
        .collect()
}

/// The deterministic cross-check note: which citations both models
/// make, and which only one does. The parent synthesizes; this is the
/// evidence, not a verdict.
pub(super) fn agreement_note(primary: &str, strong: &str) -> String {
    use std::collections::HashSet;
    let cap = 6;
    let a: HashSet<String> = cited_pointers(primary).into_iter().collect();
    let b: HashSet<String> = cited_pointers(strong).into_iter().collect();
    let both: Vec<&str> = a.intersection(&b).take(cap).map(String::as_str).collect();
    let only_a: Vec<&str> = a.difference(&b).take(cap).map(String::as_str).collect();
    let only_b: Vec<&str> = b.difference(&a).take(cap).map(String::as_str).collect();
    if a.is_empty() && b.is_empty() {
        return "[agreement note: neither answer cites file:line pointers]".to_owned();
    }
    let mut note = String::from("[agreement note: ");
    if both.is_empty() {
        note.push_str("no shared citations");
    } else {
        let _ = write!(note, "both cite {}", both.join(", "));
    }
    if !only_a.is_empty() {
        let _ = write!(note, "; only the primary model cites {}", only_a.join(", "));
    }
    if !only_b.is_empty() {
        let _ = write!(note, "; only the strong model cites {}", only_b.join(", "));
    }
    format!("{note}]")
}
