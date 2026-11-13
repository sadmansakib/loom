//! Hardening for the git invocations loom itself runs on the host —
//! the facts scan (`repo.rs`), worktree machinery (`worktrees.rs`),
//! and the CLI's rewind checkpoint (`cli/workspace.rs`). Distinct
//! from model-driven git, which runs inside the `run_command`
//! sandbox.
//!
//! Threat (adversarial review, F-03): `.git/config` is
//! repo-controlled state, and two of its knobs name *programs git
//! will execute* — `core.fsmonitor` spawns on every status, and
//! `core.hooksPath` relocates hooks anywhere on disk. Before the
//! control-state carve-out, a sandboxed child could plant them; a
//! tarball-style "repo" shipped with `.git` included still can. These
//! overrides disable both knobs for loom's own invocations regardless
//! of what the repo says, and `GIT_CONFIG_NOSYSTEM` keeps a
//! compromised system config out. Global (`~/.gitconfig`) config
//! stays honored: it is operator-owned, and neutralizing it would
//! break `insteadOf` rewrites and commit identity.
//!
//! `core.hooksPath=/dev/null` works because git execs
//! `<hooksPath>/<hook-name>`: `/dev/null/pre-commit` fails `ENOTDIR`
//! and the hook is skipped.
//!
//! Residual, closed (follow-up to the PR2 residual): repo config
//! naming `filter.*`/`diff.*` commands used to still execute on
//! content-rewriting operations (`git add -A` runs clean filters;
//! worktree checkout and `reset --hard` run smudge filters). The
//! diff sites pass `--no-ext-diff --no-textconv`, and every host
//! invocation now also probes repo-configured `filter.*` sections
//! and overrides them to identity pass-throughs
//! ([`neutralize_filters`]) — so repo config no longer names *any*
//! program git executes on loom's behalf.

/// The `-c` overrides every host-side loom git invocation prepends.
/// `core.quotepath=false` is parsing hygiene (unambiguous porcelain
/// paths); the other two are the code-execution kills.
pub const CONFIG_ARGS: &[&str] = &[
    "-c",
    "core.quotepath=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

use std::path::Path;

/// Environment every host-side loom git invocation sets: never read
/// the system-level config (`/etc/gitconfig`, or wherever the distro
/// moved it) — loom's git runs need none of it.
pub const ENV_NOSYSTEM: (&str, &str) = ("GIT_CONFIG_NOSYSTEM", "1");

#[must_use]
/// Turns the stdout of `git config --local --get-regexp ^filter\.`
/// into `-c` overrides that neutralize every repo-configured filter:
/// `clean`/`smudge` become `cat`, `process` is emptied so git falls
/// back to the (now identity) clean/smudge pair, and `required=false`
/// means a filter that still fails passes content through unfiltered
/// instead of aborting. Closing the PR2 residual: repo config can no
/// longer name *any* program git executes on content-rewriting
/// operations (`add -A` clean filters, worktree/`reset --hard`
/// smudge filters). Global (`~/.gitconfig`) filters are untouched —
/// operator-owned by the same line the rest of that file is.
pub fn filter_neutralizer_args(config_regexp_output: &str) -> Vec<String> {
    let mut names: Vec<&str> = Vec::new();
    for line in config_regexp_output.lines() {
        let Some(key) = line.split_whitespace().next() else {
            continue;
        };
        let Some(name) = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.split('.').next())
        else {
            continue;
        };
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    let mut args = Vec::with_capacity(names.len() * 8);
    for name in names {
        for (key, value) in [
            ("clean", "cat"),
            ("smudge", "cat"),
            ("process", ""),
            ("required", "false"),
        ] {
            args.push("-c".to_owned());
            args.push(format!("filter.{name}.{key}={value}"));
        }
    }
    args
}

#[must_use]
/// The filter neutralizers for `dir`'s repo config: probes
/// `filter.*` with the same hardened invocation shape the callers
/// use, then maps the result through [`filter_neutralizer_args`]. A
/// probe failure yields no overrides — the manifest trust gate owns
/// that corner, and a broken `git config` leaves nothing to execute.
pub fn neutralize_filters(dir: &Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(CONFIG_ARGS)
        .env(ENV_NOSYSTEM.0, ENV_NOSYSTEM.1)
        .arg("-C")
        .arg(dir)
        .args(["config", "--local", "--get-regexp", r"^filter\."])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            filter_neutralizer_args(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_names_map_to_identity_overrides() {
        let args = filter_neutralizer_args(
            "filter.lfs.clean git-lfs clean -- %f\n\
             filter.lfs.smudge git-lfs smudge -- %f\n\
             filter.lfs.process git-lfs filter-process\n\
             filter.lfs.required true\n\
             filter.evil.clean /tmp/run-me\n",
        );
        assert_eq!(
            args,
            vec![
                "-c",
                "filter.lfs.clean=cat",
                "-c",
                "filter.lfs.smudge=cat",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.required=false",
                "-c",
                "filter.evil.clean=cat",
                "-c",
                "filter.evil.smudge=cat",
                "-c",
                "filter.evil.process=",
                "-c",
                "filter.evil.required=false",
            ]
        );
        // Unknown or non-filter keys and blank lines are ignored.
        assert_eq!(
            filter_neutralizer_args("core.fsmonitor x\n\nremote.origin.url y\n").len(),
            0
        );
    }
}
