//! The standard SWE-bench task prompt: the problem statement
//! verbatim, the repository context, and the working-tree contract
//! the patch extraction relies on — frozen like every other prompt.

use super::dataset::SweInstance;
#[cfg(test)]
use super::dataset::{fixture_jsonl, parse_dataset};

/// The standard SWE-bench task prompt: the problem statement verbatim,
/// the repository context, and the working-tree contract the patch
/// extraction relies on. Frozen like every other prompt — changing it
/// invalidates every baseline recorded against it. (v2, 2026-10-21:
/// added the iterate-early, run-existing-tests, and scratch-cleanup
/// directives after the first 5-instance sweep; v1 baseline = 3/5
/// resolved on astropy. v3, 2026-10-22: added the reproduce-and-verify
/// and scope-minimality directives after the 25-instance deepseek-flash
/// sweep diagnosed two process misses; v2 baseline = 23/25. Both v3
/// rules mirror the field-standard prompts — SWE-agent's
/// always-replicate-first tip and the standard harness's
/// minimal-changes-to-non-tests step.) v5, 2026-10-31: the repro rule now
/// demands the issue's example verbatim — same nesting, same definitions —
/// after sphinx-9281 died to __qualname__ on a test-local enum the model's
/// simplified repro (module-level class) could never expose.
#[must_use]
pub fn build_prompt(instance: &SweInstance) -> String {
    prompt_text(
        &instance.repo,
        &instance.problem_statement,
        &instance.hints_text,
    )
}

/// The prompt body shared by every dataset (Verified today, the
/// Multilingual Rust slice tomorrow): the frozen v5 text needs only the
/// repo, the issue, and the hints, so sibling datasets reuse it
/// byte-identical instead of forking the contract.
pub(crate) fn prompt_text(repo: &str, problem_statement: &str, hints_text: &str) -> String {
    let mut prompt = format!(
        "You are an autonomous software engineer fixing a real issue in a GitHub repository.\n\n\
         Repository: {repo}\n\
         The repository is checked out at the issue's base commit in your working \
         directory. You have file tools and a shell: explore the code and edit the \
         source to fix it.\n\n\
         Rules:\n\
         - Plan before editing: reproduce the bug, name the exact faulty \
         code path in one or two sentences, and say what change will fix it \
         and why — then edit. Planning-first beats direct issue-to-patch on \
         hard tasks (planning→repair studies, 2025).\n\
         - Fix the root cause in the source; do not paper over the symptom.\n\
         - Fix exactly the reported defect: no new restrictions, errors on \
         previously working inputs, API changes, or renames unless the issue \
         explicitly asks for them. Behavior the issue does not mention must \
         keep working.\n\
         - Once you have a workable hypothesis, start editing and iterate from \
         there; do not keep exploring toward certainty — you cannot verify the \
         fix by reading alone.\n\
         - Reproduce the reported behavior with a minimal script — the issue's \
         own example when it has one — that fails (nonzero exit) while the bug \
         is present; after the fix, run it again and confirm it passes and that \
         the behavior matches what the issue says is correct, including any \
         exact expected values it states. Copy the issue's example exactly — \
         define classes and functions where the issue defines them (a class \
         nested in a function behaves differently from a module-level one: \
         __qualname__, name mangling, scoping); do not simplify its shape.\n\
         - Before finishing, run the repository's existing tests that cover the \
         files you changed, and make sure they still pass.\n\
         - Delete any scratch or reproduction scripts you created along the way; \
         only the fix itself should remain in the tree.\n\
         - Do not commit, branch, or tag; leave your changes in the working tree.\n\
         - Do not modify or delete the .git directory.\n\n\
         The issue follows.\n\n\
         <problem_statement>\n{problem_statement}\n</problem_statement>\n",
    );
    if !hints_text.trim().is_empty() {
        use std::fmt::Write as _;
        let _ = write!(prompt, "\n<hints>\n{}\n</hints>\n", hints_text.trim());
    }
    prompt.push_str(
        "\nWhen the fix is complete, reply with a short summary of what you changed and why, and finish — do not wait for further input.\n",
    );
    prompt
}

#[test]
fn prompt_carries_the_problem_and_the_tree_contract() {
    let instances = parse_dataset(&fixture_jsonl());
    let prompt = build_prompt(&instances[0]);
    assert!(prompt.contains("Repository: org/one"));
    assert!(prompt.contains("It crashes."));
    assert!(prompt.contains("<problem_statement>"));
    assert!(prompt.contains("leave your changes in the working tree"));
    // v2 directives: iterate early, run existing tests, clean scratch.
    assert!(prompt.contains("start editing and iterate"));
    assert!(prompt.contains("existing tests that cover the"));
    assert!(prompt.contains("Delete any scratch or reproduction scripts"));
    assert!(prompt.contains("and finish — do not wait for further input"));
    // v3 directives: scope minimality, reproduce-and-verify.
    assert!(prompt.contains("no new restrictions"));
    assert!(prompt.contains("that fails (nonzero exit) while the bug"));
    assert!(prompt.contains("including any exact expected values it states"));
    // No hints in the fixture → no hints block.
    assert!(!prompt.contains("<hints>"));

    let mut hinted = instances[1].clone();
    hinted.hints_text = "see docs/x".to_owned();
    assert!(build_prompt(&hinted).contains("<hints>\nsee docs/x"));
}
