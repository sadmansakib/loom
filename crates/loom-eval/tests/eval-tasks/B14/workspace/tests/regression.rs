use loom_eval_b14::{Effect, FileState, ToolCache};
use std::collections::HashMap;

fn state(size: u64, mtime: u64, hash: &str) -> FileState {
    FileState {
        size,
        mtime,
        content_hash: hash.to_owned(),
    }
}

#[test]
fn a_content_change_is_a_miss() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a-1"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    files.insert(
        "src/a.rs".to_owned(),
        state(9, 100, "hash-a-2"),
    );
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn a_touch_without_an_edit_is_a_miss() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    files.insert(
        "src/a.rs".to_owned(),
        state(9, 200, "hash-a"),
    );
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn an_absent_file_that_appears_is_a_miss() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    cache.store("read_file", "src/new.rs", &["src/new.rs"], "no such file", &|p| files.get(p).cloned());
    files.insert(
        "src/new.rs".to_owned(),
        state(4, 300, "hash-new"),
    );
    assert_eq!(cache.lookup("read_file", "src/new.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn a_read_file_that_disappears_is_a_miss() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    files.remove("src/a.rs");
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn an_unchanged_repeat_is_a_hit() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    assert_eq!(
        cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()),
        Some("fn a() {}".to_owned())
    );
}

#[test]
fn different_args_do_not_hit() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    assert_eq!(cache.lookup("read_file", "src/b.rs", &|p| files.get(p).cloned()), None);
    assert_eq!(cache.lookup("list_dir", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn an_overlapping_write_invalidates() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    cache.invalidate(&[Effect::FileWritten("src/a.rs".to_owned())]);
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn a_directory_write_invalidates_beneath_it() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    cache.invalidate(&[Effect::FileWritten("src".to_owned())]);
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn run_command_clears_everything() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    files.insert("src/lib.rs".to_owned(), state(11, 100, "hash-lib"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    cache.store("read_file", "src/lib.rs", &["src/lib.rs"], "fn lib() {}", &|p| files.get(p).cloned());
    cache.invalidate(&[Effect::CommandRun]);
    assert_eq!(cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()), None);
    assert_eq!(cache.lookup("read_file", "src/lib.rs", &|p| files.get(p).cloned()), None);
}

#[test]
fn a_disjoint_write_keeps_the_entry() {
    let mut cache = ToolCache::new();
    let mut files = HashMap::new();
    files.insert("src/a.rs".to_owned(), state(9, 100, "hash-a"));
    cache.store("read_file", "src/a.rs", &["src/a.rs"], "fn a() {}", &|p| files.get(p).cloned());
    cache.invalidate(&[Effect::FileWritten("docs/b.md".to_owned())]);
    assert_eq!(
        cache.lookup("read_file", "src/a.rs", &|p| files.get(p).cloned()),
        Some("fn a() {}".to_owned())
    );
}
