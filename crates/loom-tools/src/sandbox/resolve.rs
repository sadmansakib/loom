use std::path::{Path, PathBuf};

use super::SandboxGrants;

/// A resolved sandbox for one command invocation: the writable set is
/// concrete (canonical where possible), the grants fixed.
#[derive(Debug, Clone)]
pub(crate) struct Sandbox {
    pub(super) grants: SandboxGrants,
    /// Workspace root — always writable.
    pub(super) workspace: PathBuf,
    /// Temp directory (`TMPDIR`/`/tmp`) — always writable; builds
    /// scratch here.
    pub(super) temp: PathBuf,
    /// Cargo home (`CARGO_HOME` or `~/.cargo`) when it exists —
    /// writable so registry locks and caches work; without it a plain
    /// `cargo build` cannot run at all.
    pub(super) cargo_home: Option<PathBuf>,
    /// Cargo target dir (`CARGO_TARGET_DIR`) when set — writable so
    /// cargo's own build locks and artifacts resolve under the sandbox
    /// even when the build root lies elsewhere (e.g. a temp-dir crate
    /// that still targets the real repo's scratch).
    pub(super) cargo_target_dir: Option<PathBuf>,
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
    pub(super) fn writable(&self) -> Vec<PathBuf> {
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
    pub(super) fn protected(&self) -> Vec<PathBuf> {
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
    pub(super) fn home_secrets(&self) -> Vec<PathBuf> {
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
