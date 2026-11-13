//! The formatting intent: route the task's explicit `cargo fmt` /
//! `gofmt` phrasing to the workspace's own formatter and verify.

use std::path::{Path, PathBuf};

use crate::policy::CommandPolicy;
/// Routes a formatting intent to the workspace's formatter. The
/// phrase-class intents stay strict: `FormatRust` only ever runs
/// `cargo fmt`, `FormatGo` only `gofmt`, and the neutral phrase picks
/// whatever the manifest says. Ecosystems without a deterministic,
/// workspace-local formatter (npm, Python, Make) decline.
pub(super) async fn apply_format(
    workspace: &Path,
    policy: &CommandPolicy,
    class: FormatClass,
) -> Option<FastPathOutcome> {
    let ecosystem = crate::verify::detect_ecosystem(workspace);
    match class {
        FormatClass::Rust if ecosystem == Some(crate::verify::Ecosystem::Cargo) => {
            apply_format_cargo(workspace, policy).await
        }
        FormatClass::Go if ecosystem == Some(crate::verify::Ecosystem::Go) => {
            apply_format_go(workspace, policy).await
        }
        FormatClass::Auto => match ecosystem {
            Some(crate::verify::Ecosystem::Cargo) => apply_format_cargo(workspace, policy).await,
            Some(crate::verify::Ecosystem::Go) => apply_format_go(workspace, policy).await,
            Some(
                crate::verify::Ecosystem::Python
                | crate::verify::Ecosystem::Npm
                | crate::verify::Ecosystem::Make,
            )
            | None => None,
        },
        FormatClass::Rust | FormatClass::Go => None,
    }
}

use super::verify::{cargo_ok, program_ok, program_output};
use super::workspace::{Snapshot, restore, rust_files};

use super::FastPathOutcome;

/// Which formatter a phrase class demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormatClass {
    Rust,
    Go,
    Auto,
}

/// Applies the format intent to a Cargo workspace: `cargo fmt`, then
/// format-idempotence and compile verification.
async fn apply_format_cargo(workspace: &Path, policy: &CommandPolicy) -> Option<FastPathOutcome> {
    // Pre-check names the files fmt would touch, so the snapshot is
    // exactly the revert set.
    let check = program_output(workspace, policy, "cargo", &["fmt", "--check"]).await?;
    let unformatted = u32::try_from(
        check
            .lines()
            .filter(|line| line.starts_with("Diff in "))
            .count(),
    )
    .unwrap_or(u32::MAX);
    if unformatted == 0 {
        return Some(FastPathOutcome {
            kind: "format",
            files_changed: 0,
            note: "Fast path (format): the codebase is already \
                   rustfmt-clean; nothing to change. Verified with \
                   `cargo fmt --check`."
                .to_owned(),
        });
    }
    let snapshots: Vec<Snapshot> = rust_files(workspace)
        .into_iter()
        .filter_map(|path| {
            std::fs::read(&path)
                .ok()
                .map(|bytes| Snapshot { path, bytes })
        })
        .collect();
    if !cargo_ok(workspace, policy, &["fmt"]).await {
        restore(&snapshots);
        return None;
    }
    let green = cargo_ok(workspace, policy, &["fmt", "--check"]).await
        && cargo_ok(workspace, policy, &["check"]).await;
    if !green {
        restore(&snapshots);
        return None;
    }
    Some(FastPathOutcome {
        kind: "format",
        files_changed: unformatted,
        note: format!(
            "Fast path (format): `cargo fmt` applied to {unformatted} file(s); \
             verified with `cargo fmt --check` and `cargo check`."
        ),
    })
}

/// Applies the format intent to a Go module: `gofmt -l .` names the
/// unformatted files (the revert set), `gofmt -w` rewrites them, and
/// idempotence plus `go build ./...` verify green. A module whose
/// dependencies need downloading fails `go build` under the sandbox's
/// network-off default and the whole path declines — never worse.
async fn apply_format_go(workspace: &Path, policy: &CommandPolicy) -> Option<FastPathOutcome> {
    let listed = program_output(workspace, policy, "gofmt", &["-l", "."]).await?;
    let files: Vec<PathBuf> = listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('/') && !line.contains(".."))
        .map(|relative| workspace.join(relative))
        .collect();
    if files.is_empty() {
        return Some(FastPathOutcome {
            kind: "format",
            files_changed: 0,
            note: "Fast path (format): the module is already \
                   gofmt-clean; nothing to change. Verified with \
                   `gofmt -l .`."
                .to_owned(),
        });
    }
    if files.len() > 500 {
        return None;
    }
    let snapshots: Vec<Snapshot> = files
        .iter()
        .filter_map(|path| {
            std::fs::read(path).ok().map(|bytes| Snapshot {
                path: path.clone(),
                bytes,
            })
        })
        .collect();
    let owned: Vec<String> = files
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let mut write_args: Vec<&str> = vec!["-w"];
    write_args.extend(owned.iter().map(String::as_str));
    if !program_ok(workspace, policy, "gofmt", &write_args).await {
        restore(&snapshots);
        return None;
    }
    let still_listed = program_output(workspace, policy, "gofmt", &["-l", "."])
        .await
        .is_some_and(|out| out.lines().any(|line| !line.trim().is_empty()));
    let green = !still_listed && program_ok(workspace, policy, "go", &["build", "./..."]).await;
    if !green {
        restore(&snapshots);
        return None;
    }
    Some(FastPathOutcome {
        kind: "format",
        files_changed: u32::try_from(files.len()).unwrap_or(u32::MAX),
        note: format!(
            "Fast path (format): `gofmt` applied to {} file(s); verified \
             with `gofmt -l .` and `go build ./...`.",
            files.len()
        ),
    })
}
