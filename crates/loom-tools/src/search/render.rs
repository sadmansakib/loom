//! Rendering `search` matches: raw `path:line: text` lines below the
//! summary threshold, a per-file summary above it.

use std::collections::HashMap;

use super::Match;
use super::cues::cap_cue;

/// Cap on each reported matching line: a file that is one huge line
/// (minified JS, lockfiles) would otherwise buffer up to
/// [`MAX_FILE_BYTES`] per match — 200 matches ≈ 200 MiB — before the
/// engine's 64 KiB output bound ever sees it.
pub(super) const MAX_MATCH_LINE_BYTES: usize = 256;
/// Above this many matches, results are summarized per file (count plus
/// a preview) instead of listing every matching line — the tool result is
/// the dominant input-token cost (ADR-014, V8).
///
/// Kept high: the default `max_results` cap already bounds a raw listing
/// (200 matches at ≤256 bytes each). Summarizing sooner hides hits, which
/// pushed the model into extra refine/re-search turns on B1 — the summary
/// should only replace a listing too large to present, not a routine one.
pub(super) const SUMMARY_THRESHOLD: usize = 100;
/// How many matching lines to show per file in the summarized form.
const PER_FILE_PREVIEW: usize = 5;

/// Renders matches: `path:line: text` lines when the set is small, or a
/// per-file summary (count plus a short preview each) above
/// [`SUMMARY_THRESHOLD`]. A search stopped at `max_results` carries an
/// explicit marker rather than silently dropping the remainder (V9).
pub(super) fn render(
    matches: &[Match],
    truncated: bool,
    max_results: usize,
    cues: &HashMap<String, HashMap<usize, String>>,
) -> (String, bool) {
    if matches.is_empty() {
        return (String::new(), false);
    }
    if matches.len() <= SUMMARY_THRESHOLD {
        let mut text = matches
            .iter()
            .map(
                |m| match cues.get(&m.rel).and_then(|file| file.get(&m.line)) {
                    Some(cue) => format!("{}:{}: {}  [in {}]", m.rel, m.line, m.text, cap_cue(cue)),
                    None => format!("{}:{}: {}", m.rel, m.line, m.text),
                },
            )
            .collect::<Vec<_>>()
            .join("\n");
        if truncated {
            text.push('\n');
            text.push_str(&cap_hint(max_results));
        }
        return (text, truncated);
    }
    // Over the threshold: one block per file, in first-seen (walk) order,
    // so the file that matched first is the one the model reads first.
    let mut files: Vec<(&str, Vec<&Match>)> = Vec::new();
    for found in matches {
        match files.iter_mut().find(|(name, _)| *name == found.rel) {
            Some((_, group)) => group.push(found),
            None => files.push((found.rel.as_str(), vec![found])),
        }
    }
    let mut lines = vec![format!(
        "{} matches in {} {}{}",
        matches.len(),
        files.len(),
        if files.len() == 1 { "file" } else { "files" },
        if truncated {
            format!(" (stopped at the {max_results}-match cap)")
        } else {
            String::new()
        }
    )];
    let mut shown = 0usize;
    for (name, group) in &files {
        lines.push(format!("{name} ({} matches)", group.len()));
        for found in group.iter().take(PER_FILE_PREVIEW) {
            match cues.get(*name).and_then(|file| file.get(&found.line)) {
                Some(cue) => lines.push(format!(
                    "  {}: {}  [in {}]",
                    found.line,
                    found.text,
                    cap_cue(cue)
                )),
                None => lines.push(format!("  {}: {}", found.line, found.text)),
            }
            shown += 1;
        }
    }
    let hidden = matches.len() - shown;
    if hidden > 0 {
        lines.push(format!(
            "... [{hidden} more matches; refine the pattern or pass path to narrow the search]"
        ));
    }
    (lines.join("\n"), hidden > 0 || truncated)
}

/// The marker appended when a search stops at its `max_results` cap.
fn cap_hint(max_results: usize) -> String {
    format!(
        "... [stopped at the {max_results}-match cap; refine the pattern or pass path to narrow the search]"
    )
}

/// Truncates a matched line to [`MAX_MATCH_LINE_BYTES`] on a UTF-8
/// char boundary, marking the cut with an ellipsis. Short lines are
/// returned verbatim.
pub(super) fn capped_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_owned();
    }
    let mut cut = MAX_MATCH_LINE_BYTES;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &line[..cut])
}
