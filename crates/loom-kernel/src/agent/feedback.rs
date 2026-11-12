//! Evidence-shaped loop feedback (§13; roadmap items 1, 8, 30): the
//! zero-progress and budget nudges, the failure ledger, the edit-stall
//! breaker, and the completion gate with its refusal prompts.

use super::Agent;
use super::TaskError;
use super::error::persistence;
use super::summary::Accounting;
use crate::event::{EventKind, EventSink};
use crate::id::TaskId;
use crate::session::Session;
use crate::transcript::{AssistantMessage, Item, ToolCall, ToolResult, ToolStatus};
use std::collections::{BTreeMap, HashSet};

/// The zero-progress nudge: consecutive turns without a successful
/// mutating tool call after which the loop tells the model to stop
/// exploring and land its patch.
///
/// Turn-count based on purpose. A *semantic* stuck-detector (repeated
/// reasoning, unchanged plan) fires while the model is still working and
/// its false positives cost more than the nudge buys; counting turns
/// since the workspace last changed cannot misfire that way, and the
/// nudge itself only adds a message — it never refuses a call.
pub const ZERO_PROGRESS_TURNS: u32 = 12;

/// The malformed-arguments ceiling: consecutive turns in which *every*
/// tool call was a salvaged (unparseable) call after which the task
/// fails instead of looping. One malformed call is fed back to the
/// model as an error result — the field-standard recovery loop — but a
/// model that cannot produce a well-formed call at all is stuck, not
/// recovering, and burning turns on it helps nobody.
pub const MALFORMED_ARGUMENT_TURNS_LIMIT: u32 = 3;

/// The failed-call loop breaker (2026-10-31): consecutive turns in
/// which every tool call *failed* (well-formed but rejected — wrong
/// shape, denied, unusable) before a corrective nudge fires. Unlike
/// unparseable arguments, these are recoverable — the same model
/// self-corrected mid-run — so the first response is a nudge quoting
/// the error, not task failure.
pub const FAILED_CALL_TURNS_NUDGE: u32 = 5;

/// The edit-stall threshold (roadmap item 30): consecutive failed
/// edit-family calls on one file after which the loop injects the
/// file's symbol outline as context. Two, because the failure ledger
/// already refuses the *identical* retry — a second distinct failure
/// on the same file is a shape problem, not bad luck.
pub const EDIT_STALL_TURNS: u32 = 2;
/// Consecutive all-failed turns that fail the task outright: a model
/// still failing every call after the corrective nudge is stuck.
/// (The 13398 oracle run burned 130 of 250 turns here.)
pub const FAILED_CALL_TURNS_LIMIT: u32 = 10;

/// Whether the zero-progress nudge is due: enough turns have passed
/// without a change, and this stall has not been nudged yet.
pub(super) fn zero_progress_due(turns_since_mutation: u32, nudged: bool) -> bool {
    !nudged && turns_since_mutation >= ZERO_PROGRESS_TURNS
}

/// The share of the turn budget after which the loop tells the model how
/// much of it it has spent (three fifths).
pub(super) const LATE_BUDGET_NUMERATOR: u32 = 3;
pub(super) const LATE_BUDGET_DENOMINATOR: u32 = 5;

/// Whether the turn-budget nudge is due: the run has spent this share of
/// its turns and has not been told yet.
///
/// [`zero_progress_due`] counts turns since the last successful mutating
/// call, so it can never fire while the model keeps editing — every
/// successful edit resets its counter, and a run that churns through
/// edits hears nothing at all. This notice counts turns only and fires
/// at most once, while a change of approach is still affordable.
pub(super) fn late_budget_due(turn: u32, max_turns: u32, nudged: bool) -> bool {
    !nudged && turn * LATE_BUDGET_DENOMINATOR >= max_turns * LATE_BUDGET_NUMERATOR
}

impl Agent {
    /// The refusal half of the completion gate (roadmap item 8): when
    /// files changed since the last green check and the refusal cap
    /// allows it, commit the assistant's reply, append a refusal as a
    /// normal user item — persisted, projected, and answered like any
    /// other turn, quoting the changed paths and the original task —
    /// and let the loop take one more turn. The gate loops: every
    /// unverified edit re-arms it until green or the cap. Returns
    /// `true` when the refusal fired; the caller then continues
    /// instead of finishing.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn refuse_completion_once(
        &self,
        session: &mut Session,
        task_id: TaskId,
        turn: u32,
        completion: &mut CompletionGate,
        accounting: &mut Accounting,
        assistant: &AssistantMessage,
        sink: &dyn EventSink,
    ) -> Result<bool, TaskError> {
        // Pending verification takes precedence: the green-check gap is
        // the sharper failure. The test-consultation nudge (2026-10-23)
        // fires on the next EndTurn if the task still never ran tests.
        let pending_refusal = completion.pending() && completion.can_refuse();
        let test_refusal = completion.untested() && !completion.test_refused;
        let oracle_refusal = completion.unproven() && !completion.oracle_refused;
        if !self.loop_shaping
            || turn >= self.max_turns
            || !(pending_refusal || test_refusal || oracle_refusal)
        {
            return Ok(false);
        }
        let text = if pending_refusal {
            completion.refusals += 1;
            completion_refusal(session, completion.refusals)
        } else if test_refusal {
            completion.test_refused = true;
            test_consultation_refusal(session)
        } else {
            completion.oracle_refused = true;
            acceptance_oracle_refusal(session)
        };
        sink.emit(task_id, EventKind::LoopNudge { text: text.clone() });
        self.commit(session, assistant.clone(), Vec::new())?;
        let reminder = Item::User { text };
        if let Some(store) = &self.store {
            store
                .append_item(&session.id, session.next_index(), &reminder)
                .map_err(persistence)?;
        }
        session
            .push_item(reminder)
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        accounting.turns = turn;
        accounting.sync(session, task_id);
        Ok(true)
    }
}

/// Tracks the calls that failed during one task so the loop can refuse an
/// exact repeat (§13). Cleared whenever any tool call succeeds, because a
/// new success means the workspace may have changed and a previously failing
/// call may now be legitimate (e.g. write a file, then edit it).
#[derive(Default)]
pub(super) struct FailureLedger {
    pub(super) failed: HashSet<(String, String)>,
}

impl FailureLedger {
    /// Whether `call` exactly repeats a recorded failure with no success since.
    pub(super) fn refuses(&self, call: &ToolCall) -> bool {
        self.failed.contains(&ledger_key(call))
    }

    /// Records a failed call.
    pub(super) fn record(&mut self, call: &ToolCall) {
        self.failed.insert(ledger_key(call));
    }

    /// Forgets every failure: some tool call has succeeded since.
    pub(super) fn clear(&mut self) {
        self.failed.clear();
    }
}

/// Identity of a call for the failure ledger: tool name plus canonical
/// argument JSON. Object keys are byte-sorted by
/// [`crate::canonical::canonical_json`], so equality does not depend on
/// serde_json's map iteration order — which the build graph can flip to
/// insertion order when any package (agent-client-protocol does)
/// enables the `preserve_order` feature. Without this, two calls
/// differing only in key order would key differently and evade the
/// refusal.
pub(super) fn ledger_key(call: &ToolCall) -> (String, String) {
    (
        call.name.clone(),
        crate::canonical::canonical_json(&call.arguments),
    )
}

/// Per-file edit-stall state (roadmap item 30): consecutive failed
/// edit-family calls per workspace-relative path. Any success on the
/// path clears it — the stall is over, whatever else the batch did.
#[derive(Default)]
pub(super) struct EditStalls {
    pub(super) consecutive_failures: BTreeMap<String, u32>,
}

impl EditStalls {
    /// Learns from one finished call, exactly like the completion gate
    /// and failure ledger do.
    pub(super) fn note(&mut self, call: &ToolCall, result: &ToolResult) {
        let Some(path) = edit_target(call) else {
            return;
        };
        match result.status {
            ToolStatus::Success => {
                self.consecutive_failures.remove(&path);
            }
            ToolStatus::Error(_) => {
                *self.consecutive_failures.entry(path).or_default() += 1;
            }
            ToolStatus::Cancelled { .. } => {}
        }
    }

    /// The first path at or past [`EDIT_STALL_TURNS`], if any. BTreeMap
    /// order makes the pick deterministic when several files stall.
    pub(super) fn stalled_file(&self) -> Option<(String, u32)> {
        self.consecutive_failures
            .iter()
            .find(|(_, count)| **count >= EDIT_STALL_TURNS)
            .map(|(path, count)| (path.clone(), *count))
    }

    /// Forgets a stall the loop has answered.
    pub(super) fn resolved(&mut self, path: &str) {
        self.consecutive_failures.remove(path);
    }
}

/// The workspace-relative `path` argument of an edit-family call, when
/// it carries one. Refused repeats and malformed calls never reach
/// [`EditStalls::note`]'s caller with different shapes — every tool in
/// the family names its target `path`.
pub(super) fn edit_target(call: &ToolCall) -> Option<String> {
    const EDIT_TOOLS: [&str; 3] = ["edit_file", "write_file", "replace_symbol"];
    if !EDIT_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    call.arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Completion-gate state for one task (roadmap item 8).
///
/// Tracks whether any file mutations are outstanding — changed after the
/// last green check the model ran. "Green check" is detected from the
/// result shape every command-bearing tool shares (`$ cmd` first, an
/// `exit status: 0` line), covering `verify`, `run_command`, and the fused
/// edit check alike. The refusal loops until green, capped by
/// [`COMPLETION_REFUSAL_CAP`].
#[derive(Default)]
pub(super) struct CompletionGate {
    /// Mutation epoch: bumped on every committed file write or delete.
    pub(super) mutations: u64,
    /// The epoch that had a green check; `None` when nothing ever passed.
    pub(super) verified_at: Option<u64>,
    /// The epoch that had a green run of the FULL suite (bare runner,
    /// no file filters). A narrow green (`pytest one_file.py`) passes
    /// while an untouched regression fails elsewhere — the exact
    /// pytest-7490 hard-mix loss (2026-10-31 N=20): FAIL_TO_PASS 2/2
    /// green, one PASS_TO_PASS regressed.
    pub(super) suite_green_at: Option<u64>,
    /// How many unverified-mutation refusals fired (capped by
    /// [`COMPLETION_REFUSAL_CAP`]: the gate loops until green, but a
    /// model that edits past three refusals without a check is
    /// stalling, not verifying).
    pub(super) refusals: u8,
    /// Whether any committed result ran a test-like command this task.
    pub(super) tested: bool,
    /// Whether the one-shot test-consultation refusal already fired.
    pub(super) test_refused: bool,
    /// Whether the task mutated any test-shaped file. A test the agent
    /// itself authored is the cheapest executable oracle for new
    /// behavior (Agentless/Syncause reproduce-first, 2026-10-31).
    pub(super) test_mutation: bool,
    /// Non-test files this task wrote, so a later green command that
    /// executes one of them counts as an agent-authored check.
    pub(super) authored: Vec<String>,
    /// Whether an agent-authored check satisfied the oracle: a mutated
    /// test file, or a green command executing a file the task wrote.
    pub(super) oracle_ok: bool,
    /// Whether the one-shot acceptance-oracle refusal already fired.
    pub(super) oracle_refused: bool,
}

impl CompletionGate {
    /// Learns one committed tool result.
    pub(super) fn note(&mut self, result: &ToolResult) {
        if !result.effects.files_written.is_empty() || !result.effects.files_deleted.is_empty() {
            self.mutations += 1;
        }
        if result
            .effects
            .files_written
            .iter()
            .any(|path| is_test_path(path))
        {
            self.test_mutation = true;
        }
        for path in &result.effects.files_written {
            if !is_test_path(path) && !self.authored.contains(path) {
                self.authored.push(path.clone());
            }
        }
        if green_check(result) {
            self.verified_at = Some(self.mutations);
            if runs_full_suite(&result.content) {
                self.suite_green_at = Some(self.mutations);
            }
            let command = result
                .content
                .lines()
                .find(|line| line.starts_with("$ "))
                .unwrap_or_default();
            if self
                .authored
                .iter()
                .any(|path| command.contains(path.as_str()))
            {
                self.oracle_ok = true;
            }
        }
        if command_runs_tests(&result.content) {
            self.tested = true;
        }
    }

    /// Whether unverified mutations are outstanding: the FULL suite
    /// must be green at the current epoch — a narrow green does not
    /// settle (pytest-7490 loss, 2026-10-31).
    pub(super) fn pending(&self) -> bool {
        self.mutations > 0 && self.suite_green_at != Some(self.mutations)
    }

    /// Whether files changed but no test-like command ran (2026-10-23
    /// test-consultation nudge).
    pub(super) fn untested(&self) -> bool {
        self.mutations > 0 && !self.tested
    }

    /// Whether mutations exist that no agent-authored check exercises.
    /// The project's own suite predates the change and passes green on
    /// wrong code — only a check the task itself authored can falsify
    /// new behavior.
    pub(super) fn unproven(&self) -> bool {
        self.mutations > 0 && !self.test_mutation && !self.oracle_ok
    }

    /// Whether the unverified-mutation refusal may still fire.
    pub(super) fn can_refuse(&self) -> bool {
        self.refusals < COMPLETION_REFUSAL_CAP
    }
}

/// Whether a path looks like a test file by standard conventions:
/// a `tests`/`test` directory segment, or a `test_*`/`*_test.*` basename.
pub(super) fn is_test_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|segment| segment == "tests" || segment == "test")
        || basename.starts_with("test_")
        || basename.contains("_test.")
}

/// Ceiling on unverified-mutation refusals per task: the gate loops
/// until green, but bounded — three nudges that each end in another
/// unverified edit mean the model is stalling, not verifying.
pub(super) const COMPLETION_REFUSAL_CAP: u8 = 3;

pub(super) fn green_check(result: &ToolResult) -> bool {
    if !matches!(result.status, ToolStatus::Success) {
        return false;
    }
    let mut command_shaped = false;
    for line in result.content.lines() {
        if line.starts_with("$ ") {
            command_shaped = true;
        }
        if line.trim() == "exit status: 0" {
            return command_shaped;
        }
    }
    false
}

/// Whether a committed result's transcript shows a test-like command:
/// any `$ …` line mentioning "test" (case-insensitive). Recall is
/// favored over precision — `pytest`, `go test`, `cargo test`,
/// `npm test`, `python tests/test_x.py`, jest `-Dtest`, and a stray
/// "latest" all match — because the consumer is a once-only advisory
/// nudge, and exit status is irrelevant: a failing suite still means
/// the tests were consulted.
pub(super) fn command_runs_tests(content: &str) -> bool {
    content
        .lines()
        .filter(|line| line.starts_with("$ "))
        .any(|line| line[2..].to_ascii_lowercase().contains("test"))
}

/// Whether a result shows a green run of the FULL suite: a recognized
/// test runner invoked bare (env prefixes allowed) with at most
/// directory arguments — no file filters, no `-k`, no node ids. This
/// is the settle condition for [`CompletionGate`]: `pytest
/// testing/test_skipping.py` passing while an untouched PASS_TO_PASS
/// test regresses is the exact pytest-7490 miss (2026-10-31 N=20).
/// Whole-directory arguments (`pytest tests/`) still count as full.
pub(super) fn runs_full_suite(content: &str) -> bool {
    /// Command heads that run a project's suite.
    const RUNNERS: &[&[&str]] = &[
        &["pytest"],
        &["py.test"],
        &["python", "-m", "pytest"],
        &["python3", "-m", "pytest"],
        &["python", "-m", "unittest"],
        &["cargo", "test"],
        &["go", "test"],
        &["npm", "test"],
        &["yarn", "test"],
        &["tox"],
        &["make", "test"],
        &["jest"],
    ];
    content
        .lines()
        .filter(|line| line.starts_with("$ "))
        .any(|line| {
            let tokens: Vec<&str> = line[2..].split_whitespace().collect();
            // Skip leading env assignments (`FOO=1 pytest …`).
            let rest = tokens
                .iter()
                .position(|token| !token.contains('='))
                .map(|start| &tokens[start..])
                .unwrap_or(&[]);
            let runner = RUNNERS.iter().copied().find(|head| rest.starts_with(head));
            match runner {
                // Every remaining argument must be a whole directory —
                // anything else (a file, -k, ::node) narrows the run.
                Some(head) => rest[head.len()..].iter().all(|token| token.ends_with('/')),
                None => false,
            }
        })
}

/// The one-shot refusal text: changed paths plus the original task,
/// head-bounded so an oversized instruction cannot balloon the turn.
pub(super) fn completion_refusal(session: &Session, attempt: u8) -> String {
    let paths = recent_changes(session);
    let instruction = session
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let instruction_head = instruction.chars().take(800).collect::<String>();
    format!(
        "Hold on (verification reminder {attempt} of {COMPLETION_REFUSAL_CAP}) — these files \
         changed since the last full-suite green check: {}. Before finishing: (1) review your \
         full diff (`git diff`) hunk by hunk against the original task and revert anything the \
         task did not ask for — reworded messages, formatting, unrelated files; (2) run the \
         FULL suite bare (e.g. `pytest` / `cargo test` at the repo root, no file filters, no \
         -k) and fix what fails anywhere in the suite, not just in the file you changed; (3) \
         keep HEAD on the base commit — never stash, checkout, or commit; every change lives \
         in the working tree so the final diff applies cleanly to the original state. If every \
         hunk is task-scoped and the full suite passes, the task is complete. \
         For a Python repo the full-suite command is exactly: pytest -q (from the repo root, \
         nothing else on the command line). Run that one command now and paste its tail. \
         Original task: {}",
        paths.join(", "),
        instruction_head,
    )
}

/// The one-shot test-consultation nudge (2026-10-23): the task changed
/// files but no command-shaped result ever looked like a test run. Three
/// of four undiagnosed 100-sweep misses were exactly this — the failing
/// assertion lived in a pre-existing in-repo test file the run never
/// consulted. Style-mates [`completion_refusal`]: changed paths plus the
/// original task, head-bounded.
pub(super) fn test_consultation_refusal(session: &Session) -> String {
    let paths = recent_changes(session);
    let instruction = session
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let instruction_head = instruction.chars().take(800).collect::<String>();
    format!(
        "Hold on — these files changed during this task, but no existing \
         test suite was run: {}. Before finishing, run the affected \
         module's existing tests (the project's usual test command over \
         the touched paths) and address any failure your change caused. \
         If a failure predates your change, note that and finish. If the \
         project has no tests to run, finish. Original task: {}",
        paths.join(", "),
        instruction_head,
    )
}

/// The acceptance-oracle refusal (2026-10-31): mutations exist, checks
/// ran green, but nothing the agent itself authored exercises the new
/// behavior — the project's suite predates the change and passes green
/// on wrong code (astropy 13398: 63 old tests green, 8 hidden ITRS
/// tests red). Agentless/Syncause reproduce-first: demand a minimal
/// check derived from the task's own acceptance criteria. One-shot:
/// a model that ignores it once will not write a better oracle nagged.
pub(super) fn acceptance_oracle_refusal(session: &Session) -> String {
    let paths = recent_changes(session);
    let instruction = session
        .transcript
        .iter()
        .find_map(|item| match item {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let instruction_head = instruction.chars().take(800).collect::<String>();
    format!(
        "Hold on — these files changed and existing checks pass, but no check you \
         wrote yourself exercises the new behavior: {}. The project's own suite \
         predates your change and stays green on wrong code. Before finishing: \
         (1) derive the task's acceptance criteria (the concrete expected \
         behavior stated or implied in the task, e.g. an example input/output); \
         (2) write a minimal throwaway check for it — a test file or a small \
         script is fine; (3) run it and fix whatever fails; a fresh check that \
         passes on old code and fails before your fix is the ideal. If the task \
         genuinely has no observable behavior change (pure refactor, formatting, \
         docs), say so in one line and finish. Original task: {}",
        paths.join(", "),
        instruction_head,
    )
}

/// Workspace-relative paths the task has mutated so far (2F, ADR-017).
///
/// Derived from the recorded [`ToolEffects`](crate::tool::ToolEffects) on
/// each committed tool result — never diffed and never model-declared — so
/// verification links a failure to what the loop actually changed. Deduped
/// oldest-first and capped, most recent retained. Effects from calls in the
/// *current* batch are appended to the shared [`ToolContext`] list as those
/// calls finish (`ToolContext::note_effects`), covering same-batch writes.
pub(super) fn recent_changes(session: &Session) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for item in &session.transcript {
        let Item::ToolResult(result) = item else {
            continue;
        };
        for path in result
            .effects
            .files_written
            .iter()
            .chain(result.effects.files_deleted.iter())
        {
            if seen.insert(path.clone()) {
                paths.push(path.clone());
            }
        }
    }
    if paths.len() > crate::tool::MAX_RECENT_CHANGES {
        paths.drain(..paths.len() - crate::tool::MAX_RECENT_CHANGES);
    }
    paths
}
