//! SWE-bench Verified plumbing: dataset loading, instance selection,
//! prompt construction, container-run assembly, and predictions
//! emission for the official docker scorer.
//!
//! The measurement contract (roadmap "Measurement"): the agent runs in
//! complete isolation inside the official per-instance testbed image —
//! repo checked out at `base_commit`, correct interpreter and
//! dependencies, no access to the host working tree. Grading belongs
//! to the official `swebench` harness, which applies its own
//! `test_patch` and decides `resolved`. Loom's job ends at a
//! predictions JSONL plus per-run artifacts.
//!
//! Everything here is offline-testable except the two network edges
//! (dataset fetch, docker), which the `swebench-run` binary owns.

// A test harness: panicking on a setup failure is the intended behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod container;
mod dataset;
mod predictions;
mod prompt;
mod provenance;
mod runs;

pub use container::{
    AgentKind, CONTAINER_SCRIPT, ContainerSpec, MINI_CONTAINER_SCRIPT, RESERVED_CONTAINER_ENV,
    api_key_envs, container_script, docker_run_argv, headers_env_value, scored_image,
    testbed_image,
};
pub use dataset::{
    DATASET_ID, EXPECTED_ROWS, SweInstance, dataset_cache_path, fetch_dataset_verified,
    load_dataset_cached, parse_dataset, select_instances,
};
pub use predictions::{Prediction, select_patch, slug, write_prediction_sets, write_predictions};
pub use prompt::build_prompt;
pub use provenance::{
    PROMPT_VERSION, SweepProvenance, recorded_loom_digest, sha256_hex, span_one_build,
    sweep_provenance,
};
pub use runs::{
    RunMetrics, RunUsage, integrity_flags, run_metrics, run_outcome, run_turns, run_usage,
    scorer_outcomes, tool_calls,
};

/// Shared with the Multilingual module, whose rows use the same
/// `FAIL_TO_PASS` / `PASS_TO_PASS` encodings.
pub(crate) use dataset::string_or_vec;

/// The prompt body shared by every dataset (Verified today, the
/// Multilingual Rust slice tomorrow).
pub(crate) use prompt::prompt_text;
