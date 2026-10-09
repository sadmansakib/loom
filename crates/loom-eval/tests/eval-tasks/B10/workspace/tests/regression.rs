use loom_eval_b10::*;

fn file(lines: usize) -> String {
    (0..lines).map(|i| format!("line-{i:05}")).collect::<Vec<_>>().join("\n")
}

#[test]
fn default_read_returns_small_files_whole() {
    let content = file(400);
    let window = read_window(&content, 0, None);
    assert_eq!(window.total_lines, 400);
    assert_eq!(window.text, content, "an ordinary file must come back in one call");
    assert!(!window.truncated);
}

#[test]
fn oversized_default_read_fills_the_budget_not_the_line_window() {
    let content = file(20_000);
    assert!(content.len() > READ_BUDGET_BYTES);
    let window = read_window(&content, 0, None);
    assert!(window.truncated);
    assert!(
        window.text.len() >= READ_BUDGET_BYTES * 9 / 10,
        "a default read past the budget must spend the budget, not a fraction of it: {} bytes",
        window.text.len()
    );
    assert!(window.text.len() <= READ_BUDGET_BYTES);
}

#[test]
fn explicit_limit_is_honoured() {
    let content = file(400);
    let window = read_window(&content, 0, Some(10));
    assert_eq!(window.text.lines().count(), 10);
    assert_eq!(window.text, content.lines().take(10).collect::<Vec<_>>().join("\n"));
    assert!(window.truncated);
}

#[test]
fn offset_is_honoured() {
    let content = file(400);
    let window = read_window(&content, 395, None);
    assert_eq!(window.offset, 395);
    assert_eq!(
        window.text,
        content.lines().skip(395).collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn empty_file_returns_empty_whole() {
    let window = read_window("", 0, None);
    assert_eq!(window.text, "");
    assert_eq!(window.total_lines, 0);
    assert!(!window.truncated);
}
