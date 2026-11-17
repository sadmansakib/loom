//! Matching machinery: loading target files (size cap, UTF-8, CRLF/BOM
//! rescue view), unique exact matching with the whitespace-repair and
//! near-miss probes, and byte-span replacement on char boundaries.

use loom_kernel::store::ArtifactStore;
use loom_kernel::tool::{BeforeImage, ToolError};
use std::path::{Path, PathBuf};

use crate::confinement::workspace_relative;

/// Edits larger than this are refused: the whole file is read into memory and
/// rewritten, so an unbounded read is a memory spike (a large generated file
/// is the realistic case). 8 MiB covers source files with room to spare.
pub(super) const MAX_EDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Everything the caller needs after a successful batch: the success
/// line, the final text of each distinct file, and where each
/// replacement landed (file index + final byte span) for the echo.
pub(super) struct EditOutcome {
    pub(super) detail: String,
    /// `(workspace_relative, final_text)` per distinct file.
    pub(super) files: Vec<(String, String)>,
    /// `(file index, start, len)` per edit, in application order.
    pub(super) spans: Vec<(usize, usize, usize)>,
    /// Repair notes for calls Loom salvaged (e.g. a whitespace-padded
    /// `old_string` that only matched after trimming) — surfaced in the
    /// result and recorded in the effects for the session stats.
    pub(super) repairs: Vec<String>,
    /// Tier-2 rewind pre-images, one per distinct touched file, taken
    /// from the loaded originals before the writes landed.
    pub(super) before_images: Vec<BeforeImage>,
}

/// Loads every distinct target file once, enforcing the size cap and the
/// UTF-8 requirement up front.
///
/// Unlike the read tools, a missing file here is an execution error, not an
/// informational "does not exist": a write path that targets nothing is a
/// model mistake (read first, or use `write_file`), and the loop's failure
/// ledger should refuse a blind repeat (ADR-020).
/// The line-ending/BOM rescue (review item 3.1): models emit LF-only
/// `old_string`s, while checkout tools and Windows editors leave CRLF
/// (and sometimes a BOM) in files. Matching happens against a
/// normalized *view* (BOM stripped, CRLF folded to LF); the matched
/// span maps back to original-text byte offsets, so the file's own
/// endings and BOM are preserved verbatim and mixed line endings are
/// never rewritten wholesale.
pub(super) struct NormalizedView {
    /// BOM-stripped, CRLF-folded copy used only for matching.
    pub(super) view: String,
    /// View byte offset → original byte offset, one entry per view
    /// byte (multi-byte view chars advance both sides together).
    map: Vec<usize>,
    /// Whether every newline in the original is CRLF (drives whether
    /// inserted text adopts CRLF; mixed files keep the model's LF).
    pub(super) all_crlf: bool,
}

/// Builds the normalized view of `text`. Cheap single pass; called
/// only after an exact match already failed.
pub(super) fn normalized_view(text: &str) -> NormalizedView {
    let bom_len = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut view = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    let mut saw_lone_lf = bom_len > 0 && text[bom_len..].starts_with('\n');
    let mut i = bom_len;
    while let Some(c) = text.get(i..).and_then(|rest| rest.chars().next()) {
        if c == '\r' && text[i + 1..].starts_with('\n') {
            view.push('\n');
            map.push(i);
            i += 2;
            continue;
        }
        if c == '\n' {
            saw_lone_lf = true;
        }
        view.push(c);
        let len = c.len_utf8();
        for k in 0..len {
            map.push(i + k);
        }
        i += len;
    }
    map.push(text.len());
    NormalizedView {
        view,
        map,
        all_crlf: !saw_lone_lf && text.contains("\r\n"),
    }
}

impl NormalizedView {
    /// True when there is nothing to rescue (the view equals the
    /// original), so callers can skip the probe entirely.
    pub(super) fn is_identity(&self, original: &str) -> bool {
        self.view == original
    }

    /// Maps a `(start, len)` span in the view to original-text byte
    /// offsets. `len` is measured in view bytes (LF counts once).
    pub(super) fn map_span(&self, start: usize, len: usize) -> (usize, usize) {
        let original_start = self.map[start];
        let original_end = if start + len >= self.view.len() {
            // The sentinel entry maps view-end to original-end.
            self.map[self.view.len()]
        } else {
            self.map[start + len]
        };
        (original_start, original_end - original_start)
    }
}

/// Normalizes the model's `old_string` the same way (BOM strip,
/// CRLF fold) so either side of the mismatch is rescued.
pub(super) fn normalized_old(old: &str) -> String {
    let stripped = old.strip_prefix('\u{feff}').unwrap_or(old);
    stripped.replace("\r\n", "\n")
}

pub(super) fn load_edit_files(
    edits: &[(PathBuf, String, String)],
) -> Result<Vec<(PathBuf, String, String)>, ToolError> {
    let mut files: Vec<(PathBuf, String, String)> = Vec::new();
    for (path, _, _) in edits {
        if files.iter().any(|(p, _, _)| p == path) {
            continue;
        }
        let text = load_text(path)?;
        // The untouched text rides along so the syntax guard below can
        // tell a genuine breakage from a file that already failed to
        // parse before this edit.
        files.push((path.clone(), text.clone(), text));
    }
    Ok(files)
}

/// Reads one editable file under [`MAX_EDIT_FILE_BYTES`] and requires
/// UTF-8. Shared by `edit_file` and `replace_all` so both write paths
/// agree on the cap and the error text.
pub(crate) fn load_text(path: &Path) -> Result<String, ToolError> {
    // Review E3: checking metadata and then reading is a TOCTOU —
    // a file growing between the two calls loaded unbounded. Read
    // through a bounded handle instead: the limit is enforced on
    // what was actually read, not on a stat that may be stale.
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(
            std::fs::File::open(path).map_err(|err| {
                ToolError::Execution(format!("cannot read \"{}\": {err}", path.display()))
            })?,
            MAX_EDIT_FILE_BYTES + 1,
        ),
        &mut bytes,
    )
    .map_err(|err| ToolError::Execution(format!("cannot read \"{}\": {err}", path.display())))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "file is too large to edit ({} bytes; limit {MAX_EDIT_FILE_BYTES} bytes)",
            bytes.len()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution(String::from("file is not valid UTF-8")))
}

/// What a finished write reports: `(workspace_relative, final_text)`
/// per written file, plus the rewind pre-images captured before it
/// touched the disk.
pub(crate) type WriteOutcome = (Vec<(String, String)>, Vec<BeforeImage>);

/// The shared write pipeline for every tool that rewrites whole files:
/// the parse guard over the candidate set first, then the rewind
/// pre-images, then one atomic write per file. Nothing lands unless
/// every guard passes, so a batch is all-or-nothing on the guard too.
pub(crate) fn write_touched(
    root: &Path,
    files: &[(PathBuf, String, String)],
    artifacts: Option<&dyn ArtifactStore>,
) -> Result<WriteOutcome, ToolError> {
    // Syntax guard (roadmap item 5 phase B): every candidate buffer
    // must parse before anything reaches the disk, so a batch that
    // would leave any file unparsable refuses as a whole — nothing is
    // written, exactly like a failed match.
    for (path, original, text) in files {
        crate::syntax::guard_against(original, text, path).map_err(|err| {
            ToolError::InvalidInput(format!(
                "refusing to write \"{}\": {err}",
                workspace_relative(root, path)
            ))
        })?;
    }
    // Tier-2 rewind capture (roadmap gap 1): the loaded originals are
    // exactly the pre-image of every touched file — capture them before
    // the writes below, content-addressed into the artifact store.
    let before_images: Vec<BeforeImage> = files
        .iter()
        .filter_map(|(path, original, _)| {
            crate::pre_image::capture_before_image(
                artifacts,
                &workspace_relative(root, path),
                Some(original.as_bytes()),
            )
        })
        .collect();
    // Every candidate passed: write everything back. Each write
    // re-verifies confinement at use time (review item 2.2) and lands
    // atomically (review item 2.3).
    for (path, _, text) in files {
        crate::confinement::verify_parent_within(root, path)?;
        crate::atomic::atomic_write(path, text.as_bytes())
            .map_err(|err| ToolError::Execution(format!("cannot write: {err}")))?;
    }
    let resolved = files
        .iter()
        .map(|(path, _, text)| (workspace_relative(root, path), text.clone()))
        .collect();
    Ok((resolved, before_images))
}

/// Replaces `old_len` bytes at `start` with `new` in `text`, dropping
/// the file's already-recorded spans the replacement range intersects
/// (their bytes are gone — an edit whose `old_string` swallowed an
/// earlier edit's replacement), shifting the spans that sit after it,
/// and recording the replacement's own span.
///
/// This keeps every recorded span on UTF-8 char boundaries in the
/// final text, which `render_echo` byte-slices through (review E1:
/// an overlapped span used to survive with stale offsets and panic
/// the echo *after* the file had already been written).
pub(super) fn replace_span(
    text: &mut String,
    file_spans: &mut Vec<(usize, usize)>,
    start: usize,
    old_len: usize,
    new: &str,
) {
    let end = start + old_len;
    file_spans.retain(|(recorded_start, recorded_len)| {
        let recorded_end = recorded_start + recorded_len;
        recorded_end <= start || *recorded_start >= end
    });
    text.replace_range(start..end, new);
    // Offsets are file positions, far below isize::MAX on any target;
    // the signed delta only shifts recorded spans left or right.
    #[allow(clippy::cast_possible_wrap)]
    let delta = new.len() as isize - old_len as isize;
    for (recorded_start, _) in file_spans.iter_mut() {
        if start < *recorded_start {
            #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
            {
                let shifted = *recorded_start as isize + delta;
                *recorded_start = shifted as usize;
            }
        }
    }
    file_spans.push((start, new.len()));
}

/// The repair probe (roadmap item 6): models pad `old_string` with
/// incidental whitespace far more often than they genuinely misremember
/// the text. Returns the byte offset when exactly one trimmed copy
/// matches (and the trim removed something); anything else stays `None`
/// so the caller keeps the original error.
pub(super) fn trimmed_match(text: &str, old: &str) -> Option<usize> {
    let trimmed = old.trim();
    if trimmed.is_empty() || text.matches(trimmed).count() != 1 {
        return None;
    }
    text.find(trimmed)
}

/// The near-miss probe (roadmap item 6's second half): an anchor that
/// matches nothing usually means the model remembered the shape of the
/// line but not its current text. Points at where that first line still
/// occurs — or, failing that, at the closest current line — so the next
/// attempt re-reads instead of guessing again. `None` when nothing is
/// close enough, so the caller keeps its plain error.
pub(super) fn near_miss_hint(text: &str, old: &str) -> Option<String> {
    let probe = old.lines().map(str::trim).find(|line| !line.is_empty())?;
    let wanted = probe.split_whitespace().collect::<Vec<_>>().join(" ");
    if wanted.is_empty() {
        return None;
    }
    let mut occurrences = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.contains(&wanted) {
            occurrences.push(index + 1);
            if occurrences.len() == 3 {
                break;
            }
        }
    }
    if !occurrences.is_empty() {
        let lines = occurrences
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            " (its first line still occurs at line {lines}; re-read the file and copy its current text)"
        ));
    }
    let probe_len = probe.chars().count();
    let mut closest: Option<(usize, usize, &str)> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let common = probe
            .chars()
            .zip(trimmed.chars())
            .take_while(|(anchor, current)| anchor == current)
            .count();
        let shorter = probe_len.min(trimmed.chars().count());
        if common < 8 || common * 2 < shorter {
            continue;
        }
        let better = match closest {
            None => true,
            Some((best, _, _)) => common > best,
        };
        if better {
            closest = Some((common, index + 1, trimmed));
        }
    }
    closest.map(|(_, line, current)| {
        format!(
            " (the nearest line is {line}: `{current}`; re-read the file and copy its current text)"
        )
    })
}
