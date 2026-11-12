//! Output rendering: the success line and the numbered region echo the
//! model reads to confirm the edit landed.

use crate::budget::{self, ToolBudgets};

use super::matching::EditOutcome;

/// The `edited …` success line: one file gets the occurrence count,
/// several get the per-file rollup.
pub(super) fn edit_detail(resolved: &[(String, String)], per_file_counts: &[usize]) -> String {
    if resolved.len() == 1 {
        let (relative, _) = &resolved[0];
        format!(
            "{relative}: replaced {} occurrence{}",
            per_file_counts[0],
            if per_file_counts[0] == 1 { "" } else { "s" }
        )
    } else {
        let summary = resolved
            .iter()
            .zip(per_file_counts)
            .map(|((relative, _), count)| format!("{relative} ×{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} edits across {} files ({summary})",
            per_file_counts.iter().sum::<usize>(),
            resolved.len()
        )
    }
}

/// Renders the edited regions from the final file state: one numbered
/// snippet per edit, ±`edit_echo_context_lines` lines, capped by the echo
/// byte budget. Returns the text and whether the budget cut anything.
pub(super) fn render_echo(outcome: &EditOutcome, budgets: &ToolBudgets) -> (String, bool) {
    let mut joined = String::new();
    for (file_idx, start, len) in &outcome.spans {
        let (relative, text) = &outcome.files[*file_idx];
        let total_lines = text.lines().count().max(1);
        let line_of =
            |byte: usize| -> usize { 1 + text[..byte.min(text.len())].matches('\n').count() };
        let first_line = line_of(*start);
        let mut last_line = line_of(start + len);
        if *len > 0 && text[..(start + len).min(text.len())].ends_with('\n') {
            last_line -= 1;
        }
        let last_line = last_line.max(first_line);
        let ctx_first = first_line
            .saturating_sub(budgets.edit_echo_context_lines)
            .max(1);
        let ctx_last = (last_line + budgets.edit_echo_context_lines).min(total_lines);
        joined.push_str(&format!(
            "{relative}: lines {first_line}-{last_line} of {total_lines}\n"
        ));
        for (number, line) in text.lines().enumerate().take(ctx_last).skip(ctx_first - 1) {
            joined.push_str(&format!("{}\t{line}\n", number + 1));
        }
    }
    if joined.is_empty() {
        return (String::new(), false);
    }
    let half = budgets.edit_echo_max_bytes / 2;
    budget::head_tail(&joined, half, half)
}
