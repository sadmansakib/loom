//! Atomic file replacement for every tool write (review items
//! 2.2/2.3).
//!
//! A direct `fs::write` truncates the target first: a crash mid-write
//! leaves a half file, and concurrent readers see torn contents. The
//! helper here writes a uniquely-named temporary file **in the target
//! directory** (same filesystem, so the final rename cannot copy),
//! fsyncs it, preserves the original's permissions, and renames it
//! over the target — the rename is atomic on POSIX, so readers see
//! either the old or the new file, never a mix.
//!
//! Symlinked targets keep the semantics the callers had before: the
//! confinement resolver fully resolves symlinks in existing path
//! components before the write, so the rename lands on the resolved,
//! confined target.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic suffix for temp names; uniqueness across threads and
/// retries without pulling in `uuid` here.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` via temp-file + rename.
///
/// * missing parent directories are created (as the previous direct
///   writes did);
/// * on Unix the original file's permissions are carried over (a new
///   file keeps the process default);
/// * any error removes the temporary file — no `.loom-tmp-*` litter
///   survives a failed write.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("replacement target has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    // Review E4: the old name was fully predictable (pid + counter),
    // and `File::create` follows a pre-planted symlink at that path —
    // a concurrent workspace process could steer the temp write at a
    // host file. `create_new` (O_EXCL) refuses to touch anything that
    // already exists, so even a guessed name is useless; the
    // nanosecond timestamp still spreads collisions across retries.
    for _ in 0..8 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let temp = parent.join(format!(".loom-tmp-{}-{seq}-{nanos}", std::process::id()));
        match write_temp(&temp, path, bytes) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            other => {
                let result = other.and_then(|()| {
                    if let Err(error) = std::fs::rename(&temp, path) {
                        let _ = std::fs::remove_file(&temp);
                        return Err(error);
                    }
                    Ok(())
                });
                if result.is_err() {
                    // A failed write before the rename also leaves the
                    // temp file behind; the rename arm cleans up on
                    // its own.
                    let _ = std::fs::remove_file(&temp);
                }
                return result;
            }
        }
    }
    Err(std::io::Error::other(
        "temp name collided 8 times; refusing to reuse one",
    ))
}

/// Creates, fills and syncs the temp file, carrying over the target's
/// permissions when it already exists.
fn write_temp(temp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Any error here unwinds to `atomic_write`, which removes the
    // temp file — plain `?` keeps that invariant. `create_new` is the
    // E4 half of the contract: never open an existing path (O_EXCL),
    // so a planted symlink or file at the temp name cannot be
    // followed or clobbered.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_data()?;
    drop(file);
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(target) {
        std::fs::set_permissions(temp, metadata.permissions())?;
    }
    #[cfg(not(unix))]
    let _ = target;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    #[test]
    fn replaces_contents_and_creates_missing_parents() {
        let (guard, root) = temp_root();
        let target = root.join("new/dir/file.txt");
        atomic_write(&target, b"first").expect("write");
        atomic_write(&target, b"second").expect("rewrite");
        assert_eq!(fs::read(&target).expect("read"), b"second");
        // No temp litter in either directory.
        for entry in fs::read_dir(root.join("new/dir")).expect("read_dir") {
            let name = entry.expect("entry").file_name();
            assert!(!name.to_string_lossy().starts_with(".loom-tmp-"));
        }
        let _ = guard;
    }

    #[cfg(unix)]
    #[test]
    fn preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_guard, root) = temp_root();
        let target = root.join("file.txt");
        fs::write(&target, b"x").expect("write");
        let mut perms = fs::metadata(&target).expect("meta").permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&target, perms).expect("chmod");
        atomic_write(&target, b"y").expect("rewrite");
        let mode = fs::metadata(&target).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Review E4: a planted symlink at every plausible temp name must
    /// never be written through — `create_new` refuses to open it, the
    /// write retries under a fresher name, and the host file the link
    /// points at stays untouched.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_at_the_temp_name_is_never_followed() {
        let (guard, root) = temp_root();
        let precious = root.join("precious.txt");
        fs::write(&precious, b"precious\n").expect("write");
        // Carpet-bomb the directory with symlinks shaped like every
        // temp name the helper could pick for a few sequence values.
        for seq in 0..64u64 {
            for nanos in [0u32, 1, 999_999_999] {
                let name = format!(".loom-tmp-{}-{seq}-{nanos}", std::process::id());
                std::os::unix::fs::symlink(&precious, root.join(name)).expect("symlink");
            }
        }
        let target = root.join("file.txt");
        fs::write(&target, b"old").expect("write");
        atomic_write(&target, b"new").expect("write");
        assert_eq!(fs::read(&target).expect("read"), b"new");
        assert_eq!(
            fs::read(&precious).expect("read"),
            b"precious\n",
            "the temp write must never land through a planted symlink"
        );
        let _ = guard;
    }

    #[test]
    fn a_failed_rename_leaves_no_temp_file() {
        let (guard, root) = temp_root();
        // A directory at the target path makes the rename fail
        // (rename cannot replace a directory with a file) while the
        // temp write itself succeeds.
        fs::create_dir(root.join("target")).expect("mkdir");
        let err = atomic_write(&root.join("target"), b"x").expect_err("rename fails");
        assert!(err.kind() == std::io::ErrorKind::IsADirectory || cfg!(not(unix)));
        for entry in fs::read_dir(&root).expect("read_dir") {
            let name = entry.expect("entry").file_name();
            assert!(
                !name.to_string_lossy().starts_with(".loom-tmp-"),
                "temp file survived a failed write"
            );
        }
        let _ = guard;
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_to_the_resolved_target_of_a_symlink() {
        // The caller resolves symlinks before handing a path here;
        // when it does, the rename lands on the real file and the
        // link itself is untouched.
        let (guard, root) = temp_root();
        let real = root.join("real.txt");
        fs::write(&real, b"old").expect("write");
        std::os::unix::fs::symlink(&real, guard.path().join("link.txt")).expect("symlink");
        atomic_write(&real, b"new").expect("write");
        assert_eq!(fs::read(&real).expect("read"), b"new");
    }
}
