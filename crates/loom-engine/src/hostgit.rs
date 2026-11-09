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
//! Residual, stated: repo config can still name `filter.*` and
//! `diff.*` commands, which execute on content-rewriting operations
//! (`git add -A` runs clean filters; `git worktree add` runs smudge
//! filters on checkout). There is no blanket `-c` for unnamed filter
//! sections; the diff sites pass `--no-ext-diff --no-textconv`, and
//! the shipped-manifest trust gate is the follow-up owner of this
//! class.

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

/// Environment every host-side loom git invocation sets: never read
/// the system-level config (`/etc/gitconfig`, or wherever the distro
/// moved it) — loom's git runs need none of it.
pub const ENV_NOSYSTEM: (&str, &str) = ("GIT_CONFIG_NOSYSTEM", "1");
