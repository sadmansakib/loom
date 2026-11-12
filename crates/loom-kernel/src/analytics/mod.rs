//! Transcript analytics: per-tool call quality from a persisted
//! [`Session`](crate::session::Session) (roadmap: efficiency eval phase 2, micro-evals).
//!
//! Everything here is derived from the durable transcript alone — the
//! same data any replay sees — so the numbers are reproducible offline
//! and identical whether computed live, from a stored session, or from
//! an eval recording. What the transcript does not carry (usage, cache
//! hit/miss, prune decisions) is deliberately *not* estimated here:
//! those live in the live-run reports only, and claiming them from a
//! transcript would be dishonest accounting.
//!
//! The split the stats encode:
//!
//! * **Model-side faults** (`unknown_tool`, `not_exposed`,
//!   `invalid_input`): the model emitted a call the harness could not
//!   honor as-is — a malformed-signal rate, the number a call-repair
//!   layer (roadmap) would have to fix.
//! * **Execution faults** (`execution`, `timeout`): the program ran and
//!   failed — legitimate signal for the model, but a high rate still
//!   marks wasted turns.
//! * **`repeated`**: the identical-call ledger refused a re-dispatch —
//!   pure waste the ledger already prevented; also counted in
//!   `identical_repeats`, which additionally counts *executed*
//!   identical calls the ledger cannot see (same name + canonical
//!   arguments, but not back-to-back).
//! * **`edit_apply`**: attempts vs applied for the edit/write family —
//!   the model-side patch acceptance rate.
//! * **`skills`**: per-skill `load_skill` telemetry — body loads,
//!   bundled-resource reads, failures, and bytes delivered into
//!   context — the usage-frequency and context-cost evidence the
//!   skill lifecycle rule (retire low-value skills on evidence)
//!   needs.

mod mechanisms;
mod session;
mod skills;
mod stats;

pub use mechanisms::{
    MechanismStats, mechanisms_json, mechanisms_line, mechanisms_overview, session_mechanisms,
};
pub use session::{SessionOverview, SessionsOverview, session_stats, sessions_overview};
pub use skills::{
    MAX_DELIVERED_BYTES_EVALUATED, MIN_DELIVERED_BYTES_EVALUATED, MIN_SUCCESSFUL_CALLS_EVALUATED,
    SkillStats, SkillVerdict, skill_retirement_verdicts,
};
pub use stats::{EditApply, SessionStats, Spill, ToolCounts};

#[cfg(test)]
mod tests;
