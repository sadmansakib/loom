//! The ADR-021 layering rules, enforced by `cargo test --workspace`.
//!
//! `architecture.md` §1 claims dependencies "point strictly downward — the
//! kernel never links a filesystem store or a CLI" and that provider and tool
//! specifics "cannot leak into the kernel by construction".
//!
//! Cargo enforces *part* of that by accident: because every implementation
//! crate already depends on the kernel, wiring the kernel to one of them is a
//! cycle and `cargo build` rejects it. So a cycle cannot reach CI. Three
//! shapes it does **not** catch, which are what this file exists for:
//!
//! - **Dev-dependency cycles.** Cargo permits them deliberately — a
//!   dev-dependency is not part of the published graph. `loom-kernel` gaining
//!   `loom-store` under `[dev-dependencies]` resolves cleanly, and it is still
//!   an inversion: the kernel's own tests would then exercise implementations
//!   that are supposed to sit strictly below it.
//! - **Facade bypasses that are not cycles.** `loom-eval` reaching
//!   `loom-engine` directly is perfectly legal to cargo and invisible to
//!   review; it just quietly routes the harness around the public surface
//!   every other embedder uses, so an arm can pass against a private path.
//! - **Silently dropped policy.** Removing `unsafe_code = "forbid"` from the
//!   workspace lints is a one-line edit that turns a compile error into a
//!   warning nobody reads, and it fails no build at all.
//!
//! This file extends the sibling guard in `loom-cli/tests/facade.rs` (which
//! pins the binary to the facade) up to the full graph:
//!
//! - `loom-kernel` depends on no workspace crate — it defines the
//!   side-effect traits (`ModelProvider`, `SessionStore`, `ArtifactStore`,
//!   `Tool`, `FactsSource`, `ContextCompiler`) that everything else implements.
//! - `loom-providers`, `loom-store`, and `loom-tools` each depend only on
//!   `loom-kernel`, never on the composition root or the facade.
//! - `loom-engine` is the composition root: it may reach the kernel and the
//!   three implementation crates, but not `loom-core` (the facade is above it).
//!
//! Every dependency-declaration line is scanned — `[dependencies]`,
//! `[dev-dependencies]`, and any target-specific section — so a violation
//! cannot hide in an uncommon section. Dev-dependencies are deliberately in
//! scope, and that is the whole reason the kernel guard is not redundant with
//! cargo's cycle detection: a dev-dep cycle is legal to cargo and still an
//! inversion here.

use std::fs;
use std::path::PathBuf;

/// Every `loom-*` crate in the workspace, by directory name.
const WORKSPACE_CRATES: &[&str] = &[
    "loom-core",
    "loom-kernel",
    "loom-providers",
    "loom-store",
    "loom-tools",
    "loom-engine",
    "loom-cli",
    "loom-eval",
];

fn manifest(crate_name: &str) -> String {
    // `CARGO_MANIFEST_DIR` is `crates/loom-kernel`, so one level up is
    // `crates/`, where every sibling crate's manifest lives.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_name)
        .join("Cargo.toml");
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "reading {crate_name}'s manifest at {}: {error}",
            path.display()
        )
    })
}

/// Every workspace-crate dependency edge declared by `crate_name`.
///
/// Returns the crate names, not the raw lines, so the assertion messages can
/// name the offending dependency directly. Comment lines are skipped because a
/// dependency key never begins with `#`.
fn workspace_deps(crate_name: &str) -> Vec<&'static str> {
    manifest(crate_name)
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("loom-"))
        .filter_map(|line| {
            let name = line
                .split_once(['=', ' '])
                .map(|(name, _)| name)
                .unwrap_or(line);
            WORKSPACE_CRATES
                .iter()
                .copied()
                .find(|candidate| name == *candidate)
        })
        .collect()
}

fn assert_no_workspace_deps(crate_name: &str, allowed: &[&str]) {
    let deps = workspace_deps(crate_name);
    let offenders: Vec<&&str> = deps.iter().filter(|dep| !allowed.contains(dep)).collect();

    assert!(
        offenders.is_empty(),
        "{crate_name} must not depend on {:?}; found {:?}. \
         Dependencies point strictly downward (architecture §1). Where cargo \
         would also reject this as a cycle, the failure here is still the \
         better one: it names the rule instead of a four-hop cycle path.",
        allowed,
        offenders
    );
}

/// The guards must not pass vacuously: each crate under test must still
/// declare at least the dependencies that make the rule meaningful.
fn assert_still_wired(crate_name: &str, expected: &[&str]) {
    let deps = workspace_deps(crate_name);
    for want in expected {
        assert!(
            deps.contains(want),
            "{crate_name} no longer depends on {want}; the boundary guard \
             needs the edge present to mean anything"
        );
    }
}

#[test]
fn the_kernel_depends_on_no_workspace_crate() {
    assert_no_workspace_deps("loom-kernel", &[]);
}

#[test]
fn the_implementation_crates_depend_on_the_kernel_alone() {
    for crate_name in ["loom-providers", "loom-store", "loom-tools"] {
        assert_no_workspace_deps(crate_name, &["loom-kernel"]);
        assert_still_wired(crate_name, &["loom-kernel"]);
    }
}

#[test]
fn the_composition_root_does_not_reach_the_facade_above_it() {
    // `loom-engine` sits below `loom-core` in the graph: it may depend on the
    // kernel and the three implementation crates, but the facade re-exports
    // *its* surface, so a dependency here would be a cycle.
    assert_no_workspace_deps(
        "loom-engine",
        &["loom-kernel", "loom-providers", "loom-store", "loom-tools"],
    );
    assert_still_wired("loom-engine", &["loom-kernel"]);
}

#[test]
fn the_edge_cases_depend_on_the_facade_alone() {
    // `loom-cli`'s own guard lives in `loom-cli/tests/facade.rs` (ADR-021).
    // `loom-eval` is the same shape: the harness opens every arm through
    // `EngineOverrides` on the engine rather than hand-wiring an `Agent`, and
    // it reaches engine code through `loom-core` so the arms exercise the same
    // public surface a real embedder would.
    for crate_name in ["loom-cli", "loom-eval"] {
        assert_no_workspace_deps(crate_name, &["loom-core"]);
        assert_still_wired(crate_name, &["loom-core"]);
    }
}

/// The kernel's `unsafe_code = "forbid"` is the other half of "cannot leak by
/// construction": a future edit that needs `unsafe` to satisfy a trait must
/// fail the suite, not just a lint nobody runs.
///
/// The forbid is declared once at the workspace root and inherited, so this
/// checks both halves of that chain — the root policy *and* the kernel's
/// opt-in to it. Checking either alone would pass on a broken inheritance: a
/// root policy the kernel stopped applying, or an `[lints]` block the root
/// never granted.
#[test]
fn the_kernel_forbids_unsafe() {
    let root =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"))
            .expect("reading the workspace root manifest");

    assert!(
        root.contains("unsafe_code = \"forbid\""),
        "the workspace no longer sets `unsafe_code = \"forbid\"` under \
         [workspace.lints.rust]; the kernel and tools both inherit that \
         policy (architecture §1)"
    );

    let kernel = manifest("loom-kernel");
    assert!(
        kernel.contains("[lints]") && kernel.contains("workspace = true"),
        "loom-kernel no longer opts into the workspace lints, so the \
         `unsafe_code = \"forbid\"` policy above is not applied to it"
    );
}

/// Guards against the guard: if a crate were renamed or added, the
/// `workspace_deps` lookup would silently stop resolving its name and the
/// layer assertions would pass while checking nothing.
#[test]
fn every_guarded_crate_is_a_known_workspace_crate() {
    for crate_name in [
        "loom-kernel",
        "loom-providers",
        "loom-store",
        "loom-tools",
        "loom-engine",
        "loom-cli",
        "loom-eval",
    ] {
        assert!(
            WORKSPACE_CRATES.contains(&crate_name),
            "{crate_name} is asserted on but missing from WORKSPACE_CRATES"
        );
    }
    assert_eq!(
        WORKSPACE_CRATES.len(),
        8,
        "the crate count changed; update WORKSPACE_CRATES and \
         `architecture.md` §1 so the layering table stays exhaustive"
    );
}
