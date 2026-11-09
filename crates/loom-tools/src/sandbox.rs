//! OS sandbox for executed commands (roadmap: sandboxed shell).
//!
//! The allow-list of bare programs is replaced by "any command inside an
//! OS sandbox": writes confined to the workspace plus declared cache
//! directories, network off by default. The wrapper is an external
//! program so no `unsafe` pre-exec hooks are ever needed (the workspace
//! forbids `unsafe`):
//!
//! * macOS — `sandbox-exec` with a generated Seatbelt profile
//!   (`(allow default)` then re-`deny` the deviation axes — writes
//!   outside the writable set and, unless granted, network — plus an
//!   unconditional Apple-event deny, so the
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

/// The OS-sandbox baseline a rule-less command runs under, for the
/// trace's one-shot `sandbox` notice (v8): which wrapper confines
/// commands on this platform, or that none exists. Per-command
/// deviations (a policy opt-out, a platform without a wrapper) surface
/// separately as `sandbox.passthrough` notices.
pub fn baseline() -> &'static str {
    if cfg!(target_os = "macos") {
        "commands run OS-confined (macOS Seatbelt profile: workspace + temp + cargo home writable, network off)"
    } else if cfg!(target_os = "linux") {
        "commands run OS-confined (bubblewrap: workspace + temp + cargo home writable, network off)"
    } else {
        "no OS sandbox wrapper on this platform: commands run unconfined"
    }
}

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
    /// Cargo target dir (`CARGO_TARGET_DIR`) when set — writable so
    /// cargo's own build locks and artifacts resolve under the sandbox
    /// even when the build root lies elsewhere (e.g. a temp-dir crate
    /// that still targets the real repo's scratch).
    cargo_target_dir: Option<PathBuf>,
}

impl Sandbox {
    /// Resolves the writable set for a command in `workspace` under
    /// `grants`.
    pub(crate) fn resolve(workspace: &Path, grants: SandboxGrants) -> Self {
        let workspace = canonicalize_maybe(workspace);
        let temp = canonicalize_maybe(&std::env::temp_dir());
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
            .map(|dir| canonicalize_maybe(&dir));
        // Same tolerance for the cargo target dir: when `CARGO_TARGET_DIR`
        // is set it must be writable (its debug/.cargo-build-lock is opened
        // by every cargo invocation), even if it sits outside the workspace
        // being sandboxed — a temp-dir crate targeting the repo's scratch.
        let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .map(|dir| canonicalize_maybe(&dir));
        // Grants are operator paths spelled in a manifest; they get the
        // same canonicalization or a symlinked spelling (`/tmp/...` on
        // macOS) would never match the child's real syscalls (F-01).
        let mut grants = grants;
        grants.extra_writable = grants
            .extra_writable
            .iter()
            .map(|dir| canonicalize_maybe(dir))
            .collect();
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
            cargo_target_dir,
        }
    }

    /// The writable set: workspace, temp, cargo home, cargo target
    /// dir, then grants.
    fn writable(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.workspace.clone(), self.temp.clone()];
        if let Some(cargo_home) = &self.cargo_home {
            dirs.push(cargo_home.clone());
        }
        if let Some(cargo_target_dir) = &self.cargo_target_dir {
            dirs.push(cargo_target_dir.clone());
        }
        dirs.extend(self.grants.extra_writable.iter().cloned());
        dirs
    }

    /// Workspace control state that stays read-only *inside* the
    /// sandbox even though the workspace is writable (review items
    /// F-02/F-03/F-04/F-12): `.loom` policy manifests, `.cargo`
    /// config (aliases/wrappers execute any program), and the git
    /// config/hooks/modules subtree of `.git`, whose settings
    /// (`core.fsmonitor`, `core.hooksPath`, filter commands) host-side
    /// git executes outside the sandbox.
    ///
    /// Not the whole `.git`: the index and object database stay
    /// writable so `git status`/`git add`/`git commit` keep working.
    /// The list is denial-of-write only — reads are untouched, and a
    /// fresh `git init` inside the sandbox still creates its own
    /// config (harmless: host git is separately hardened with
    /// `-c` overrides when loom itself runs it).
    fn protected(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.workspace.join(".loom"),
            self.workspace.join(".cargo"),
            self.workspace.join(".git/config"),
            self.workspace.join(".git/config.lock"),
            self.workspace.join(".git/hooks"),
            self.workspace.join(".git/modules"),
        ];
        // The grants are the operator's explicit escalation; an
        // operator-granted writable dir wins over the defaults.
        for granted in &self.grants.extra_writable {
            paths.retain(|protected| !protected.starts_with(granted));
        }
        paths
    }

    /// Home state a sandboxed child must not *read* (review F-11):
    /// private key material, credential stores, and loom's own
    /// database and config. Denied even though the profile is
    /// otherwise default-allow — exfiltrating `~/.ssh` or the
    /// keychain is a credential breach, never a toolchain need; the
    /// macOS keychain's IPC channel is sealed by the same profile's
    /// blanket mach deny. The operator's single escalation axis
    /// applies: an `extra_writable` grant covering a secret drops it
    /// (e.g. an operator explicitly admitting sandboxed `ssh`).
    fn home_secrets(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut push = |path: PathBuf| {
            if !paths.contains(&path) {
                paths.push(path);
            }
        };
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            for secret in [
                ".ssh",
                ".gnupg",
                ".aws",
                ".kube",
                ".netrc",
                ".git-credentials",
                ".config/loom",
                ".local/share/loom",
            ] {
                push(home.join(secret));
            }
            // macOS keychain material on disk; the daemon path is the
            // mach deny.
            push(home.join("Library/Keychains"));
            push(home.join("Library/Security"));
        }
        // Test/CI overrides point loom state at other locations; cover
        // them when present so a sandboxed child cannot read the very
        // database the tests run against.
        for var in ["LOOM_DATA_DIR", "LOOM_DB", "LOOM_CONFIG_DIR"] {
            if let Some(path) = std::env::var_os(var) {
                push(PathBuf::from(path));
            }
        }
        for granted in &self.grants.extra_writable {
            paths.retain(|secret| !secret.starts_with(granted));
        }
        paths
    }

    /// Whether commands under this sandbox are OS-confined. `false`
    /// means every wrap passes through: the policy opted out, or the
    /// platform has no wrapper at all — a fact the trace surfaces as a
    /// `sandbox.passthrough` notice rather than a silent difference.
    pub(crate) fn confined(&self) -> bool {
        if !self.grants.enabled {
            return false;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            true
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
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
        let profile = seatbelt_profile(
            &self.writable(),
            &self.protected(),
            &self.home_secrets(),
            self.grants.network,
        );
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
    // Home secrets (`secrets`) are masked with an empty tmpfs (F-11):
    // the home dir is already readable, so `~/.ssh` et al. need their
    // own mount shadow to read empty instead of credential material.
    for path in secrets {
        let path = path.to_string_lossy().into_owned();
        wrapped.extend(["--tmpfs", &path].iter().map(|s| (*s).to_owned()));
    }
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

/// Probes that the platform's sandbox wrapper can actually start a
/// process — before a task runs, not at the first `run_command`.
///
/// On Linux this executes the same smoke invocation CI uses (see
/// `.github/workflows/ci.yml`): `bwrap --die-with-parent --new-session
/// --unshare-pid --unshare-net --ro-bind-try /usr /usr --proc /proc
/// --tmpfs /tmp -- true`. A host whose kernel/AppArmor policy denies
/// unprivileged user namespaces — Ubuntu 24.04+ by default — fails here
/// with actionable remediation instead of failing every sandboxed
/// command mid-task with a bare `bwrap: setting up uid map: Permission
/// denied`.
///
/// On macOS (`/usr/bin/sandbox-exec` ships with the OS) and on targets
/// where [`Sandbox::wrap`] passes through unchanged, the probe is a
/// no-op. See `docs/linux-sandbox.md` for the distro matrix.
pub fn preflight() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux_preflight()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

/// The Linux probe: locate `bwrap`, run the CI smoke command, and map a
/// setup failure through [`bwrap_remediation`].
#[cfg(target_os = "linux")]
fn linux_preflight() -> Result<(), String> {
    let Some(bwrap) = find_bwrap() else {
        return Err(
            "bubblewrap (`bwrap`) not found on PATH — the Linux sandbox has no wrapper.\n\
             Fix: install it (`apt install bubblewrap`, `dnf install bubblewrap`, or \
             `pacman -S bubblewrap`; use `bubblewrap-suid` on hardened kernels), or set \
             `sandbox = false` in `.loom/commands.toml` for the commands that must run \
             unsandboxed. See docs/linux-sandbox.md."
                .to_owned(),
        );
    };
    // The exact CI smoke shape, so what passes CI passes preflight.
    let probe = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-net",
        "--ro-bind-try",
        "/usr",
        "/usr",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--",
        "true",
    ];
    let output = std::process::Command::new(&bwrap)
        .args(probe)
        .output()
        .map_err(|error| format!("failed to start the sandbox probe ({bwrap}): {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = match bwrap_remediation(&stderr) {
        Some(remediation) => format!("{stderr}\n{remediation}"),
        None => format!("{stderr}\nSee docs/linux-sandbox.md for the distro matrix."),
    };
    Err(format!(
        "the sandbox probe failed (status {}):\n{detail}",
        output.status
    ))
}

/// Actionable remediation for a sandbox *setup* failure — a `bwrap:`
/// line on stderr means the wrapper died before the command ever ran,
/// so the usual "your command failed" reading of the output is wrong.
///
/// Returns `None` for output that carries no wrapper setup signature,
/// so ordinary command failures are never annotated. Used by
/// [`preflight`] and appended to failed `run_command` output, where the
/// raw `bwrap:` line alone would otherwise read as a defect in the
/// agent's command rather than a host-policy denial. The full distro
/// matrix lives in `docs/linux-sandbox.md`.
pub fn bwrap_remediation(stderr: &str) -> Option<String> {
    // Only the wrapper speaks with this prefix; a command that merely
    // mentions bubblewrap in its own output must not be annotated.
    if !stderr.lines().any(|line| line.starts_with("bwrap:")) {
        return None;
    }
    let escape = "Alternatively, set `sandbox = false` for the affected commands in \
         `.loom/commands.toml` (an explicit operator opt-out).";
    if stderr.contains("setting up uid map")
        || stderr.contains("No permissions to create new namespace")
        || stderr.contains("Failed to create new user namespace")
    {
        return Some(format!(
            "The host denies unprivileged user namespaces, so the sandbox cannot start \
             (Ubuntu 24.04+ and other AppArmor-restricted hosts are affected by default). \
             Fixes, in order of preference:\n\
             1. Install an AppArmor profile granting `userns,` to the loom binary — the \
             per-binary profile pattern Ubuntu uses for flatpak/steam; profile stanza in \
             docs/linux-sandbox.md.\n\
             2. Install the setuid bubblewrap (`bubblewrap-suid` on Arch's linux-hardened, \
             or your distro's equivalent), which sets up namespaces with privileges.\n\
             3. Last resort, host-wide: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.\n\
             In containers, run with `--security-opt seccomp=unconfined` and \
             `--security-opt apparmor=unconfined`.\n\
             {escape}"
        ));
    }
    if stderr.contains("seccomp") || stderr.contains("Failed to clone") {
        return Some(format!(
            "The container's default seccomp/AppArmor profile blocks the namespace \
             syscalls bubblewrap needs. Run the container with `--security-opt \
             seccomp=unconfined --security-opt apparmor=unconfined`, or use a host with \
             a working sandbox. See docs/linux-sandbox.md.\n{escape}"
        ));
    }
    Some(format!(
        "bubblewrap could not set up the sandbox (see its message above). See the \
         distro matrix in docs/linux-sandbox.md — an AppArmor `userns,` rule for the \
         loom binary or the `bubblewrap-suid` package usually fixes this.\n{escape}"
    ))
}

/// The Seatbelt profile: default-allow (real toolchains need read,
/// exec, mach-lookup, sysctl), then re-denied — all writes outside
/// the writable set (with `/dev/null` re-allowed as a sink device
/// real toolchains open read-write), Apple-event IPC, and (unless
/// granted) all network. Later filters win, so the subpath allows
/// land after the blanket deny.
///
/// Apple events get two layers because the classic filter no longer
/// enforces everywhere: `(deny appleevent-send)` is kept for macOS
/// versions where it still bites, and on current macOS (verified
/// empirically — an `osascript` "tell Finder" succeeded under the
/// deny, and the `destination` filter key is not even in the schema)
/// the operative control is the exec deny of the stock OSA
/// interpreter. A copied `osascript` is not a bypass: re-signing
/// system binaries fails under codesigning enforcement, and the
/// deny is defense-in-depth over the model never being asked to
/// script other applications in the first place.
pub(crate) fn seatbelt_profile(
    writable: &[PathBuf],
    protected: &[PathBuf],
    secrets: &[PathBuf],
    network: bool,
) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    // /dev/null must be writable: real git opens it read-write during
    // ordinary setup, and the blanket deny made every `git --version`
    // fatal under the sandbox (pre-existing; surfaced by review item
    // 2.4's confirmation step). A sink device is safe to allow.
    profile.push_str("(allow file-write* (subpath \"/dev/null\"))\n");
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
    // Control state is re-denied *after* the writable allow: later
    // filters win, so the workspace stays writable while `.loom`,
    // `.cargo`, and git's config/hooks/modules subtree do not.
    // (Verified empirically: subpath matching resolves case variants —
    // `.GIT/config` matches `.git/config` — because the filter sees the
    // vnode's on-disk name, not the syscall string.)
    if !protected.is_empty() {
        profile.push_str("(deny file-write*");
        for dir in protected {
            profile.push_str(&format!(" (subpath \"{}\")", escape_seatbelt_path(dir)));
        }
        profile.push_str(")\n");
    }
    // Home secrets are denied *read* even under `(allow default)`
    // (F-11): private keys, credential stores, loom's own database.
    if !secrets.is_empty() {
        profile.push_str("(deny file-read*");
        for path in secrets {
            profile.push_str(&format!(" (subpath \"{}\")", escape_seatbelt_path(path)));
        }
        profile.push_str(")\n");
    }
    profile.push_str("(deny appleevent-send)\n");
    profile.push_str("(deny process-exec* (subpath \"/usr/bin/osascript\"))\n");
    // LaunchServices seal (F-08): `open -g -j ./P.app` reached
    // LaunchServices through `(allow default)` and started a bundle
    // *outside* the sandbox. On current macOS named mach-lookup
    // denials parse but do not enforce (verified empirically for the
    // LaunchServices and syslog service names, matching the
    // `appleevent-send` finding), so the blanket class deny is the
    // operative control. Verified not to disturb what the profile
    // exists for: git, cargo (including a real build), python,
    // sw_vers, and DNS/HTTP under a network grant all run under it.
    // The exec denies on `open`/`automator`/`shortcuts` are friction
    // against the same vector through other front doors.
    profile.push_str("(deny mach-lookup)\n");
    profile.push_str("(deny process-exec* (subpath \"/usr/bin/open\"))\n");
    profile.push_str("(deny process-exec* (subpath \"/usr/bin/automator\"))\n");
    profile.push_str("(deny process-exec* (subpath \"/usr/bin/shortcuts\"))\n");
    if !network {
        profile.push_str("(deny network*)\n");
    }
    profile
}
/// Canonicalizes `path` as far as it exists, keeping the
/// not-yet-created remainder verbatim. Sandbox profile matching
/// (Seatbelt `subpath`, bwrap binds) is path-based, so a writable
/// entry spelled through a symlink — `/tmp/...` where macOS resolves
/// to `/private/tmp/...` — never matches the child's real syscalls
/// and every write lands on the blanket deny (review F-01: this is
/// what made a symlinked `CARGO_TARGET_DIR` fail eight tests).
fn canonicalize_maybe(path: &Path) -> PathBuf {
    if let Ok(full) = path.canonicalize() {
        return full;
    }
    // Walk up to the deepest existing ancestor, canonicalize that, and
    // rejoin the pending tail.
    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while !current.exists() {
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                pending.push(name.to_owned());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    match current.canonicalize() {
        Ok(base) => pending.iter().rev().fold(base, |acc, part| acc.join(part)),
        Err(_) => path.to_path_buf(),
    }
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
        let profile = seatbelt_profile(&[PathBuf::from("/ws")], &[], &[], false);
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(deny appleevent-send)"));
        assert!(profile.contains("(subpath \"/usr/bin/osascript\")"));
        assert!(profile.contains("(subpath \"/ws\")"));
        assert!(profile.contains("(subpath \"/dev/null\")"));
        // The subpath allow must come after the blanket deny.
        let deny = profile.find("(deny file-write*)").unwrap();
        let allow = profile.find("(allow file-write*").unwrap();
        assert!(deny < allow);
    }

    #[test]
    fn seatbelt_re_denies_control_state_after_the_writable_allow() {
        // Later filters win, so the control-state deny must land after
        // the workspace allow it carves out of.
        let workspace = PathBuf::from("/ws");
        let sandbox = Sandbox::resolve(&workspace, SandboxGrants::from_rule(None));
        let profile = seatbelt_profile(&[workspace], &sandbox.protected(), &[], false);
        for relative in [".loom", ".cargo", ".git/config", ".git/hooks"] {
            assert!(
                profile.contains(&format!("(subpath \"/ws/{relative}\")")),
                "{relative} missing from the control-state deny in:\n{profile}"
            );
        }
        let allow = profile.find("(allow file-write*").unwrap();
        let deny = profile.rfind("(deny file-write*").unwrap();
        assert!(
            allow < deny,
            "control-state deny must follow the allow:\n{profile}"
        );
    }

    #[test]
    fn grants_override_the_control_state_defaults() {
        // An operator-granted writable dir is the explicit escalation
        // layer; the defaults must not fight it.
        let workspace = PathBuf::from("/ws");
        let grants = SandboxGrants {
            enabled: true,
            network: false,
            extra_writable: vec![PathBuf::from("/ws/.loom")],
        };
        let sandbox = Sandbox::resolve(&workspace, grants);
        assert!(
            !sandbox
                .protected()
                .iter()
                .any(|path| path.starts_with("/ws/.loom")),
            "granted writable dir still protected: {:?}",
            sandbox.protected()
        );
        assert!(
            sandbox
                .protected()
                .iter()
                .any(|path| path.ends_with(".git/config")),
            "ungranted control state must stay protected"
        );
    }

    #[test]
    fn seatbelt_network_grant_omits_the_deny() {
        let profile = seatbelt_profile(&[PathBuf::from("/ws")], &[], &[], true);
        assert!(!profile.contains("(deny network*)"));
        // The Apple-event deny is unconditional: there is no grant
        // axis for IPC to other applications.
        assert!(profile.contains("(deny appleevent-send)"));
    }

    /// Runs `program args...` under the real Seatbelt wrapper with the
    /// shipped profile, in a scratch workspace (integration check;
    /// macOS only).
    #[cfg(target_os = "macos")]
    fn run_under_seatbelt(program: &str, args: &[&str]) -> std::process::Output {
        let workspace = tempfile::tempdir().expect("tempdir");
        let sandbox = Sandbox::resolve(workspace.path(), SandboxGrants::from_rule(None));
        let profile = seatbelt_profile(
            &sandbox.writable(),
            &sandbox.protected(),
            &sandbox.home_secrets(),
            false,
        );
        std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("--")
            .arg(program)
            .args(args)
            .output()
            .expect("spawn under sandbox-exec")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_apple_events() {
        // Review item 2.4: a model-reachable child must not be able
        // to script other applications. The exec deny is the
        // operative layer on current macOS (see the profile docs).
        let output = run_under_seatbelt(
            "/usr/bin/osascript",
            &["-e", "tell application \"Finder\" to get name"],
        );
        assert!(
            !output.status.success(),
            "osascript automation escaped the sandbox: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_still_runs_real_toolchains() {
        // The added IPC/exec denies must not break the programs the
        // profile exists for. Probes are self-contained binaries:
        // /usr/bin/git and /usr/bin/python3 are xcode-select *shims*
        // on some machines and the shims write /dev/null, which the
        // pre-existing (deny file-write*) has always refused — a
        // machine artifact, not this profile's regression. Where the
        // real CLT toolchain exists, exercise it directly.
        let mut probes: Vec<(String, Vec<&str>)> = vec![
            ("/usr/bin/sw_vers".to_owned(), vec![]),
            ("/bin/ls".to_owned(), vec!["/usr/bin"]),
        ];
        let clt_git = "/Library/Developer/CommandLineTools/usr/bin/git";
        if std::path::Path::new(clt_git).exists() {
            probes.push((clt_git.to_owned(), vec!["--version"]));
        }
        for (program, args) in probes {
            let output = run_under_seatbelt(&program, &args);
            assert!(
                output.status.success(),
                "{program} no longer runs under the profile: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// The sandbox-escape regression from the adversarial review
    /// (F-02/F-03/F-04), inverted: a sandboxed `sh` must not be able
    /// to plant policy manifests or git hook/config state, while the
    /// ordinary git workflow (status/add — index and object writes)
    /// keeps working. macOS integration check.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_control_state_writes_but_not_git() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let root = workspace.path();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(root)
                .status()
                .expect("git init")
                .success(),
            "fixture needs git"
        );
        std::fs::write(root.join("tracked.txt"), "content\n").expect("fixture file");
        std::fs::create_dir_all(root.join(".loom")).expect("loom dir");

        let sandbox = Sandbox::resolve(root, SandboxGrants::from_rule(None));
        let profile = seatbelt_profile(
            &sandbox.writable(),
            &sandbox.protected(),
            &sandbox.home_secrets(),
            false,
        );
        let run = |script: &str| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("--")
                .arg("/bin/sh")
                .arg("-c")
                .arg(script)
                .current_dir(root)
                .output()
                .expect("spawn under sandbox-exec")
        };

        for target in [".loom/verify.toml", ".git/hooks/pre-commit", ".git/config"] {
            let output = run(&format!("echo pwned > {target}"));
            assert!(
                !output.status.success(),
                "sandboxed write to {target} succeeded: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            !root.join(".loom/verify.toml").exists(),
            ".loom/verify.toml must not be creatable from inside the sandbox"
        );
        assert!(
            !root.join(".git/hooks/pre-commit").exists(),
            "hooks must not be plantable from inside the sandbox"
        );
        // The on-disk config byte-for-byte unchanged (the write must
        // fail, not truncate-then-fail).
        assert!(
            std::fs::read_to_string(root.join(".git/config"))
                .expect("git config")
                .contains("[core]"),
            "git config was damaged by the blocked write"
        );

        // The carve-out must not break ordinary git: `status` refreshes
        // the index, `add` writes the index and the object database.
        for git_args in [["status", "--porcelain"], ["add", "-A"]] {
            let output = run(&format!("git {}", git_args.join(" ")));
            assert!(
                output.status.success(),
                "git {} no longer runs under the profile: {:?}",
                git_args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// F-08 regression: `open -g -j ./P.app` reached LaunchServices
    /// through `(allow default)` and started the bundle *outside* the
    /// sandbox (verified in the review with a real bundle). The
    /// blanket mach deny must seal it: the launch fails and the
    /// bundle's executable never runs. macOS integration check.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_launchservices_app_launch() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let root = workspace.path();
        let app = root.join("P.app/Contents/MacOS");
        std::fs::create_dir_all(&app).expect("bundle dir");
        std::fs::write(
            app.join("P"),
            format!("#!/bin/sh\ntouch {}\n", root.join("probe").display()),
        )
        .expect("executable");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(app.join("P"), std::fs::Permissions::from_mode(0o755))
                .expect("exec bit");
        }
        std::fs::write(
            root.join("P.app/Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>P</string>\
             <key>CFBundleIdentifier</key><string>com.loomtest.P</string>\
             </dict></plist>\n",
        )
        .expect("Info.plist");

        let sandbox = Sandbox::resolve(root, SandboxGrants::from_rule(None));
        let profile = seatbelt_profile(
            &sandbox.writable(),
            &sandbox.protected(),
            &sandbox.home_secrets(),
            false,
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("--")
            .arg("/usr/bin/open")
            .arg("-g")
            .arg("-j")
            .arg(root.join("P.app"))
            .output()
            .expect("spawn under sandbox-exec");
        // LaunchServices is unreachable, so `open` itself fails.
        assert!(
            !output.status.success(),
            "open should not reach LaunchServices: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The decisive assertion: even if a future macOS reports
        // success, the bundle must never have run.
        std::thread::sleep(std::time::Duration::from_millis(2_000));
        assert!(
            !root.join("probe").exists(),
            "P.app launched outside the sandbox via LaunchServices"
        );
    }

    /// F-11 regression: home credential material must be unreadable
    /// from inside the sandbox. Skips (loudly) on hosts without
    /// `~/.ssh` so it never reads the operator's real keys to pass.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_home_secret_reads() {
        let home = std::env::var_os("HOME").expect("HOME set");
        let ssh = std::path::Path::new(&home).join(".ssh");
        if !ssh.exists() {
            eprintln!("no ~/.ssh on this host; skipping");
            return;
        }
        let output = run_under_seatbelt("/bin/ls", &[&ssh.to_string_lossy()]);
        assert!(
            !output.status.success(),
            "~/.ssh must be unreadable from inside the sandbox"
        );
    }

    #[test]
    fn seatbelt_escapes_path_metacharacters() {
        let profile = seatbelt_profile(&[PathBuf::from("/ws/na\"me\\dir")], &[], &[], false);
        assert!(profile.contains("/ws/na\\\"me\\\\dir"));
    }

    #[test]
    fn seatbelt_denies_launchservices_and_home_secrets() {
        // F-08: the blanket mach deny is the LaunchServices seal; F-11:
        // home credential material is denied read even under
        // `(allow default)`.
        let workspace = PathBuf::from("/ws");
        let sandbox = Sandbox::resolve(&workspace, SandboxGrants::from_rule(None));
        let profile = seatbelt_profile(
            &[workspace],
            &sandbox.protected(),
            &sandbox.home_secrets(),
            false,
        );
        assert!(profile.contains("(deny mach-lookup)"));
        for program in ["/usr/bin/open", "/usr/bin/automator", "/usr/bin/shortcuts"] {
            assert!(
                profile.contains(&format!("(subpath \"{program}\")")),
                "{program} exec deny missing from:\n{profile}"
            );
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            for secret in [
                ".ssh",
                ".gnupg",
                ".aws",
                ".kube",
                ".config/loom",
                ".local/share/loom",
            ] {
                assert!(
                    profile.contains(&format!(
                        "(subpath \"{}\")",
                        escape_seatbelt_path(&home.join(secret))
                    )),
                    "{secret} missing from the home-secrets deny in:\n{profile}"
                );
            }
        }
    }

    #[test]
    fn grants_override_the_home_secrets_defaults() {
        // The operator's escalation axis applies to secrets the same
        // way it applies to control state: an explicitly granted
        // writable dir (e.g. for sandboxed `ssh`) is not denied read.
        let workspace = PathBuf::from("/ws");
        let ssh = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".ssh"))
            .expect("HOME set");
        let grants = SandboxGrants {
            enabled: true,
            network: false,
            extra_writable: vec![ssh.clone()],
        };
        let sandbox = Sandbox::resolve(&workspace, grants);
        assert!(
            !sandbox.home_secrets().contains(&ssh),
            "granted ~/.ssh still denied: {:?}",
            sandbox.home_secrets()
        );
    }

    #[test]
    #[cfg(unix)]
    fn writable_entries_are_canonicalized() {
        // F-01: a grant spelled through a symlink must land in the
        // writable set as its real path, or the Seatbelt subpath allow
        // and the bwrap bind never match the child's syscalls.
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(base.path().join("real")).expect("real dir");
        std::os::unix::fs::symlink(base.path().join("real"), base.path().join("link"))
            .expect("symlink");
        let granted = base.path().join("link/pending/dir");
        let sandbox = Sandbox::resolve(
            base.path(),
            SandboxGrants {
                enabled: true,
                network: false,
                extra_writable: vec![granted],
            },
        );
        let expected = base.path().join("real").canonicalize().expect("canonical");
        let expected = expected.join("pending/dir");
        assert!(
            sandbox.writable().contains(&expected),
            "canonical grant path missing from {:?}",
            sandbox.writable()
        );
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
        let sandbox = Sandbox::resolve(Path::new("/ws"), SandboxGrants::from_rule(None));
        bubblewrap_argv(
            "cargo",
            &["build".to_owned(), "--release".to_owned()],
            &BubblewrapWritable {
                workspace: Path::new("/ws"),
                optional: &optional,
                temp: Path::new("/tmp"),
            },
            &sandbox.protected(),
            &sandbox.home_secrets(),
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
    fn bubblewrap_binds_control_state_read_only_after_every_writable_bind() {
        // Later mounts shadow earlier ones, so the control-state
        // `--ro-bind-try`s must come after the workspace `--bind` (and
        // after every optional writable bind) to carve `.loom`,
        // `.cargo`, and git's config/hooks back out of the writable
        // workspace.
        let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
        let read_only = bind_pairs(&argv, "--ro-bind-try");
        for relative in [".loom", ".cargo", ".git/config", ".git/hooks"] {
            let path = format!("/ws/{relative}");
            assert!(
                read_only
                    .iter()
                    .any(|[src, dest]| src == &path && dest == &path),
                "{path} not bound read-only in {argv:?}"
            );
        }
        // Every read-only control-state bind lands after the last
        // read-write bind of any kind.
        let last_rw = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == "--bind" || arg.as_str() == "--bind-try")
            .map(|(index, _)| index)
            .next_back()
            .unwrap_or_else(|| panic!("no writable binds in {argv:?}"));
        for (index, arg) in argv.iter().enumerate() {
            if arg == "--ro-bind-try" && argv[index + 1].starts_with("/ws/.") {
                assert!(
                    index > last_rw,
                    "control-state bind before a writable bind in {argv:?}"
                );
            }
        }
    }

    #[test]
    fn bubblewrap_masks_home_secrets_with_an_empty_tmpfs() {
        // F-11 parity on Linux: the home dir is read-only-mounted
        // whole, so `~/.ssh` et al. need their own tmpfs shadow to
        // read empty instead of credential material. `home_secrets()`
        // derives from the process HOME, so assert against that.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME set");
        let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
        let tmpfs: Vec<String> = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == "--tmpfs")
            .map(|(index, _)| argv[index + 1].clone())
            .collect();
        for secret in [
            home.join(".ssh"),
            home.join(".gnupg"),
            home.join(".aws"),
            home.join(".kube"),
            home.join(".config/loom"),
            home.join(".local/share/loom"),
        ] {
            assert!(
                tmpfs.contains(&secret.to_string_lossy().into_owned()),
                "{} not masked by a tmpfs in {argv:?}",
                secret.display()
            );
        }
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

    /// The probe must be a no-op where the wrapper ships with the OS
    /// (macOS) or the sandbox passes through — `loom run` calls it at
    /// setup on every platform.
    #[test]
    fn preflight_is_ok_where_no_wrapper_is_needed() {
        #[cfg(not(target_os = "linux"))]
        assert_eq!(preflight(), Ok(()));
        // On Linux the probe must at least resolve to a real verdict;
        // run it only when a wrapper exists so a dev box without bwrap
        // tests the classification below instead of failing here.
        #[cfg(target_os = "linux")]
        if find_bwrap().is_some() {
            assert_eq!(preflight(), Ok(()), "bwrap present but probe failed");
        }
    }

    /// Ubuntu 24.04+'s AppArmor denial, verbatim in shape: the wrapper
    /// dies at uid-map setup and the command never runs.
    #[test]
    fn remediation_maps_the_apparmor_userns_denial() {
        let stderr = "bwrap: setting up uid map: Permission denied\n";
        let hint = bwrap_remediation(stderr).expect("uid-map denial must be annotated");
        assert!(hint.contains("user namespaces"), "{hint}");
        assert!(
            hint.contains("apparmor_restrict_unprivileged_userns"),
            "{hint}"
        );
        assert!(hint.contains(".loom/commands.toml"), "{hint}");
    }

    #[test]
    fn remediation_maps_container_seccomp_denials() {
        let stderr = "bwrap: Failed to clone: Operation not permitted\nseccomp\n";
        let hint = bwrap_remediation(stderr).expect("seccomp denial must be annotated");
        assert!(hint.contains("seccomp=unconfined"), "{hint}");
    }

    /// A bare wrapper failure still gets the generic pointer to the
    /// distro matrix rather than nothing.
    #[test]
    fn remediation_covers_unknown_bwrap_failures() {
        let hint = bwrap_remediation("bwrap: mount /proc: No such file or directory\n")
            .expect("bwrap-prefixed failure must be annotated");
        assert!(hint.contains("docs/linux-sandbox.md"), "{hint}");
    }

    /// Ordinary command failures are never annotated: no `bwrap:`
    /// prefix, no wrapper involvement.
    #[test]
    fn remediation_leaves_ordinary_failures_alone() {
        assert_eq!(
            bwrap_remediation("error: no such command `buld`\nPermission denied\n"),
            None
        );
        // A command that merely *mentions* bubblewrap in its own output.
        assert_eq!(
            bwrap_remediation("warning: bwrap not used in this configuration\n"),
            None
        );
    }
}
