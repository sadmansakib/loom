//! Syntax guard (roadmap item 5 phase B): parse-before-write refusal.
//!
//! `write_file` and `edit_file` run every candidate buffer through
//! [`guard`] *before* any filesystem mutation, so a model cannot leave
//! a source file unparsable on disk. Rust only for now
//! (`tree-sitter-rust`); every other language — and every file whose
//! extension is not `rs` — is skipped silently: an unknown grammar must
//! never block a write the guard cannot reason about. The whole guard
//! is disabled by setting `LOOM_SYNTAX_GUARD=0`
//! ([`syntax_guard_enabled`]); it defaults to on.

use std::path::Path;

use tree_sitter::{Node, Parser, Point};

/// Environment variable that disables the guard when set to exactly
/// `0` (any other value, including garbage, keeps it on).
const ENV_DISABLE: &str = "LOOM_SYNTAX_GUARD";

/// Test-only override of [`syntax_guard_enabled`] (see [`testing`]).
#[cfg(test)]
static TEST_GUARD_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One refusal: where the first syntax error starts (1-based line and
/// column) and what that line says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("syntax error at line {line}, column {column}: {excerpt}")]
pub struct SyntaxGuardError {
    /// 1-based line of the first error node.
    pub line: usize,
    /// 1-based column of the first error node.
    pub column: usize,
    /// Short excerpt of the offending line.
    pub excerpt: String,
}

/// Maximum characters of the offending line kept in the error excerpt.
const EXCERPT_MAX_CHARS: usize = 60;

/// Whether the syntax guard is active: on unless `LOOM_SYNTAX_GUARD`
/// is set to exactly `0`. Read per call so an operator can flip it
/// without restarting the session.
pub fn syntax_guard_enabled() -> bool {
    #[cfg(test)]
    {
        // Tests cannot touch the real environment (edition 2024 makes
        // `env::set_var` `unsafe`, and the workspace forbids `unsafe`),
        // so they flip this override instead — see [`testing`].
        if TEST_GUARD_DISABLED.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
    }
    enabled_from_env(std::env::var(ENV_DISABLE).ok().as_deref())
}

/// The env knob's decision table, split out so tests can cover it
/// without mutating process state: unset means on, `0` means off,
/// anything else means on.
fn enabled_from_env(value: Option<&str>) -> bool {
    value.is_none_or(|value| value != "0")
}

/// Parses one candidate buffer and refuses it when the tree contains a
/// syntax error.
///
/// `path` selects the language by extension; only `rs` is guarded, and
/// every other file — unknown language, no extension, anything —
/// passes untouched. A clean parse passes. A parse error returns the
/// first error node's 1-based line/column (tree-sitter's
/// `start_position`) and a short excerpt of the offending line, so the
/// model can fix the buffer it was about to write instead of guessing.
/// A missing tree (no language configured on the parser) also passes:
/// the guard fails open exactly like it does for unknown languages.
pub fn guard(path: &Path, candidate: &str) -> Result<(), SyntaxGuardError> {
    if !syntax_guard_enabled() {
        return Ok(());
    }
    if path
        .extension()
        .map(|ext| ext == std::ffi::OsStr::new("rs"))
        != Some(true)
    {
        return Ok(());
    }
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return Ok(());
    }
    let Some(tree) = parser.parse(candidate, None) else {
        return Ok(());
    };
    match first_error(tree.root_node()) {
        None => Ok(()),
        Some(node) => Err(SyntaxGuardError::at(node.start_position(), candidate)),
    }
}

/// Guards an edit: refuses only when the candidate introduces a parse
/// error the text it replaces did not already have.
///
/// The guard's job is to stop a model from making a file *more* broken,
/// never to demand that a file the grammar cannot read suddenly become
/// parsable. Some files legitimately fail this grammar — 2015-era Rust
/// full of `try!(…)` calls is the measured case, where tree-sitter reads
/// `try` as the keyword for a 2018 try-block and errors on the `(` — and
/// for those, refusing every write would lock the model out of the file
/// with a message pointing at a line it never touched. So when
/// `previous` already fails to parse, the guard fails open, exactly as
/// it does for a language it cannot reason about at all.
pub fn guard_against(previous: &str, candidate: &str, path: &Path) -> Result<(), SyntaxGuardError> {
    if !syntax_guard_enabled() || guard(path, previous).is_err() {
        return Ok(());
    }
    guard(path, candidate)
}

/// Preorder walk for the first ERROR/MISSING node in document order.
/// `has_error` on the root already told us one exists; this finds
/// where the first one starts so the refusal can point at it.
fn first_error(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    let mut descending = true;
    loop {
        let node = cursor.node();
        if node.is_error()
            || node.kind() == "ERROR"
            || node.is_missing()
            || node.kind() == "MISSING"
        {
            return Some(node);
        }
        if descending && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            descending = true;
            continue;
        }
        if !cursor.goto_parent() {
            return None;
        }
        descending = false;
    }
}

impl SyntaxGuardError {
    /// Builds the refusal for an error node at `point` (0-based,
    /// byte-columned — tree-sitter's `start_position`), carrying the
    /// offending source line as the excerpt.
    fn at(point: Point, source: &str) -> Self {
        let excerpt = source
            .lines()
            .nth(point.row)
            .map(excerpt_of)
            .unwrap_or_default();
        Self {
            line: point.row + 1,
            column: point.column + 1,
            excerpt,
        }
    }
}

/// The offending line, trimmed and elided to [`EXCERPT_MAX_CHARS`] so
/// long generated lines cannot bloat the refusal message.
fn excerpt_of(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() > EXCERPT_MAX_CHARS {
        let mut short: String = trimmed.chars().take(EXCERPT_MAX_CHARS).collect();
        short.push('…');
        short
    } else {
        trimmed.to_owned()
    }
}

/// Test seam for [`syntax_guard_enabled`].
///
/// Edition 2024 makes `std::env::set_var` `unsafe` and the workspace
/// forbids `unsafe`, so tests disable the guard through the
/// [`TEST_GUARD_DISABLED`] override instead of the real environment.
/// The override is process-global and tests run in parallel, so every
/// test that flips it — and every test that depends on the guard being
/// on while another might flip it — holds [`env_lock`] for the whole
/// tool call.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Mutex, MutexGuard};

    /// Serializes guard-enablement across the test binary.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the guard-enablement lock; hold it across any awaited
    /// tool call whose guard behavior the test asserts.
    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// RAII guard (acquires [`env_lock`], then disables the syntax
    /// guard until dropped) for tests that intentionally write
    /// unparsable Rust — e.g. asserting fused `cargo check`
    /// diagnostics, or fixtures whose `.rs` files hold plain words.
    pub(crate) fn disable_guard() -> TestGuard {
        let lock = env_lock();
        super::TEST_GUARD_DISABLED.store(true, std::sync::atomic::Ordering::SeqCst);
        TestGuard { _lock: lock }
    }

    /// Returned by [`disable_guard`]; re-enables the guard on drop so
    /// a failing assertion cannot poison the rest of the binary.
    pub(crate) struct TestGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for TestGuard {
        fn drop(&mut self) {
            super::TEST_GUARD_DISABLED.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_of(source: &str) -> SyntaxGuardError {
        guard(Path::new("f.rs"), source).expect_err("must be refused")
    }

    #[test]
    fn valid_rust_passes() {
        let _lock = testing::env_lock();
        assert!(guard(Path::new("f.rs"), "fn main() { let x = 1; }\n").is_ok());
        assert!(guard(Path::new("f.rs"), "").is_ok());
    }

    #[test]
    fn missing_brace_is_refused_with_line_and_column() {
        let _lock = testing::env_lock();
        let err = err_of("fn broken() {\n");
        assert!(err.line >= 1, "{err:?}");
        assert!(err.column >= 1, "{err:?}");
        assert!(
            err.to_string().contains(&format!("line {}", err.line))
                && err.to_string().contains(&format!("column {}", err.column)),
            "{err}"
        );
    }

    #[test]
    fn missing_paren_points_at_the_first_error_line() {
        let _lock = testing::env_lock();
        // The clean first line proves the reported position is the
        // error's own, not the file start.
        let err = err_of("fn ok() {}\nfn broken( {}\n");
        assert_eq!(err.line, 2, "{err}");
        assert!(err.column >= 1, "{err}");
        assert!(err.excerpt.contains("broken"), "{err}");
    }

    #[test]
    fn non_rust_paths_are_skipped() {
        let _lock = testing::env_lock();
        assert!(guard(Path::new("note.txt"), "fn broken( {").is_ok());
        assert!(guard(Path::new("noext"), "fn broken( {").is_ok());
        // Extension matching is case-sensitive: `.RS` is not Rust.
        assert!(guard(Path::new("F.RS"), "fn broken( {").is_ok());
    }

    #[test]
    fn the_env_knob_only_disables_on_exact_zero() {
        assert!(enabled_from_env(None));
        assert!(!enabled_from_env(Some("0")));
        assert!(enabled_from_env(Some("1")));
        assert!(enabled_from_env(Some("off")));
        assert!(enabled_from_env(Some("00")));
    }

    #[test]
    fn the_test_override_disables_the_guard() {
        let _disabled = testing::disable_guard();
        assert!(guard(Path::new("f.rs"), "fn broken( {").is_ok());
    }

    #[test]
    fn editing_a_file_that_already_fails_to_parse_is_allowed() {
        let _lock = testing::env_lock();
        // 2015-era `try!(…)` defeats this grammar. The guard must not
        // lock the model out of such a file with an error at a line it
        // never touched.
        let legacy = "fn a() {\n    let mmap = try!(self.mmap(&paths));\n}\n";
        assert!(
            guard(Path::new("f.rs"), legacy).is_err(),
            "the grammar really does reject try!()"
        );
        assert!(guard_against(legacy, legacy, Path::new("f.rs")).is_ok());
    }

    #[test]
    fn an_edit_that_breaks_a_parsable_file_is_still_refused() {
        let _lock = testing::env_lock();
        assert!(guard_against("fn ok() {}\n", "fn broken( {}\n", Path::new("f.rs")).is_err());
    }

    #[test]
    fn a_new_file_is_guarded_like_any_other() {
        let _lock = testing::env_lock();
        assert!(guard_against("", "fn broken( {}\n", Path::new("f.rs")).is_err());
        assert!(guard_against("", "fn ok() {}\n", Path::new("f.rs")).is_ok());
    }
}
