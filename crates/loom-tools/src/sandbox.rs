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
        // A nonexistent cargo home is left in the set: creating it is a
        // legitimate write (a container's first `cargo` run), and every
        // sandbox treats a missing directory as creatable-or-ignorable.
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
        let mut wrapped = vec!["--die-with-parent".to_owned(), "--new-session".to_owned()];
        if !self.grants.network {
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
        if let Some(home) = std::env::var_os("HOME") {
            let home = home.to_string_lossy().into_owned();
            wrapped.extend(
                ["--ro-bind-try", &home, &home]
                    .iter()
                    .map(|s| (*s).to_owned()),
            );
        }
        for dir in self.writable() {
            if dir == self.temp {
                continue; // /tmp is a tmpfs; the TMPDIR path gets its own bind.
            }
            let dir = dir.to_string_lossy().into_owned();
            wrapped.extend(["--bind", &dir, &dir].iter().map(|s| (*s).to_owned()));
        }
        let temp = self.temp.to_string_lossy().into_owned();
        wrapped.extend(["--bind", &temp, &temp].iter().map(|s| (*s).to_owned()));
        let workspace = self.workspace.to_string_lossy().into_owned();
        wrapped.extend(["--chdir", &workspace].iter().map(|s| (*s).to_owned()));
        wrapped.push(program.to_owned());
        wrapped.extend(args.iter().cloned());
        (wrapper, wrapped)
    }
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
}
