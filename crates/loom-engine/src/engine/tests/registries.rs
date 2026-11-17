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

/// Roadmap item 40: the read-only profile is enforcement, not a label.
/// It keeps exactly the read-only tools and drops every mutating one.
#[test]
fn read_only_registry_drops_every_mutating_tool() {
    let built = build_registry(
        &CommandPolicy::default(),
        ToolBudgets::default(),
        false,
        None,
        None,
    )
    .expect("built-in registry");
    let registry = read_only_registry(built);
    let names: Vec<&str> = registry.specs().map(|spec| spec.name.as_str()).collect();
    assert_eq!(
        names,
        ["read_file", "list_dir", "search", "artifact_read"],
        "the read-only profile exposes only read-only built-ins"
    );
    for name in ["write_file", "edit_file", "run_command", "verify", "todo"] {
        assert!(
            !names.contains(&name),
            "the read-only profile must drop {name}: {names:?}"
        );
    }
}
