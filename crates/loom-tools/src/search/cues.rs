//! Enclosing-symbol cues for `search` hits: one capped re-read and
//! tree-sitter parse per file with hits.

use std::collections::HashMap;
use std::path::Path;

use crate::symbols::enclosing_symbol_cues;

use super::Match;
use super::read_capped_text;

/// Cap on each appended symbol cue (`  [in Class/method]`): deep
/// name-paths in generated code would otherwise re-inflate the very
/// output the summary form exists to bound (item 31).
const CUE_MAX_BYTES: usize = 64;

/// Builds per-file enclosing-symbol maps for the match set (item 31).
/// Runs on the blocking worker: one capped re-read and one tree-sitter
/// parse per file with hits, never per line. Empty (no cues) when the
/// language is unsupported or [`symbol_cues_enabled`] is off.
pub(super) fn symbol_cues_for(
    matches: &[Match],
    root: &Path,
) -> HashMap<String, HashMap<usize, String>> {
    if !symbol_cues_enabled() {
        return HashMap::new();
    }
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for found in matches {
        by_file
            .entry(found.rel.as_str())
            .or_default()
            .push(found.line);
    }
    let mut cues = HashMap::new();
    for (rel, lines) in by_file {
        let Some(source) = read_capped_text(&root.join(rel)) else {
            continue;
        };
        if let Some(map) = enclosing_symbol_cues(Path::new(rel), &source, &lines) {
            cues.insert(rel.to_owned(), map);
        }
    }
    cues
}

/// Truncates a symbol cue to [`CUE_MAX_BYTES`] on a UTF-8 char boundary,
/// matching [`capped_line`]'s convention.
pub(super) fn cap_cue(cue: &str) -> String {
    if cue.len() <= CUE_MAX_BYTES {
        return cue.to_owned();
    }
    let mut cut = CUE_MAX_BYTES;
    while !cue.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &cue[..cut])
}

/// Whether search hits carry an enclosing-symbol cue (roadmap item 31):
/// on unless `LOOM_SEARCH_SYMBOL_CUES` is set to exactly `0`. Read per
/// call so an operator can flip it without restarting the session.
const ENV_DISABLE_CUES: &str = "LOOM_SEARCH_SYMBOL_CUES";

#[cfg(test)]
pub(super) static TEST_CUES_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn symbol_cues_enabled() -> bool {
    #[cfg(test)]
    {
        if TEST_CUES_DISABLED.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
    }
    std::env::var(ENV_DISABLE_CUES).ok().as_deref() != Some("0")
}
