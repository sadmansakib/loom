//! The ADR-021 boundary guard: `loom-cli` may depend on `loom-core` and
//! nothing else from this workspace.
//!
//! The facade rule (architecture.md, §9) keeps domain logic out of the
//! binary: if a change needs `loom-kernel`, `loom-engine`, or any other
//! workspace crate directly, the missing piece belongs behind `loom-core`
//! instead. This test fails the suite the moment such a dependency
//! appears, so the rule is enforced by `cargo test --workspace` (and
//! therefore CI) rather than by review vigilance.
//!
//! The check scans every dependency-declaration line in the manifest —
//! `[dependencies]`, `[dev-dependencies]`, and any target-specific
//! section — so a violation cannot hide in an uncommon section.

use std::fs;

#[test]
fn the_binary_depends_on_the_facade_alone() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("reading loom-cli's own manifest");

    // Every workspace-crate dependency line (`loom-… = …`) must be the
    // facade. Comments are skipped; dependency keys never start with `#`.
    let offenders: Vec<&str> = manifest
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("loom-"))
        .filter(|line| !line.starts_with("loom-core"))
        .collect();

    assert!(
        offenders.is_empty(),
        "loom-cli must reach workspace code through loom-core alone (ADR-021); \
         found direct dependencies: {offenders:?}. Route the missing piece \
         through the facade instead."
    );

    // The guard must not pass vacuously: the facade itself has to be wired.
    assert!(
        manifest.contains("loom-core"),
        "loom-cli no longer depends on loom-core; the guard needs the \
         facade present to mean anything"
    );
}
