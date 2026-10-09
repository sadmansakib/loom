//! Command-aware output filters (roadmap item 3).
//!
//! RTK-style reducers for *successful* `run_command` output whose shape
//! is well known and unit-addressable — `git status`/`diff`/`log`,
//! `ls`/`tree`, `grep`, and `cargo test` pass lists. A byte-window cut can
//! land mid-line and hide the very count the model needs ("how many?"),
//! so these reducers cut at the command's own unit boundaries (a commit,
//! a matched line, a passing test) and name what was dropped
//! (ADR-014's explicit-omission rule).
//!
//! Nothing is lost: a reduction applies only when the call has an
//! artifact store attached — the caller tees the unreduced stdout into
//! it and points the model at the refetch, exactly like the engine's
//! oversized-output spill (ADR-012 decision 3). Failed commands are
//! never reduced; failure evidence owns that path (2F, ADR-017).
//!
//! Scope is deliberately the bare commands a model actually issues
//! through this tool; `bash -c "git log"` wrapping is not parsed — the
//! model can always run the bare command, and a wrong guess would
//! rewrite output the model asked to see verbatim.

use std::fmt::Write as _;

/// A reduction is only worth an artifact when it saves at least this
/// much; below it the inline form is already smaller than the refetch
/// note that would accompany it.
pub(crate) const MIN_SAVED_BYTES: usize = 256;

/// Most commits kept from `git log`.
const LOG_COMMITS: usize = 25;
/// Most diff lines kept from `git diff`.
const DIFF_LINES: usize = 200;
/// Most lines kept from `git status` (branch header plus entries).
const STATUS_LINES: usize = 40;
/// Most entries kept from `ls` / `tree`.
const LS_LINES: usize = 100;
/// Most matched lines kept from `grep`.
const GREP_LINES: usize = 60;

/// One applied reduction: the reduced text and the bytes it dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reduction {
    /// The stdout text the model sees instead of the raw stream.
    pub text: String,
    /// `raw.len() - text.len()`; always ≥ [`MIN_SAVED_BYTES`].
    pub saved: usize,
}

/// Picks and applies the reducer for one successful command.
///
/// `None` means no reducer applies — or the saving would not clear
/// [`MIN_SAVED_BYTES`] — and the caller must keep the raw stream.
pub(crate) fn reduce(program: &str, args: &[String], stdout: &str) -> Option<Reduction> {
    let reduced = match program {
        "git" => match args.first().map(String::as_str) {
            Some("log") => git_log(stdout),
            Some("diff") => cap_lines(stdout, DIFF_LINES, "diff lines"),
            Some("status") => cap_lines(stdout, STATUS_LINES, "status lines"),
            _ => return None,
        },
        "ls" | "tree" => cap_lines(stdout, LS_LINES, "entries"),
        "grep" => cap_lines(stdout, GREP_LINES, "matches"),
        "cargo" if args.first().map(String::as_str) == Some("test") => cargo_test(stdout),
        _ => return None,
    }?;
    let saved = stdout.len().saturating_sub(reduced.len());
    (saved >= MIN_SAVED_BYTES).then_some(Reduction {
        text: reduced,
        saved,
    })
}

/// Keeps the first `cap` lines and names the rest — the shared shape of
/// the line-capped reducers. `unit` names what a line is here ("entries",
/// "matches", ...).
fn cap_lines(stdout: &str, cap: usize, unit: &str) -> Option<String> {
    let total = stdout.lines().count();
    if total <= cap {
        return None;
    }
    let mut text = String::with_capacity(stdout.len());
    for line in stdout.lines().take(cap) {
        text.push_str(line);
        text.push('\n');
    }
    let _ = writeln!(text, "[... {} more {unit} omitted]", total - cap);
    Some(text)
}

/// `git log`: whole commit blocks are the unit. Default-format blocks
/// start at `commit <sha>` lines (graph prefixes tolerated); one-line
/// format has no such marker and is capped by line instead.
fn git_log(stdout: &str) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            let bare = line.trim_start_matches(['*', '|', '/', '\\', ' ']);
            bare.starts_with("commit ") || bare.starts_with("commit\t")
        })
        .map(|(index, _)| index)
        .collect();
    if starts.is_empty() {
        // One-line format (or a format with no `commit` markers): a line
        // is a commit.
        return cap_lines(stdout, LOG_COMMITS, "commits");
    }
    if starts.len() <= LOG_COMMITS {
        return None;
    }
    let cut = starts[LOG_COMMITS];
    let mut text = String::with_capacity(stdout.len());
    for line in &lines[..cut] {
        text.push_str(line);
        text.push('\n');
    }
    let _ = writeln!(
        text,
        "[... {} more commits omitted]",
        starts.len() - LOG_COMMITS
    );
    Some(text)
}

/// `cargo test` on success: the per-test `test <name> ... ok` lines are
/// noise the closing `test result:` line already summarizes, so a run
/// of them collapses to one counted line. Everything else — compile
/// warnings, the `running N tests` headers, ignored/skipped entries,
/// and the result lines — is kept verbatim.
fn cargo_test(stdout: &str) -> Option<String> {
    let mut text = String::with_capacity(stdout.len());
    let mut pending: usize = 0;
    let mut collapsed_total: usize = 0;
    let flush = |text: &mut String, pending: &mut usize, total: &mut usize| {
        if *pending > 0 {
            let _ = writeln!(text, "[... {pending} passing test lines collapsed]");
            *total += *pending;
            *pending = 0;
        }
    };
    for line in stdout.lines() {
        if is_passing_test_line(line) {
            pending += 1;
            continue;
        }
        flush(&mut text, &mut pending, &mut collapsed_total);
        text.push_str(line);
        text.push('\n');
    }
    flush(&mut text, &mut pending, &mut collapsed_total);
    (collapsed_total > 0).then_some(text)
}

/// libtest's per-test pass line: `test <name> ... ok`, including the
/// doc-test form `test src/lib.rs - foo (line 3) ... ok`. Anything else
/// (`FAILED`, `ignored`, bench lines) stays visible.
fn is_passing_test_line(line: &str) -> bool {
    line.starts_with("test ") && line.ends_with(" ... ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_blocks(count: usize) -> String {
        let mut text = String::new();
        for index in 0..count {
            let _ = write!(
                text,
                "commit 00000000000000000000000000000000000000{index:02}\n\
                 Author: A <a@example.com>\n    subject {index}\n\n    body {index}\n"
            );
        }
        text
    }

    #[test]
    fn git_log_keeps_the_first_blocks_and_counts_the_rest() {
        let raw = commit_blocks(30);
        let reduction = reduce("git", &["log".to_owned()], &raw).expect("reduced");
        assert!(reduction.text.contains("subject 24"), "{}", reduction.text);
        assert!(!reduction.text.contains("subject 25"));
        assert!(
            reduction.text.contains("[... 5 more commits omitted]"),
            "{}",
            reduction.text
        );
        assert!(reduction.saved >= MIN_SAVED_BYTES);
    }

    #[test]
    fn git_log_under_the_cap_is_untouched() {
        let raw = commit_blocks(3);
        assert!(reduce("git", &["log".to_owned()], &raw).is_none());
    }

    #[test]
    fn git_log_one_line_format_caps_by_line() {
        let raw: String = (0..40)
            .map(|index| format!("{index:040x} subject {index}\n"))
            .collect();
        let reduction = reduce("git", &["log".to_owned()], &raw).expect("reduced");
        assert!(reduction.text.contains("subject 24"));
        assert!(reduction.text.contains("[... 15 more commits omitted]"));
    }

    #[test]
    fn git_diff_caps_at_line_boundaries() {
        let raw: String = (0..250).map(|_| "a line of diff\n").collect();
        let reduction = reduce("git", &["diff".to_owned()], &raw).expect("reduced");
        assert_eq!(reduction.text.lines().count(), 201); // 200 + marker
        assert!(reduction.text.contains("[... 50 more diff lines omitted]"));
    }

    #[test]
    fn grep_keeps_a_head_of_matches() {
        let raw: String = (0..100)
            .map(|i| format!("src/a.rs:{i}: found it\n"))
            .collect();
        let reduction = reduce("grep", &[], &raw).expect("reduced");
        assert!(reduction.text.contains("src/a.rs:59"));
        assert!(!reduction.text.contains("src/a.rs:60:"));
        assert!(reduction.text.contains("[... 40 more matches omitted]"));
    }

    #[test]
    fn ls_keeps_a_head_of_entries() {
        let raw: String = (0..150).map(|i| format!("entry-{i}\n")).collect();
        let reduction = reduce("ls", &[], &raw).expect("reduced");
        assert!(reduction.text.contains("[... 50 more entries omitted]"));
    }

    #[test]
    fn cargo_test_collapses_passing_lines_and_keeps_the_rest() {
        let mut raw = String::from("warning: unused variable\nrunning 27 tests\n");
        for index in 0..25 {
            let _ = writeln!(raw, "test t{index} ... ok");
        }
        raw.push_str("test c ... ignored, reason: slow\ntest d::e ... ok\n");
        raw.push_str("running 1 test\ntest src/lib.rs - f (line 3) ... ok\n\n");
        raw.push_str("test result: ok. 26 passed; 0 failed; 1 ignored\n");

        let reduction = reduce("cargo", &["test".to_owned()], &raw).expect("reduced");
        assert!(
            reduction
                .text
                .contains("[... 25 passing test lines collapsed]"),
            "{}",
            reduction.text
        );
        assert_eq!(
            reduction
                .text
                .matches("[... 1 passing test lines collapsed]")
                .count(),
            2,
            "one marker per run section: {}",
            reduction.text
        );
        // Kept verbatim: the warning, the ignored entry, the headers.
        assert!(reduction.text.contains("warning: unused variable"));
        assert!(reduction.text.contains("test c ... ignored, reason: slow"));
        assert!(reduction.text.contains("running 1 test"));
        assert!(reduction.text.contains("test result: ok. 26 passed;"));
        assert!(!reduction.text.contains("test t0 ... ok"));
    }

    #[test]
    fn a_failing_test_line_is_never_collapsed() {
        let raw = "test a ... FAILED\ntest b ... ok\n";
        // The FAILED line must stay; the single `ok` line saving does not
        // clear MIN_SAVED_BYTES, so nothing is reduced at all.
        assert!(reduce("cargo", &["test".to_owned()], raw).is_none());
    }

    #[test]
    fn small_savings_are_not_worth_an_artifact() {
        // 10 grep matches is well under the byte floor.
        let raw: String = (0..10).map(|i| format!("m{i}\n")).collect();
        assert!(reduce("grep", &[], &raw).is_none());
    }

    #[test]
    fn commands_without_a_reducer_stay_raw() {
        assert!(reduce("cargo", &["build".to_owned()], "many\nlines\n").is_none());
        assert!(reduce("git", &["stash".to_owned()], "many\nlines\n").is_none());
        assert!(reduce("find", &[], "many\nlines\n").is_none());
        assert!(reduce("echo", &[], "anything at all").is_none());
    }
}
