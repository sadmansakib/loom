//! Per-tool output budgets and explicit omission markers (ADR-014).
//!
//! The engine still applies the global 64 KiB bound as a backstop, but each
//! tool shapes its own output first: a tool knows what its result *means* and
//! where the information the model needs lives. Bounds are per tool so a
//! command's error tail is not thrown away to make room for a file head.
//!
//! Every cut is explicit. Silent truncation makes a model reason over a
//! partial result as if it were whole (ADR-014); each helper here leaves an
//! `... [N bytes omitted] ...` marker in the text and reports whether it cut.

/// Bytes kept from the head of a `read_file` result (ADR-014, V7).
///
/// Sized so an ordinary source file (plus its line-number prefixes) comes
/// back whole in a single call: the repository's largest source file is
/// ~47 KiB, so 56 KiB covers it while staying under the engine's 64 KiB
/// backstop. A smaller cap forces paging, and each page costs a model turn.
pub(crate) const READ_FILE_HEAD_BYTES: usize = 56 * 1024;

/// Bytes kept from the head of a `run_command` result (banner, early output).
pub(crate) const COMMAND_HEAD_BYTES: usize = 16 * 1024;

/// Bytes kept from the tail of a `run_command` result (errors live here).
pub(crate) const COMMAND_TAIL_BYTES: usize = 16 * 1024;

/// Bytes kept from an `edit_file` echo of the edited regions. Each edit
/// shows ±3 numbered lines of its final state, so the model never
/// re-reads to confirm what it wrote; a pathological batch (many edits,
/// long lines) is bounded by this cap instead of flooding the turn.
pub(crate) const EDIT_ECHO_MAX_BYTES: usize = 8 * 1024;

/// Bytes a `symbols_overview` rendering may occupy before falling back to
/// per-kind counts. The overview exists to point at symbols, not to
/// enumerate a whole crate: past this budget the model gets the shape of
/// the file (`functions: 120`) and raises depth on a narrower path instead.
pub(crate) const SYMBOLS_OVERVIEW_MAX_BYTES: usize = 8 * 1024;

/// Bytes kept from the head of a `load_skill` body (iteration 5). Skills
/// are hand-written playbooks, not data: 16 KiB covers a thorough one and
/// matches the command head budget, while a runaway file is cut with the
/// standard omission marker instead of flooding the context.
pub(crate) const SKILL_BODY_HEAD_BYTES: usize = 16 * 1024;

/// The explicit-omission marker appended where output was dropped.
pub(crate) fn omitted(count: u64) -> String {
    format!("... [{count} bytes omitted]")
}

/// Keeps the first `cap` bytes of `text` (never splitting a character) and
/// appends an explicit marker naming what was dropped. Returns the text and
/// whether it was cut.
pub(crate) fn head_only(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_owned(), false);
    }
    let end = floor_char_boundary(text, cap);
    let mut shaped = String::with_capacity(end + 48);
    shaped.push_str(&text[..end]);
    shaped.push('\n');
    shaped.push_str(&omitted((text.len() - end) as u64));
    (shaped, true)
}

/// Keeps the first `head_cap` and the last `tail_cap` bytes of `text` (never
/// splitting a character) around an explicit omission marker. Returns the text
/// and whether it was cut.
pub(crate) fn head_tail(text: &str, head_cap: usize, tail_cap: usize) -> (String, bool) {
    if text.len() <= head_cap + tail_cap {
        return (text.to_owned(), false);
    }
    let head_end = floor_char_boundary(text, head_cap);
    let tail_start = ceil_char_boundary(text, text.len() - tail_cap);
    let dropped = tail_start.saturating_sub(head_end) as u64;
    let mut shaped = String::with_capacity(head_end + tail_cap + 48);
    shaped.push_str(&text[..head_end]);
    shaped.push('\n');
    shaped.push_str(&omitted(dropped));
    shaped.push_str(" ...\n");
    shaped.push_str(&text[tail_start..]);
    (shaped, true)
}

/// The largest index `<= index` that is a UTF-8 char boundary.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The smallest index `>= index` that is a UTF-8 char boundary.
fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_untouched() {
        let (shaped, cut) = head_tail("hello", 8, 8);
        assert_eq!(shaped, "hello");
        assert!(!cut);
    }

    #[test]
    fn head_tail_keeps_both_ends_around_the_marker() {
        let text = format!("{}MIDDLE{}", "h".repeat(8), "t".repeat(8));
        let (shaped, cut) = head_tail(&text, 8, 8);
        assert!(cut);
        assert_eq!(shaped, format!("hhhhhhhh\n{} ...\ntttttttt", omitted(6)));
    }

    #[test]
    fn head_only_keeps_the_head_and_marks_the_rest() {
        let (shaped, cut) = head_only("abcdefghij", 4);
        assert!(cut);
        assert_eq!(shaped, format!("abcd\n{}", omitted(6)));
        let (shaped, cut) = head_only("hello", 32);
        assert_eq!(shaped, "hello");
        assert!(!cut);
    }

    #[test]
    fn cuts_never_split_a_multibyte_character() {
        // Each character is 3 bytes; a cap of 4 lands mid-character.
        let text = "✀✀✀".to_owned();
        let (shaped, cut) = head_tail(&text, 4, 4);
        assert!(cut);
        assert!(shaped.starts_with('✀'));
        assert!(shaped.ends_with('✀'));
        let (shaped, cut) = head_only(&text, 4);
        assert!(cut);
        assert_eq!(shaped, format!("✀\n{}", omitted(6)));
    }
}
