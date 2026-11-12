//! Process-level helpers: `PATH` resolution for bare program names,
//! executability checks, and the exit-status line.

/// Resolves a bare program name against `PATH`, mirroring the search the
/// OS would perform — used to fail fast (and legibly) before the program
/// enters the sandbox wrapper, which cannot report a missing binary.
///
/// A candidate must be a regular file the user may execute, which is what
/// the kernel itself requires. Without the check a non-executable file
/// sitting early on `PATH` passes here, shadows a later entry that would
/// have worked, and the failure then surfaces from the spawn as a
/// permissions error rather than the legible "not found on PATH".
pub(crate) fn resolve_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `candidate` is a regular file with an execute bit, the way
/// the kernel decides when exec'ing a `PATH` entry.
#[cfg(unix)]
pub(super) fn is_executable_file(candidate: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Windows has no execute bit; a regular file is the best available
/// approximation.
#[cfg(not(unix))]
pub(super) fn is_executable_file(candidate: &std::path::Path) -> bool {
    candidate.is_file()
}

/// `exit status: N`, or a signal note when the platform reports none.
pub(super) fn status_line(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status: {code}"),
        None => String::from("exit status: terminated by signal"),
    }
}
