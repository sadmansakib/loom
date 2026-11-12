#[cfg(target_os = "linux")]
use super::Sandbox;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
impl Sandbox {
    pub(super) fn wrap_bubblewrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        let wrapper = find_bwrap().unwrap_or_else(|| "bwrap".to_owned());
        // Every directory outside the workspace is optional: its absence
        // must not abort the sandbox (see `bubblewrap_argv`).
        let optional: Vec<PathBuf> = self
            .cargo_home
            .iter()
            .cloned()
            .chain(self.grants.extra_writable.iter().cloned())
            .collect();
        let home = std::env::var_os("HOME").map(|home| home.to_string_lossy().into_owned());
        let protected = self.protected();
        let secrets = self.home_secrets();
        let wrapped = bubblewrap_argv(
            program,
            args,
            &BubblewrapWritable {
                workspace: &self.workspace,
                optional: &optional,
                temp: &self.temp,
            },
            &protected,
            &secrets,
            self.grants.network,
            home.as_deref(),
        );
        (wrapper, wrapped)
    }
}

/// The writable set `bubblewrap_argv` must bind, split by whether a
/// missing source is tolerable.
pub(super) struct BubblewrapWritable<'a> {
    /// The workspace. A missing source here is a caller mistake, so it is
    /// the one bind allowed to be strict.
    pub(super) workspace: &'a Path,
    /// Directories bound read-write only when they exist.
    pub(super) optional: &'a [PathBuf],
    /// `TMPDIR`: the same tolerance as `optional`.
    pub(super) temp: &'a Path,
}

/// Assembles the bwrap invocation: the wrapper's own options, then the
/// COMMAND and its arguments.
///
/// This lives outside `#[cfg(target_os = "linux")]` on purpose, so the
/// *shape* of the invocation is unit-tested on every platform. Both
/// Linux-only defects it now guards against — a missing `--unshare-pid`
/// in the kill cascade, and a plain `--bind` on a directory that need not
/// exist — shipped green on a macOS dev box precisely because only the
/// wrapper that consumes this argv was platform-gated, so nothing ever
/// inspected the argv itself.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn bubblewrap_argv(
    program: &str,
    args: &[String],
    writable: &BubblewrapWritable<'_>,
    protected: &[PathBuf],
    secrets: &[PathBuf],
    network: bool,
    home: Option<&str>,
) -> Vec<String> {
    let mut wrapped = vec![
        "--die-with-parent".to_owned(),
        "--new-session".to_owned(),
        // Required for the kill cascade to be complete.
        // `--new-session` calls setsid() in bwrap's sandboxed child,
        // which moves the COMMAND into a new session and process
        // group, so `killpg` on the wrapper's group can only ever
        // reach bwrap itself. `--unshare-pid` puts the COMMAND in its
        // own PID namespace, where bwrap's PID-1 reaper reaps
        // orphans and the kernel sends SIGKILL to every remaining
        // process when the namespace empties. `--die-with-parent`
        // sets PR_SET_PDEATHSIG on that PID 1, so when the wrapper
        // dies (policy timeout, cancellation) the namespace is torn
        // down and `cargo` plus every `rustc` under it die with it.
        // Without this flag the whole command tree survives the kill,
        // which contradicts architecture.md §2/§6.8.
        //
        // Depends on `--proc /proc` below: with `--unshare-pid` set,
        // bwrap mounts a fresh procfs instead of bind-mounting the
        // host one.
        "--unshare-pid".to_owned(),
    ];
    if !network {
        wrapped.push("--unshare-net".to_owned());
    }
    wrapped.extend(
        [
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--ro-bind-try",
            "/usr",
            "/usr",
            // Rustup toolchains and cargo/rustup shims are commonly
            // installed under /usr/local (including SWE-bench images).
            // Keep the installation visible but read-only; Cargo's
            // configured home is mounted separately as writable below.
            "--ro-bind-try",
            "/usr/local",
            "/usr/local",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--ro-bind-try",
            "/bin",
            "/bin",
            "--ro-bind-try",
            "/sbin",
            "/sbin",
            "--ro-bind-try",
            "/etc",
            "/etc",
            "--ro-bind-try",
            "/opt",
            "/opt",
            "--ro-bind-try",
            "/nix",
            "/nix",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>(),
    );
    // The home directory stays readable; only the writable set is
    // bind-mounted read-write.
    if let Some(home) = home {
        wrapped.extend(
            ["--ro-bind-try", home, home]
                .iter()
                .map(|s| (*s).to_owned()),
        );
    }
    // The workspace is the one bind that must exist: running a command
    // against a missing workspace is a misconfiguration, and failing
    // loudly beats silently running somewhere else. (bwrap's setup
    // loop dies with "Can't open source" when a `--bind` source is
    // absent, so this cannot silently become a no-op.)
    let workspace = writable.workspace.to_string_lossy().into_owned();
    wrapped.extend(
        ["--bind", &workspace, &workspace]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    // Optional writable dirs tolerate absence. `--bind` would abort the
    // entire sandbox when the source is missing, and a Linux host with
    // no `~/.cargo` and no `CARGO_HOME` (containers, CI, rustup
    // installed elsewhere) would then fail *every* `run_command` and
    // `verify` invocation rather than just the ones that need cargo.
    // `--bind-try` skips a missing source and still binds a present
    // one read-write, which is the intended grant either way.
    for dir in writable.optional {
        let dir = dir.to_string_lossy().into_owned();
        wrapped.extend(["--bind-try", &dir, &dir].iter().map(|s| (*s).to_owned()));
    }
    // /tmp is a tmpfs mounted above; this bind only matters when
    // TMPDIR points somewhere else, and is optional for the same
    // reason as the grants.
    let temp = writable.temp.to_string_lossy().into_owned();
    wrapped.extend(["--bind-try", &temp, &temp].iter().map(|s| (*s).to_owned()));
    // Control state (`protected`) is bound read-only *after* every
    // read-write bind: bubblewrap mounts in argument order, so a later
    // `--ro-bind-try` shadows the workspace bind at that subtree and
    // writes to `.loom`, `.cargo`, or git's config/hooks land on a
    // read-only mount. `-try` keeps a fresh workspace without `.git`
    // (or without `.loom`) from aborting the sandbox.
    for path in protected {
        let path = path.to_string_lossy().into_owned();
        wrapped.extend(
            ["--ro-bind-try", &path, &path]
                .iter()
                .map(|s| (*s).to_owned()),
        );
    }
    // Home secrets (`secrets`) are masked (F-11): the home dir is already
    // readable, so existing secret directories need their own tmpfs mount
    // and secret files are covered with /dev/null. Missing paths must be
    // skipped: bwrap cannot create a mountpoint below the read-only home
    // bind, and there is no credential material at a path that does not
    // exist.
    for path in secrets {
        if path.is_dir() {
            let path = path.to_string_lossy().into_owned();
            wrapped.extend(["--tmpfs", &path].iter().map(|s| (*s).to_owned()));
        } else if path.is_file() {
            let path = path.to_string_lossy().into_owned();
            wrapped.extend(
                ["--ro-bind", "/dev/null", &path]
                    .iter()
                    .map(|s| (*s).to_owned()),
            );
        }
    }
    wrapped.extend(["--chdir", &workspace].iter().map(|s| (*s).to_owned()));
    wrapped.push(program.to_owned());
    wrapped.extend(args.iter().cloned());
    wrapped
}

/// Searches `PATH` (plus common fixed locations) for `bwrap`.
#[cfg(target_os = "linux")]
pub(super) fn find_bwrap() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("bwrap")));
    }
    candidates.extend(
        [
            "/usr/local/bin/bwrap",
            "/usr/bin/bwrap",
            "/opt/homebrew/bin/bwrap",
        ]
        .iter()
        .map(PathBuf::from),
    );
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}
