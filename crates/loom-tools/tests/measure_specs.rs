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
/// parameter and its enum schema to `verify`, ~290 B; the total stays
/// under the pre-compaction 5,339 B mark the 2K budget set.
const TOTAL_SPEC_BUDGET_BYTES: usize = 5_150;
/// No single tool's serialized spec may exceed this.
const PER_SPEC_BUDGET_BYTES: usize = 840;

#[test]
fn serialized_specs_stay_within_budget() {
    let run = RunCommandTool::new();
    let tools: Vec<&dyn Tool> = vec![
        &ReadFileTool,
        &ListDirTool,
        &SearchTool,
        &WriteFileTool,
        &EditFileTool,
        &run,
        &VerifyTool,
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
        assert!(
            json.len() <= PER_SPEC_BUDGET_BYTES,
            "{} serialized spec is {} bytes (budget {PER_SPEC_BUDGET_BYTES}); \
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
