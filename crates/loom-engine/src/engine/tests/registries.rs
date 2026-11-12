//! The full vs. bare registry contract (roadmap item 8 phase B).

use super::*;

/// Roadmap item 8 phase B: the kernel-owned `todo` tool is a
/// full-registry lever. The full engine exposes it; the bare-loop
/// arm — the basic-harness baseline — must keep excluding it
/// (bare = read/list/write/edit/shell).
#[test]
fn todo_tool_is_exposed_in_the_full_registry_and_excluded_from_bare() {
    let built = build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
    )
    .expect("built-in registry");
    let full: Vec<&str> = built.specs().map(|spec| spec.name.as_str()).collect();
    assert!(
        full.contains(&"todo"),
        "the full registry must expose the kernel todo tool: {full:?}"
    );

    let bare_registry = bare_registry(built);
    let bare: Vec<&str> = bare_registry
        .specs()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(!bare.contains(&"todo"), "the bare arm must not: {bare:?}");
    assert_eq!(
        bare,
        [
            "read_file",
            "list_dir",
            "write_file",
            "edit_file",
            "run_command"
        ],
        "the bare arm is exactly the five primitives, in order"
    );
}
