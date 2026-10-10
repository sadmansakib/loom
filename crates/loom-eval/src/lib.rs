//! The Loom eval harness: task fixtures, record/replay providers, outcome
//! and bugfix gates, and suite reports — driven through [`loom_core`]'s
//! `Engine` (via `EngineOverrides`), never a hand-wired `Agent`.
//!
//! Everything here is test-only by design: the harness and its assertions
//! compile under `#[cfg(test)]`, so the crate has no public surface and
//! costs nothing in a normal `cargo build`. Live runs are `#[ignore]`
//! tests behind `LOOM_EVAL_*` knobs (see `eval.rs`); the offline suite
//! (replay equivalence, gate checks, report math) runs everywhere.
//!
//! The harness owns, in `eval.rs`:
//!
//! * the task corpus in `tests/eval-tasks/<ID>/` (`task.json` per task);
//! * the record/replay seam — `RecordingProvider` wraps a real provider
//!   built by the engine, `ReplayProvider` serves a recorded script with
//!   drift detection, so offline tests exercise the exact engine path;
//! * gates — SWE-bench-shaped bugfix grading (`cargo test` fixtures with
//!   fail-to-pass / pass-to-pass sets and protected files) and outcome
//!   specs (exact files, contains, tests-green);
//! * reports — per-run JSON with token economics, per-task and suite
//!   rollups with pass rates and cost-per-solved, written under
//!   `LOOM_EVAL_DIR` (default `<repo>/target/eval`); baselines get
//!   copied into `docs/benchmarks/` by hand when recorded.
//!
//! `benchmark.rs` carries the long-running live benchmarks (read-only
//! repository inspection and friends) behind `LOOM_BENCH_*` knobs.
//!
//! The N ≥ 5 protocol (roadmap, item 1): every field number comes from
//! at least five runs of a recorded configuration, replayed or live,
//! with per-run variance reported — never a single observation.

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod editbench;
#[cfg(test)]
mod eval;
