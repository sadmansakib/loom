//! The Loom eval harness: SWE-bench Verified runs in container
//! isolation, scored by the official `swebench` docker harness.
//!
//! The in-house task corpus, gates, and replay machinery were removed
//! once the real benchmark arrived: measurement now happens against
//! the Verified split (500 human-validated instances) with the agent
//! running inside the official per-instance testbed image — repo at
//! `base_commit`, correct interpreter and dependencies, no access to
//! the host working tree. Grading belongs to the official harness
//! (`swebench eval verified`), which applies its own `test_patch` and
//! decides `resolved`.
//!
//! [`swebench`] holds the plumbing (dataset fetch/cache, selection,
//! prompts, container-run assembly, predictions emission); the
//! `swebench-run` binary orchestrates the sweep end to end: it builds
//! (or accepts) a linux loom binary, prepares per-instance prompts,
//! starts one container per (instance, run), collects `patch.diff`
//! plus the in-container `loom run --json` report, and writes one
//! predictions JSONL per run — the file the official scorer consumes.
//!
//! [`multilingual`] is the parallel Rust-slice path over
//! SWE-bench Multilingual (43 Rust tasks, 7 repos): same prompt
//! contract, same container isolation, a Rust-toolchain variant of the
//! frozen container script, and the row's own prebuilt image name —
//! driven by the `multilingual-run` binary and scored by
//! `swebench eval multilingual`.
//!
//! Everything in the library is offline-testable except the two
//! network edges (dataset fetch, docker), which the binary owns.

pub mod multilingual;
pub mod selfopt;
pub mod suite_audit;
pub mod swebench;
