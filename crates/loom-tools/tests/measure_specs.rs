//! Serialized tool-spec budget (2K).
//!
//! The tool specs are re-sent with every request and dominate a short
//! task's context, so their serialized size is a budgeted quantity like the
//! output budgets in `budget.rs`. The dump below (`--nocapture`) attributes
//! the bytes per tool; the assertions keep the total from drifting back up
//! one description at a time. If an edit must grow a spec past a budget,
//! raise it here in the same commit and say why in the benchmark record.

use loom_kernel::tool::Tool;
use loom_tools::{
    ArtifactReadTool, EditFileTool, ListDirTool, ReadFileTool, RunCommandTool, SearchTool,
    VerifyTool, WriteFileTool,
};

/// Sum of `serde_json::to_string(spec)` over all eight shipped tools.
/// Action fusion (2M) adds a `verify` argument (an inlined check/test
/// enum) to `write_file` and `edit_file`, ~280 B each. Multi-ecosystem
/// verify (roadmap: verification beyond cargo) adds the `ecosystem`
/// parameter and its enum schema to `verify`, ~290 B. The sandboxed
/// shell (roadmap item 2) grows `run_command`'s description ~140 B —
/// any-program semantics must state the sandbox contract. Multi-edit
/// (roadmap item 5) gives `edit_file` a nested `edits` array schema,
/// ~+790 B: the batch contract has to be legible in the spec. The
/// output filters (roadmap item 3) add a compacting note to
/// `run_command`'s description, ~+100 B — the model must know
/// well-shaped output may arrive reduced with an artifact refetch.
/// The budget rose to 6,250 B with that; per-tool budgets keep every
/// other tool honest.
const TOTAL_SPEC_BUDGET_BYTES: usize = 6_250;
/// No single tool's serialized spec may exceed this, except those with
/// an explicit entry in [`SPEC_BUDGETS`].
const PER_SPEC_BUDGET_BYTES: usize = 840;
/// Per-tool overrides, for tools whose schema itself (not prose) is the
/// cost: a nested batch schema cannot be shortened, only justified.
const SPEC_BUDGETS: &[(&str, usize)] = &[("edit_file", 1_700)];

#[test]
fn serialized_specs_stay_within_budget() {
    let run = RunCommandTool::new();
    let verify = VerifyTool::new();
    let tools: Vec<&dyn Tool> = vec![
        &ReadFileTool,
        &ListDirTool,
        &SearchTool,
        &WriteFileTool,
        &EditFileTool,
        &run,
        &verify,
        &ArtifactReadTool,
    ];
    let mut total = 0;
    for tool in tools {
        let spec = tool.spec();
        let json = serde_json::to_string(spec).expect("spec serializes");
        println!(
            "{}: total={} desc={} params={}",
            spec.name,
            json.len(),
            spec.description.len(),
            serde_json::to_string(&spec.parameters)
                .expect("params serialize")
                .len()
        );
        let budget = SPEC_BUDGETS
            .iter()
            .find(|(name, _)| *name == spec.name)
            .map(|(_, budget)| *budget)
            .unwrap_or(PER_SPEC_BUDGET_BYTES);
        assert!(
            json.len() <= budget,
            "{} serialized spec is {} bytes (budget {budget}); \
             shorten its description or field docs",
            spec.name,
            json.len()
        );
        total += json.len();
    }
    println!("TOTAL_BYTES={total}");
    assert!(
        total <= TOTAL_SPEC_BUDGET_BYTES,
        "serialized tool specs total {total} bytes (budget {TOTAL_SPEC_BUDGET_BYTES})"
    );
}
