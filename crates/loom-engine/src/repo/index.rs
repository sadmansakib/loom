//! The opt-in indexes the facts block can carry: the skills menu
//! (iteration 5) and the mined-lessons store (roadmap item 20.4).

use std::collections::BTreeSet;
use std::path::Path;

use loom_kernel::context::{LessonIndexEntry, SkillIndexEntry};
use loom_kernel::lessons::{self, MinedLesson};
use loom_tools::skills::split_frontmatter;

use super::consts::{
    LESSON_STATEMENT_MAX_CHARS, LESSONS_FILE, LESSONS_MAX_ENTRIES, SKILL_DESCRIPTION_MAX_BYTES,
    SKILL_DESCRIPTION_READ_BYTES, SKILLS_DIR, SKILLS_MAX_FILES,
};
use super::scan::truncate_utf8_head;

/// Indexes `.agents/skills/*.md` (iteration 5): name = file stem,
/// description = first non-empty line stripped of heading syntax, bounded.
///
/// Infallible like the rest of the scan: a missing or unreadable directory
/// degrades to an empty index, which renders nothing. Two file shapes
/// count: flat `<name>.md` and `<name>/SKILL.md` (Agent Skills
/// convention) — the flat file wins a same-name collision because it is
/// what `load_skill` serves first.
pub(crate) fn scan_skills(root: &Path) -> Vec<SkillIndexEntry> {
    let dir = SKILLS_DIR
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut flat: Vec<SkillIndexEntry> = Vec::new();
    let mut nested: Vec<SkillIndexEntry> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // A stem that cannot be typed as a `load_skill` argument adds
            // noise, not capability.
            if stem.is_empty() {
                continue;
            }
            if let Some(index_entry) = skill_index_entry(&path, stem) {
                flat.push(index_entry);
            }
        } else if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            if let Some(index_entry) = skill_index_entry(&skill_md, name) {
                nested.push(index_entry);
            }
        }
    }
    let flat_names: BTreeSet<String> = flat.iter().map(|e| e.name.clone()).collect();
    let mut skills = flat;
    skills.extend(nested.into_iter().filter(|e| !flat_names.contains(&e.name)));
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.truncate(SKILLS_MAX_FILES);
    skills
}

/// Reads `.loom/lessons.json` (roadmap item 20.4): the evidence-gated
/// lessons `loom lessons` mined from stored transcripts.
///
/// Infallible like the rest of the scan: a missing file, unreadable
/// JSON, or an entry that no longer clears the miner's thresholds
/// degrades to fewer lessons, never to a failed task setup. Entries are
/// re-validated on read (known failure kind, bounded non-empty statement,
/// `is_live()`), name-ordered, deduped by (tool, failure), and capped.
pub(crate) fn scan_lessons(root: &Path) -> Vec<LessonIndexEntry> {
    let path = LESSONS_FILE
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(mined) = serde_json::from_str::<Vec<MinedLesson>>(&body) else {
        return Vec::new();
    };
    let mut valid: Vec<MinedLesson> = mined
        .into_iter()
        .filter(|lesson| {
            !lesson.tool.is_empty()
                && lessons::is_known_failure(&lesson.failure)
                && !lesson.statement.trim().is_empty()
                && lesson.statement.chars().count() <= LESSON_STATEMENT_MAX_CHARS
                && lesson.is_live()
        })
        .collect();
    valid.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.failure.cmp(&b.failure)));
    valid.dedup_by(|a, b| a.tool == b.tool && a.failure == b.failure);
    valid.truncate(LESSONS_MAX_ENTRIES);
    valid
        .into_iter()
        .map(|lesson| {
            let evidence = lesson.evidence();
            LessonIndexEntry::new(lesson.statement, evidence)
        })
        .collect()
}

/// Builds one menu entry from a skill file's bounded head: frontmatter
/// `description` when present, else the first non-empty body line
/// stripped of heading markers. A frontmatter `name` that disagrees
/// with the file stem keeps the skill off the menu — mislabeled
/// metadata must not become a silent alias for a different name.
fn skill_index_entry(path: &Path, stem: &str) -> Option<SkillIndexEntry> {
    let Ok(bytes) = std::fs::read(path) else {
        // Listed but unreadable: an empty description keeps the menu
        // honest about the shape without failing the scan.
        return Some(SkillIndexEntry::new(stem.to_owned(), String::new()));
    };
    let head = truncate_utf8_head(&bytes, SKILL_DESCRIPTION_READ_BYTES);
    let (frontmatter, body) = split_frontmatter(&head);
    if let Some(declared) = &frontmatter.name
        && declared != stem
    {
        return None;
    }
    let description = match &frontmatter.description {
        Some(desc) => desc.clone(),
        None => body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_owned(),
    };
    Some(SkillIndexEntry::new(
        stem.to_owned(),
        truncate_utf8_head(description.as_bytes(), SKILL_DESCRIPTION_MAX_BYTES),
    ))
}
