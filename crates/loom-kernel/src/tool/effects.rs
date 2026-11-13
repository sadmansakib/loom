//! What a tool call changed outside Loom, recorded at execution time and
//! never declared by the model (ADR-012).

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactRef;

/// One external command a tool ran, recorded at execution time.
///
/// Part of [`ToolEffects`]; kept structured — not a rendered shell
/// string — so later policy (dependency scheduling, change→failure
/// linkage, cache invalidation) can reason over it without re-parsing.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEffect {
    /// The executable name (bare, never a path).
    pub program: String,
    /// The arguments, one entry each.
    pub args: Vec<String>,
    /// Process exit code, or `None` when the platform reported none
    /// (e.g. terminated by signal).
    pub exit_code: Option<i32>,
}

impl CommandEffect {
    /// Records one executed command and its exit status.
    pub fn new(program: impl Into<String>, args: Vec<String>, exit_code: Option<i32>) -> Self {
        Self {
            program: program.into(),
            args,
            exit_code,
        }
    }
}

/// The side effects one tool call had on the outside world.
///
/// Recorded by the tool at execution time, never declared by the model
/// and never inferred after the fact (ADR-012, Invariant 12). Read-only
/// tools return [`ToolEffects::none`]. Paths are workspace-relative and
/// `/`-separated so a record is stable across platforms.
///
/// `before_images` carries the per-path pre-image the tool captured
/// before mutating (roadmap gap 1, tier 2): the file's content as it was
/// immediately before this call, content-addressed into the artifact
/// store. It is restore metadata, not a side effect of its own — like
/// `repairs`, it never makes `is_empty` false, and pre-image bodies are
/// part of the GC live set so rewind can trust them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEffects {
    /// Files created or overwritten, workspace-relative.
    pub files_written: Vec<String>,
    /// Files deleted, workspace-relative.
    pub files_deleted: Vec<String>,
    /// External commands executed.
    pub commands_run: Vec<CommandEffect>,
    /// Loom-internal repairs applied to a slightly malformed call before
    /// or during execution (e.g. a whitespace-padded `old_string` that
    /// only matched after trimming). Notes for the record, not side
    /// effects — they never invalidate caches or count as writes.
    #[serde(default)]
    pub repairs: Vec<String>,
    /// Per-path pre-images captured before the call mutated, keyed by the
    /// same path strings the tool recorded in `files_written` /
    /// `files_deleted`. A path with no entry was not captured (run
    /// predates tier 2, or no artifact store was attached) — restore
    /// refuses for that path rather than guessing.
    #[serde(default)]
    pub before_images: Vec<BeforeImage>,
    /// The verify-call autofix outcome (roadmap item 33), when this call
    /// attempted one: `MachineApplicable` suggestions applied after a
    /// failing verify, gated by a re-run. Like `repairs`, the record
    /// never makes `is_empty` true on its own — the file writes it kept
    /// already do. Additive (schema-era persisted effects deserialize
    /// without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autofix: Option<AutofixEffect>,
}

/// One autofix attempt's outcome record (roadmap item 33): what was
/// applied, what the never-worse gate saw, and whether the edits were
/// kept or reverted. Mirrors [`EventKind::AutofixApplied`]'s payload;
/// the agent loop emits the event from this record.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutofixEffect {
    /// Machine-applicable edits applied to disk before the gate.
    pub edits: u32,
    /// Distinct workspace files those edits touched.
    pub files: u32,
    /// Error+warning diagnostic count of the failing run.
    pub diagnostics_before: u32,
    /// Diagnostic count after the re-run (`<= before` when kept).
    pub diagnostics_after: u32,
    /// Whether the edits survived the never-worse gate.
    pub kept: bool,
}

impl AutofixEffect {
    /// Records one attempt's outcome. Mirrors [`EventKind::AutofixApplied`]'s
    /// payload; the agent loop emits the event from this record.
    #[must_use]
    pub fn new(
        edits: u32,
        files: u32,
        diagnostics_before: u32,
        diagnostics_after: u32,
        kept: bool,
    ) -> Self {
        Self {
            edits,
            files,
            diagnostics_before,
            diagnostics_after,
            kept,
        }
    }
}

/// One file's content immediately before the call that recorded it, or
/// that the file did not exist at all (roadmap gap 1, tier 2:
/// `docs/rewind-design.md` §4).
///
/// `content` is the content-addressed pre-image body, or `None` when
/// the file was absent before the call — restoring a boundary then
/// deletes the file rather than writing anything. Captured by the tool
/// at execution time from the file it is about to modify, never
/// inferred (ADR-012); reverse-replaying serialized tool arguments
/// would silently break on whitespace repairs and out-of-band command
/// effects, which is why this is storage, not reconstruction.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeforeImage {
    /// The path this pre-image describes, in the same string form the
    /// tool recorded in `files_written` / `files_deleted`.
    pub path: String,
    /// The pre-image body, or `None` when the file did not exist before
    /// the matching mutation.
    pub content: Option<ArtifactRef>,
}

impl BeforeImage {
    /// Records a path that did not exist before the call (restoring
    /// deletes it).
    pub fn absent(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: None,
        }
    }

    /// Records the pre-image body captured before the call mutated.
    pub fn captured(path: impl Into<String>, content: ArtifactRef) -> Self {
        Self {
            path: path.into(),
            content: Some(content),
        }
    }
}

impl ToolEffects {
    /// No effects — the read-only default.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether the call changed anything outside Loom's own state.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files_written.is_empty()
            && self.files_deleted.is_empty()
            && self.commands_run.is_empty()
    }

    /// Records a file the call created or overwrote.
    #[must_use]
    pub fn with_file_written(mut self, path: impl Into<String>) -> Self {
        self.files_written.push(path.into());
        self
    }

    /// Records a file the call deleted.
    #[must_use]
    pub fn with_file_deleted(mut self, path: impl Into<String>) -> Self {
        self.files_deleted.push(path.into());
        self
    }

    /// Records a command the call executed.
    #[must_use]
    pub fn with_command(mut self, command: CommandEffect) -> Self {
        self.commands_run.push(command);
        self
    }

    /// Records a repair Loom applied to this call's arguments before or
    /// during execution.
    #[must_use]
    pub fn with_repair(mut self, note: impl Into<String>) -> Self {
        self.repairs.push(note.into());
        self
    }

    /// Records one pre-image captured before this call mutated a path
    /// (tier-2 rewind metadata, not a side effect itself).
    #[must_use]
    pub fn with_before_image(mut self, image: BeforeImage) -> Self {
        self.before_images.push(image);
        self
    }

    /// Records an autofix attempt's outcome (roadmap item 33). The last
    /// record wins when a call somehow attempts more than one.
    #[must_use]
    pub fn with_autofix(mut self, autofix: AutofixEffect) -> Self {
        self.autofix = Some(autofix);
        self
    }

    /// Folds `other`'s records into `self` (order-preserving appends;
    /// `other`'s autofix record wins when set). The fused-verify path
    /// composes its own effects with the host tool's through this.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.files_written.extend(other.files_written);
        self.files_deleted.extend(other.files_deleted);
        self.commands_run.extend(other.commands_run);
        self.repairs.extend(other.repairs);
        self.before_images.extend(other.before_images);
        if other.autofix.is_some() {
            self.autofix = other.autofix;
        }
        self
    }
}
