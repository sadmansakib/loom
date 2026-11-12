#[cfg(target_os = "macos")]
use super::Sandbox;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
impl Sandbox {
    const WRAPPER: &str = "/usr/bin/sandbox-exec";

    pub(super) fn wrap_seatbelt(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
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

/// Escapes a path for a Seatbelt string literal: backslash and quote
/// are the two characters with meaning there.
pub(super) fn escape_seatbelt_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}
