//! Deterministic failure-to-fix lesson mining (roadmap item 20.4, the
//! open half after the skill retirement rule).
//!
//! Everything here is derived from durable transcripts alone — the same
//! data [`session_stats`](crate::analytics::session_stats) reads — so the
//! lessons are reproducible offline and identical whether mined live,
//! from stored sessions, or from eval recordings. No model authorship is
//! involved: a lesson is a counted recovery pattern, not prose a model
//! invented.
//!
//! A *recovery* is a `Success` result for the same tool, within
//! [`RECOVERY_WINDOW`] tool results after a failure for that tool in the
//! same session. A pattern becomes a [`MinedLesson`] only when it is
//! recurring: at least [`MIN_OCCURRENCES`] failures, at least
//! [`MIN_RECOVERIES`] of them recovered, across at least [`MIN_SESSIONS`]
//! distinct sessions. The thresholds are published constants, like the
//! skill-retirement rule's, so the rule is reviewable and tests pin the
//! boundaries.
//!
//! Staleness and eviction are structural: lessons are re-derived on every
//! `loom lessons` run and entries that no longer meet the thresholds
//! disappear (re-validated or evicted, never kept by age). The facts seam
//! re-validates the evidence counts on read and drops anything that no
//! longer clears the bar, so a hand-edited or stale store cannot inject
//! dead rules into a live task.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::transcript::{Item, ToolFailure, ToolStatus};

/// How many tool results after a failure a same-tool success may arrive
/// to count as its recovery. Bounded so a success far later (unrelated
/// work) cannot retroactively bless an old failure.
pub const RECOVERY_WINDOW: usize = 8;

/// A failure pattern must recur at least this many times (across the
/// mined sessions) to become a lesson — a single failure is an anecdote.
pub const MIN_OCCURRENCES: u64 = 3;

/// At least this many of the pattern's failures must be followed by a
/// same-tool success — the contrast evidence the lesson carries.
pub const MIN_RECOVERIES: u64 = 2;

/// The pattern must be observed in at least this many distinct sessions,
/// so one pathological session cannot mint a workspace-wide rule.
pub const MIN_SESSIONS: u64 = 2;

/// Maximum lessons rendered into one facts block (mirrors the skills and
/// list caps: the block stays bounded regardless of history size).
pub const MAX_LESSONS: usize = 16;

/// One mined failure-to-fix rule: a counted recovery pattern, in name
/// order across a freshly mined set.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinedLesson {
    /// Tool the pattern was observed on (e.g. `edit_file`).
    pub tool: String,
    /// Failure kind the pattern keyed on (serde snake_case of
    /// [`ToolFailure`], e.g. `invalid_input`).
    pub failure: String,
    /// Short procedural rule, templated from the failure kind — never
    /// freeform prose, never a claim about mechanism beyond the counted
    /// recovery.
    pub statement: String,
    /// Total failures of this (tool, failure) across the mined sessions.
    pub occurrences: u64,
    /// Failures followed by a same-tool success within
    /// [`RECOVERY_WINDOW`] in the same session.
    pub recoveries: u64,
    /// Distinct sessions the pattern was observed in.
    pub sessions: u64,
}

impl MinedLesson {
    /// Builds a mined lesson from its parts (the miner and the store's
    /// reader both use this constructor).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool: impl Into<String>,
        failure: impl Into<String>,
        statement: impl Into<String>,
        occurrences: u64,
        recoveries: u64,
        sessions: u64,
    ) -> Self {
        Self {
            tool: tool.into(),
            failure: failure.into(),
            statement: statement.into(),
            occurrences,
            recoveries,
            sessions,
        }
    }

    /// The evidence suffix the facts block renders beside the rule.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "{} failures, {} recovered, {} sessions",
            self.occurrences, self.recoveries, self.sessions
        )
    }

    /// Whether the evidence still clears the mining thresholds. The
    /// facts seam re-checks this on read so a stale or edited store
    /// cannot keep a dead rule alive (re-validated or evicted).
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.occurrences >= MIN_OCCURRENCES
            && self.recoveries >= MIN_RECOVERIES
            && self.sessions >= MIN_SESSIONS
            && self.recoveries <= self.occurrences
    }
}

/// One (tool, failure) bucket accumulated across sessions.
#[derive(Default)]
struct Bucket {
    occurrences: u64,
    recoveries: u64,
    sessions: BTreeSet<usize>,
}

/// Mined failure-to-fix lessons over `sessions`, in (tool, failure)
/// name order, capped at [`MAX_LESSONS`].
///
/// Pure: one pass per session, no I/O, no clocks. Call-to-result joining
/// is by call id, exactly as
/// [`session_stats`](crate::analytics::session_stats) joins them, so
/// cancelled calls (no outcome) never count and only committed history
/// is evidence.
#[must_use]
pub fn mine_lessons(sessions: &[Session]) -> Vec<MinedLesson> {
    let mut buckets: BTreeMap<(String, String), Bucket> = BTreeMap::new();

    for (session_index, session) in sessions.iter().enumerate() {
        // Call id -> tool name, joined the same way analytics joins.
        let mut calls: HashMap<&str, &str> = HashMap::new();
        // (tool, status) in result order, for the recovery scan below.
        let mut results: Vec<(String, Result<(), ToolFailure>)> = Vec::new();

        for item in &session.transcript {
            match item {
                Item::User { .. } => {}
                Item::Assistant(assistant) => {
                    for call in &assistant.tool_calls {
                        calls.insert(call.id.as_str(), &call.name);
                    }
                }
                Item::ToolResult(result) => {
                    let Some(tool) = calls.get(result.call_id.as_str()) else {
                        continue;
                    };
                    match &result.status {
                        ToolStatus::Success => results.push(((*tool).to_owned(), Ok(()))),
                        ToolStatus::Error(failure) => {
                            results.push(((*tool).to_owned(), Err(*failure)))
                        }
                        ToolStatus::Cancelled { .. } => {}
                    }
                }
            }
        }

        for (index, (tool, outcome)) in results.iter().enumerate() {
            let Err(failure) = outcome else { continue };
            let bucket = buckets
                .entry((tool.clone(), failure_name(*failure).to_owned()))
                .or_default();
            bucket.occurrences += 1;
            // A recovery is a same-tool success close behind this failure.
            let recovered = results[index + 1..]
                .iter()
                .take(RECOVERY_WINDOW)
                .any(|(other_tool, other_outcome)| other_outcome.is_ok() && other_tool == tool);
            if recovered {
                bucket.recoveries += 1;
            }
            bucket.sessions.insert(session_index);
        }
    }

    buckets
        .into_iter()
        .filter_map(|((tool, failure), bucket)| {
            let lesson = MinedLesson {
                statement: lesson_statement(&tool, &failure),
                tool,
                failure,
                occurrences: bucket.occurrences,
                recoveries: bucket.recoveries,
                sessions: bucket.sessions.len() as u64,
            };
            lesson.is_live().then_some(lesson)
        })
        .take(MAX_LESSONS)
        .collect()
}

/// The snake_case name [`ToolFailure`] serializes to — the failure key
/// stored on disk, so the facts seam can re-validate it without the enum.
#[must_use]
pub fn failure_name(failure: ToolFailure) -> &'static str {
    match failure {
        ToolFailure::UnknownTool => "unknown_tool",
        ToolFailure::NotExposed => "not_exposed",
        ToolFailure::InvalidInput => "invalid_input",
        ToolFailure::Execution => "execution",
        ToolFailure::Timeout => "timeout",
        ToolFailure::RepeatedCall => "repeated_call",
    }
}

/// Whether `name` is a failure kind a lesson may key on.
#[must_use]
pub fn is_known_failure(name: &str) -> bool {
    matches!(
        name,
        "unknown_tool"
            | "not_exposed"
            | "invalid_input"
            | "execution"
            | "timeout"
            | "repeated_call"
    )
}

/// Human-readable failure label for statements.
fn failure_label(failure: &str) -> &str {
    match failure {
        "unknown_tool" => "unknown-tool",
        "not_exposed" => "not-exposed",
        "invalid_input" => "invalid input",
        "execution" => "execution",
        "timeout" => "timeout",
        "repeated_call" => "repeated call",
        _ => failure,
    }
}

/// One short procedural rule per pattern, templated from the failure
/// kind: the rule names the observed failure class and the recovery that
/// followed it. It asserts nothing beyond the counted contrast.
fn lesson_statement(tool: &str, failure: &str) -> String {
    match failure {
        "invalid_input" => format!(
            "For {tool} invalid-input failures: correct the arguments and reissue — \
             a same-tool success followed in past sessions."
        ),
        "unknown_tool" | "not_exposed" => {
            format!(
                "For {tool} {label} failures: use only tools named in the current \
                 tool list, then reissue — a same-tool success followed in past sessions.",
                label = failure_label(failure)
            )
        }
        "repeated_call" => format!(
            "For {tool} repeated-call refusals: change the arguments before \
             reissuing — a same-tool success followed in past sessions."
        ),
        "timeout" => format!(
            "For {tool} timeouts: retry with a narrower request and reissue — \
             a same-tool success followed in past sessions."
        ),
        "execution" => format!(
            "For {tool} execution failures: read the error, narrow the target, \
             and reissue — a same-tool success followed in past sessions."
        ),
        _ => format!(
            "For {tool} {label} failures: change the approach and reissue — \
             a same-tool success followed in past sessions.",
            label = failure_label(failure)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionId;
    use crate::model::StopReason;
    use crate::transcript::{AssistantMessage, ToolArguments, ToolCall, ToolCallId, ToolResult};

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::try_from(id.to_owned()).expect("call id"),
            name: name.to_owned(),
            arguments: ToolArguments::try_from(serde_json::json!({})).expect("args"),
        }
    }

    fn result(id: &str, status: ToolStatus) -> ToolResult {
        ToolResult::new(
            ToolCallId::try_from(id.to_owned()).expect("call id"),
            status,
            "x",
            false,
        )
    }

    fn session_with(tools: Vec<(&str, ToolStatus)>) -> Session {
        // One assistant turn per call keeps call/result pairs explicit.
        let mut transcript = Vec::new();
        for (index, (tool, status)) in tools.iter().enumerate() {
            let id = format!("c{index}");
            transcript.push(Item::Assistant(AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call(&id, tool)],
                stop_reason: StopReason::ToolCalls,
            }));
            transcript.push(Item::ToolResult(result(&id, *status)));
        }
        Session {
            id: SessionId::new(),
            transcript,
            tasks: Vec::new(),
            todos: crate::session::TodoList::default(),
        }
    }

    fn err(failure: ToolFailure) -> ToolStatus {
        ToolStatus::Error(failure)
    }

    // Three sessions, each with one invalid_input edit_file failure
    // recovered by a same-tool success — the minimal qualifying pattern.

    fn qualifying_sessions() -> Vec<Session> {
        (0..3)
            .map(|_| {
                session_with(vec![
                    ("edit_file", err(ToolFailure::InvalidInput)),
                    ("edit_file", ToolStatus::Success),
                ])
            })
            .collect()
    }

    #[test]
    fn mines_a_recurring_recovered_pattern() {
        let sessions = qualifying_sessions();
        let lessons = mine_lessons(&sessions);
        assert_eq!(lessons.len(), 1);
        let lesson = &lessons[0];
        assert_eq!(lesson.tool, "edit_file");
        assert_eq!(lesson.failure, "invalid_input");
        assert_eq!(lesson.occurrences, 3);
        assert_eq!(lesson.recoveries, 3);
        assert_eq!(lesson.sessions, 3);
        assert!(lesson.statement.contains("edit_file"));
        assert!(lesson.is_live());
        assert_eq!(lesson.evidence(), "3 failures, 3 recovered, 3 sessions");
    }

    #[test]
    fn single_session_or_single_occurrence_does_not_mine() {
        // One session only: sessions threshold fails.
        let one = vec![session_with(vec![
            ("edit_file", err(ToolFailure::InvalidInput)),
            ("edit_file", ToolStatus::Success),
        ])];
        assert!(mine_lessons(&one).is_empty());

        // Three failures but none recovered within the window.
        let unrecovered: Vec<Session> = (0..3)
            .map(|_| {
                session_with(vec![
                    ("edit_file", err(ToolFailure::InvalidInput)),
                    ("read_file", ToolStatus::Success),
                ])
            })
            .collect();
        assert!(mine_lessons(&unrecovered).is_empty());
    }

    #[test]
    fn recovery_outside_the_window_does_not_count() {
        // Success comes RECOVERY_WINDOW+1 results later: no recovery.
        let mut tools = vec![("edit_file", err(ToolFailure::InvalidInput))];
        for _ in 0..RECOVERY_WINDOW {
            tools.push(("read_file", ToolStatus::Success));
        }
        tools.push(("edit_file", ToolStatus::Success));
        let sessions = vec![session_with(tools.clone()), session_with(tools)];
        let lessons = mine_lessons(&sessions);
        assert!(
            lessons.is_empty(),
            "late success must not count: {lessons:?}"
        );
    }

    #[test]
    fn lessons_are_name_ordered_and_deterministic() {
        let mut sessions = qualifying_sessions();
        // Three read_file timeout sessions, for the second qualifying pattern.
        for _ in 0..3 {
            sessions.push(session_with(vec![
                ("read_file", err(ToolFailure::Timeout)),
                ("read_file", ToolStatus::Success),
            ]));
        }
        let first = mine_lessons(&sessions);
        let second = mine_lessons(&sessions);
        assert_eq!(first, second);
        let names: Vec<(&str, &str)> = first
            .iter()
            .map(|l| (l.tool.as_str(), l.failure.as_str()))
            .collect();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], ("edit_file", "invalid_input"));
        assert_eq!(names[1], ("read_file", "timeout"));
    }

    #[test]
    fn failure_names_cover_every_variant() {
        for failure in [
            ToolFailure::UnknownTool,
            ToolFailure::NotExposed,
            ToolFailure::InvalidInput,
            ToolFailure::Execution,
            ToolFailure::Timeout,
            ToolFailure::RepeatedCall,
        ] {
            assert!(
                is_known_failure(failure_name(failure)),
                "{}",
                failure_name(failure)
            );
        }
        assert!(!is_known_failure("nope"));
        assert!(!is_known_failure(""));
    }

    #[test]
    fn evidence_and_liveness_boundaries() {
        let lesson = MinedLesson {
            tool: "edit_file".to_owned(),
            failure: "invalid_input".to_owned(),
            statement: "s".to_owned(),
            occurrences: MIN_OCCURRENCES,
            recoveries: MIN_RECOVERIES,
            sessions: MIN_SESSIONS,
        };
        assert!(lesson.is_live());
        assert_eq!(lesson.evidence(), "3 failures, 2 recovered, 2 sessions");

        // Below any threshold: dead.
        let mut dead = lesson.clone();
        dead.occurrences = MIN_OCCURRENCES - 1;
        assert!(!dead.is_live());
        dead = lesson.clone();
        dead.recoveries = MIN_RECOVERIES - 1;
        assert!(!dead.is_live());
        dead = lesson.clone();
        dead.sessions = MIN_SESSIONS - 1;
        assert!(!dead.is_live());
        // Recoveries cannot exceed occurrences.
        dead = lesson.clone();
        dead.recoveries = dead.occurrences + 1;
        assert!(!dead.is_live());
    }

    #[test]
    fn cancelled_calls_never_count() {
        let sessions = vec![
            session_with(vec![
                ("edit_file", err(ToolFailure::InvalidInput)),
                ("edit_file", ToolStatus::Cancelled { started: true }),
            ]),
            session_with(vec![
                ("edit_file", err(ToolFailure::InvalidInput)),
                ("edit_file", ToolStatus::Cancelled { started: false }),
            ]),
            session_with(vec![
                ("edit_file", err(ToolFailure::InvalidInput)),
                ("edit_file", ToolStatus::Cancelled { started: true }),
            ]),
        ];
        assert!(mine_lessons(&sessions).is_empty());
    }
}
