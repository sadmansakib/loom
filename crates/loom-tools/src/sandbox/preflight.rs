#[cfg(target_os = "linux")]
use super::bubblewrap::find_bwrap;

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
