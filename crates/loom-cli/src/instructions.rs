//! The user's global `AGENTS.md` (roadmap: global instructions support).
//!
//! Project-level instruction files (`AGENTS.md` / `CLAUDE.md` in the
//! workspace root) already ride the repository facts scan. This module
//! covers the user-level file — standing instructions that apply to every
//! run on the machine, regardless of workspace.
//!
//! Loading is a frontend responsibility (ADR-008): the CLI reads the file
//! once at process start and appends it to the system prompt, so the
//! composed prefix is byte-stable for the whole session. The engine never
//! reads the environment or this file, and the eval harness builds its own
//! `EngineConfig` — a developer's global file cannot leak into the frozen
//! suite.

use std::path::Path;

/// Byte bound for the global instructions body, matching the repository
/// facts renderer's cap on project-level instructions.
const GLOBAL_INSTRUCTIONS_MAX_BYTES: usize = 10_000;

/// Loads the global instructions file, when it exists and reads cleanly.
///
/// `None` is the normal no-op: most machines will not have the file, and
/// a missing file must not warn. A file that exists but cannot be read
/// warns on stderr and is skipped — a broken global file should not take
/// the run down. Content is capped at
/// [`GLOBAL_INSTRUCTIONS_MAX_BYTES`] bytes with a truncation marker, so a
/// runaway file cannot blow the stable prompt prefix.
pub(crate) fn load_global(path: &Path) -> Option<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!(
                "warn: ignoring global instructions {}: {error}",
                path.display()
            );
            return None;
        }
    };
    if bytes.len() <= GLOBAL_INSTRUCTIONS_MAX_BYTES {
        return String::from_utf8(bytes)
            .map_err(|error| {
                eprintln!(
                    "warn: ignoring global instructions {} (not valid UTF-8): {error}",
                    path.display()
                );
            })
            .ok();
    }
    // Truncate at the byte bound, backed off to a char boundary so the
    // result stays valid UTF-8.
    let mut boundary = GLOBAL_INSTRUCTIONS_MAX_BYTES.min(bytes.len());
    while boundary > 0 && (bytes[boundary] & 0xC0) == 0x80 {
        boundary -= 1;
    }
    let mut body = String::from_utf8_lossy(&bytes[..boundary]).into_owned();
    body.push_str(&format!(
        "\n\n[global instructions truncated: first {} of {} bytes]\n",
        boundary,
        bytes.len()
    ));
    Some(body)
}

/// Appends the global instructions to the base system prompt.
///
/// The section header names the resolved file path so traces and stored
/// sessions are self-describing about where the extra instruction came
/// from. `None` returns the base unchanged.
pub(crate) fn merge(base: String, global: Option<(String, std::path::PathBuf)>) -> String {
    let Some((body, path)) = global else {
        return base;
    };
    format!(
        "{base}\n\n## Global instructions ({})\n\n{body}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_a_clean_no_op() {
        assert!(load_global(Path::new("/nonexistent/definitely-missing/AGENTS.md")).is_none());
    }

    #[test]
    fn an_unreadable_directory_warns_and_skips() {
        // A directory "reads" as an error (EISDIR), exercising the
        // warn-and-skip branch without env manipulation.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_global(dir.path()).is_none());
    }

    #[test]
    fn a_small_file_loads_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "prefer tabs\n").expect("write");
        assert_eq!(load_global(&path).as_deref(), Some("prefer tabs\n"));
    }

    #[test]
    fn an_oversized_file_truncates_at_the_bound_with_a_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        let body = "x".repeat(GLOBAL_INSTRUCTIONS_MAX_BYTES + 500);
        std::fs::write(&path, &body).expect("write");
        let loaded = load_global(&path).expect("loaded");
        assert!(loaded.contains("[global instructions truncated: first 10000 of 10500 bytes]"));
        assert!(loaded.starts_with(&"x".repeat(9999)));
    }

    #[test]
    fn multibyte_content_truncates_on_a_char_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        // 3-byte chars: 4000 of them = 12000 bytes; the bound cuts
        // mid-character at 10000 (3333 chars + 1 byte), which must back
        // off to the 9999-byte boundary.
        let body = "€".repeat(4_000);
        std::fs::write(&path, &body).expect("write");
        let loaded = load_global(&path).expect("loaded");
        assert!(loaded.contains("[global instructions truncated: first 9999 of 12000 bytes]"));
    }

    #[test]
    fn merge_without_global_returns_the_base() {
        assert_eq!(merge("built-in".to_owned(), None), "built-in".to_owned());
    }

    #[test]
    fn merge_appends_a_path_named_section() {
        let merged = merge(
            "built-in".to_owned(),
            Some((
                "stand tall\n".to_owned(),
                std::path::PathBuf::from("/home/dev/.config/loom/AGENTS.md"),
            )),
        );
        assert!(merged.starts_with("built-in\n\n## Global instructions ("));
        assert!(merged.contains("/home/dev/.config/loom/AGENTS.md"));
        assert!(merged.ends_with(")\n\nstand tall\n"));
    }
}
