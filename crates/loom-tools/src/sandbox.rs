//! OS sandbox for executed commands (roadmap: sandboxed shell).
//!
//! The allow-list of bare programs is replaced by "any command inside an
//! OS sandbox": writes confined to the workspace plus declared cache
//! directories, network off by default. The wrapper is an external
//! program so no `unsafe` pre-exec hooks are ever needed (the workspace
//! forbids `unsafe`):
//!
//! * macOS — `sandbox-exec` with a generated Seatbelt profile
//!   (`(allow default)` then re-`deny` the two deviation axes, so the
//!   default-allow surface stays broad enough for real toolchains).
//! * Linux — `bubblewrap` (`bwrap`) with read-only system roots, a
//!   tmpfs `/tmp`, `--unshare-net`, and bind-mounts for the writable
//!   set. Landlock needs in-process syscalls the `unsafe` ban rules
//!   out, so the wrapper is the Linux path.
//!
//! The command policy's manifest stays the escalation layer: a rule may
//! grant network access, extra writable directories, or unsandboxed
//! execution, alongside the shipped limits (timeout, output window,
//! env scrubbing — the credential floor applies either way).
//!
//! Sandboxing is inherited: everything a sandboxed command spawns lives
//! in the same profile/namespace, so a `bash -c` pipeline is confined
//! exactly like a bare program.

use std::path::{Path, PathBuf};

/// Escalation grants from the command policy, beyond the sandbox
/// default. `None` rule (previously: rejected program) means the pure
/// default: sandboxed, no network, no extra writable paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SandboxGrants {
    /// Run outside the sandbox entirely (explicit operator opt-out).
    pub enabled: bool,
    /// Allow outbound and local network access.
    pub network: bool,
    /// Extra writable directories (absolute paths, from the manifest).
    pub extra_writable: Vec<PathBuf>,
}

impl SandboxGrants {
    /// The grants a policy rule carries; no rule means the boxed
    /// default.
    pub(crate) fn from_rule(rule: Option<&crate::policy::CommandRule>) -> Self {
        match rule {
            None => Self {
                enabled: true,
                network: false,
                extra_writable: Vec::new(),
            },
            Some(rule) => Self {
                enabled: rule.sandbox,
                network: rule.network,
                extra_writable: rule.extra_writable.iter().map(PathBuf::from).collect(),
            },
        }
    }
}

/// A resolved sandbox for one command invocation: the writable set is
/// concrete (canonical where possible), the grants fixed.
#[derive(Debug, Clone)]
pub(crate) struct Sandbox {
    grants: SandboxGrants,
    /// Workspace root — always writable.
    workspace: PathBuf,
    /// Temp directory (`TMPDIR`/`/tmp`) — always writable; builds
    /// scratch here.
    temp: PathBuf,
    /// Cargo home (`CARGO_HOME` or `~/.cargo`) when it exists —
    /// writable so registry locks and caches work; without it a plain
    /// `cargo build` cannot run at all.
    cargo_home: Option<PathBuf>,
}

impl Sandbox {
    /// Resolves the writable set for a command in `workspace` under
    /// `grants`.
    pub(crate) fn resolve(workspace: &Path, grants: SandboxGrants) -> Self {
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let temp = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
        // The cargo home is legitimately absent (a container's first
        // `cargo` run creates it, and `rustup` installs elsewhere on many
        // hosts). It is kept in the set either way: it is bound with
        // `--bind-try`, which bwrap skips when the source is missing —
        // unlike a plain `--bind`, which aborts the whole sandbox setup.
        Self {
            grants,
            workspace,
            temp,
            cargo_home,
        }
    }

    /// The writable set: workspace, temp, cargo home, then grants.
    fn writable(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.workspace.clone(), self.temp.clone()];
        if let Some(cargo_home) = &self.cargo_home {
            dirs.push(cargo_home.clone());
        }
        dirs.extend(self.grants.extra_writable.iter().cloned());
        dirs
    }

    /// Wraps `program args...` into the sandbox wrapper's argv, or
    /// passes it through when the grants disable the sandbox.
    pub(crate) fn wrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        if !self.grants.enabled {
            return (program.to_owned(), args.to_vec());
        }
        #[cfg(target_os = "macos")]
        return self.wrap_seatbelt(program, args);
        #[cfg(target_os = "linux")]
        return self.wrap_bubblewrap(program, args);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (program, args);
            (program.to_owned(), args.to_vec())
        }
    }
}

#[cfg(target_os = "macos")]
impl Sandbox {
    const WRAPPER: &str = "/usr/bin/sandbox-exec";

    fn wrap_seatbelt(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        // No process-group bookkeeping is needed here: `sandbox-exec`
        // execs the profile interpreter in-process and then execs the
        // COMMAND, so the child keeps the process group loom placed the
        // wrapper in and `killpg` reaches the whole tree. The Linux
        // wrapper forks and calls setsid(), which is why it needs the
        // explicit PID-namespace arrangement documented in
        // `bubblewrap_argv`.
        let profile = seatbelt_profile(&self.writable(), self.grants.network);
        let mut wrapped = vec![
            "-p".to_owned(),
            profile,
            "--".to_owned(),
            program.to_owned(),
        ];
        wrapped.extend(args.iter().cloned());
        (Self::WRAPPER.to_owned(), wrapped)
    }
}

#[cfg(target_os = "linux")]
impl Sandbox {
    fn wrap_bubblewrap(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
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
        let wrapped = bubblewrap_argv(
            program,
            args,
            &BubblewrapWritable {
                workspace: &self.workspace,
                optional: &optional,
                temp: &self.temp,
            },
            self.grants.network,
            home.as_deref(),
        );
        (wrapper, wrapped)
    }
}

/// The writable set `bubblewrap_argv` must bind, split by whether a
/// missing source is tolerable.
struct BubblewrapWritable<'a> {
    /// The workspace. A missing source here is a caller mistake, so it is
    /// the one bind allowed to be strict.
    workspace: &'a Path,
    /// Directories bound read-write only when they exist.
    optional: &'a [PathBuf],
    /// `TMPDIR`: the same tolerance as `optional`.
    temp: &'a Path,
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
fn bubblewrap_argv(
    program: &str,
    args: &[String],
    writable: &BubblewrapWritable<'_>,
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
    wrapped.extend(["--chdir", &workspace].iter().map(|s| (*s).to_owned()));
    wrapped.push(program.to_owned());
    wrapped.extend(args.iter().cloned());
    wrapped
}

/// Searches `PATH` (plus common fixed locations) for `bwrap`.
#[cfg(target_os = "linux")]
fn find_bwrap() -> Option<String> {
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

/// The Seatbelt profile: default-allow (real toolchains need read,
/// exec, mach-lookup, sysctl), then the two deviation axes re-denied —
/// all writes outside the writable set, and (unless granted) all
/// network. Later filters win, so the subpath allows land after the
/// blanket deny.
pub(crate) fn seatbelt_profile(writable: &[PathBuf], network: bool) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    if !writable.is_empty() {
        profile.push_str("(allow file-write* ");
        for (index, dir) in writable.iter().enumerate() {
            if index > 0 {
                profile.push(' ');
            }
            profile.push_str(&format!("(subpath \"{}\")", escape_seatbelt_path(dir)));
        }
        profile.push_str(")\n");
    }
    if !network {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// Escapes a path for a Seatbelt string literal: backslash and quote
/// are the two characters with meaning there.
fn escape_seatbelt_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_denies_writes_and_network_by_default() {
        let profile = seatbelt_profile(&[PathBuf::from("/ws")], false);
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(subpath \"/ws\")"));
        // The subpath allow must come after the blanket deny.
        let deny = profile.find("(deny file-write*)").unwrap();
        let allow = profile.find("(allow file-write*").unwrap();
        assert!(deny < allow);
    }

    #[test]
    fn seatbelt_network_grant_omits_the_deny() {
        let profile = seatbelt_profile(&[PathBuf::from("/ws")], true);
        assert!(!profile.contains("(deny network*)"));
    }

    #[test]
    fn seatbelt_escapes_path_metacharacters() {
        let profile = seatbelt_profile(&[PathBuf::from("/ws/na\"me\\dir")], false);
        assert!(profile.contains("/ws/na\\\"me\\\\dir"));
    }

    #[test]
    fn grants_default_to_sandboxed_without_network() {
        let grants = SandboxGrants::from_rule(None);
        assert!(grants.enabled);
        assert!(!grants.network);
        assert!(grants.extra_writable.is_empty());
    }

    /// The bwrap argv for a `cargo build --release` shaped command, with
    /// a cargo home and a granted writable directory that does not
    /// exist on this machine.
    fn bwrap_argv(network: bool, cargo_home: Option<&str>) -> Vec<String> {
        let optional: Vec<PathBuf> = cargo_home
            .into_iter()
            .map(PathBuf::from)
            .chain(std::iter::once(PathBuf::from("/loom-absent-granted-dir")))
            .collect();
        bubblewrap_argv(
            "cargo",
            &["build".to_owned(), "--release".to_owned()],
            &BubblewrapWritable {
                workspace: Path::new("/ws"),
                optional: &optional,
                temp: Path::new("/tmp"),
            },
            network,
            Some("/home/dev"),
        )
    }

    /// Every `flag SRC DEST` triple in `argv`, for bind-style flags.
    fn bind_pairs(argv: &[String], flag: &str) -> Vec<[String; 2]> {
        argv.iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == flag)
            .map(|(index, _)| [argv[index + 1].clone(), argv[index + 2].clone()])
            .collect()
    }

    #[test]
    fn bubblewrap_argv_closes_the_kill_cascade() {
        let argv = bwrap_argv(false, None);
        for flag in ["--die-with-parent", "--new-session", "--unshare-pid"] {
            assert!(
                argv.iter().any(|arg| arg == flag),
                "{flag} missing from {argv:?}"
            );
        }
        // `--unshare-pid` makes bwrap mount a fresh procfs instead of
        // bind-mounting the host one, and that only happens when the
        // sandbox asks for /proc.
        let proc = argv
            .iter()
            .position(|arg| arg == "--proc")
            .unwrap_or_else(|| panic!("--proc missing from {argv:?}"));
        assert_eq!(argv[proc + 1], "/proc");
    }

    #[test]
    fn bubblewrap_binds_only_the_workspace_strictly() {
        let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
        // A strict `--bind` on anything but the workspace aborts the whole
        // sandbox when that directory is absent — which on a Linux host
        // with no cargo home broke every single run_command invocation.
        for [src, dest] in bind_pairs(&argv, "--bind") {
            assert_eq!(
                (src.as_str(), dest.as_str()),
                ("/ws", "/ws"),
                "strict bind on an optional path in {argv:?}"
            );
        }
        // Optional writable dirs are bound with the tolerant form, whether
        // or not they exist right now.
        let tolerant = bind_pairs(&argv, "--bind-try");
        assert!(
            tolerant.contains(&["/home/dev/.cargo".to_owned(), "/home/dev/.cargo".to_owned()]),
            "cargo home not bound read-write in {tolerant:?}"
        );
        assert!(
            tolerant.contains(&[
                "/loom-absent-granted-dir".to_owned(),
                "/loom-absent-granted-dir".to_owned()
            ]),
            "granted writable dir not bound read-write in {tolerant:?}"
        );
    }

    #[test]
    fn bubblewrap_tolerates_an_absent_cargo_home() {
        // No cargo home, no grants: the only strict bind left is the
        // workspace, so a host without `~/.cargo` still gets a sandbox.
        let argv = bwrap_argv(false, None);
        let strict = bind_pairs(&argv, "--bind");
        assert_eq!(strict.len(), 1, "unexpected strict binds in {argv:?}");
        assert_eq!(strict[0], ["/ws".to_owned(), "/ws".to_owned()]);
    }

    #[test]
    fn bubblewrap_granted_network_drops_the_unshare() {
        assert!(
            !bwrap_argv(true, None)
                .iter()
                .any(|arg| arg == "--unshare-net"),
            "network was granted but still unshared"
        );
        assert!(
            bwrap_argv(false, None)
                .iter()
                .any(|arg| arg == "--unshare-net"),
            "network was not granted but is shared"
        );
    }

    #[test]
    fn bubblewrap_appends_the_command_and_its_args_last() {
        let argv = bwrap_argv(false, None);
        let command = argv.len() - 3..;
        assert_eq!(
            &argv[command],
            &[
                "cargo".to_owned(),
                "build".to_owned(),
                "--release".to_owned()
            ],
            "COMMAND must be the argv tail in {argv:?}"
        );
    }
}
