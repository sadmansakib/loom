//! Workspace path confinement (architecture.md §9.1).
//!
//! Every filesystem tool resolves user-supplied paths through
//! [`resolve_within`]: the result is either a path that provably stays
//! inside the workspace root (after symlink resolution) or an
//! [`ToolError::InvalidInput`] naming the escape. This is an execution
//! invariant, not policy — gitignore and friends never play a part
//! here.

use std::path::{Component, Path, PathBuf};

use loom_kernel::tool::ToolError;

/// Resolves `user` against `root`, rejecting anything that escapes
/// the workspace.
///
/// Resolution is **component-wise** (one segment at a time, so no
/// textual `..` collapse can smuggle a path through a symlink):
///
/// * relative paths start at the canonical root; absolute paths are
///   resolved from the filesystem root and must land inside `root`;
/// * while components exist, each is lstat-checked: a symlink is
///   fully canonicalised (targets *inside* the root are allowed,
///   targets outside are escapes — boundary = real target), a real
///   component is appended as-is;
/// * past the first missing component, the remainder is appended
///   lexically — nothing after a missing directory can exist, so no
///   symlink there can ever be followed by the OS. A `..` that pops
///   back onto an *existing* path re-canonicalises it and re-enters
///   the verified regime (it never trusts a lexically-built path as
///   canonical);
/// * a **dangling** symlink is resolved by running this same
///   component-wise walk on its target (bounded by
///   [`SYMLINK_LIMIT`]), so a `..` inside a target and nested
///   dangling links obey real resolution rules instead of textual
///   normalisation — a broken link pointing outside is an escape,
///   not a "missing path inside the root";
/// * the final path must start with the canonical root.
pub(crate) fn resolve_within(root: &Path, user: &str) -> Result<PathBuf, ToolError> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // A root that cannot be canonicalized (empty path, unreadable dir,
    // already-deleted workspace) is not a usable confinement base: the
    // lexical fallback would make `starts_with` accept everything.
    if !canonical_root.is_absolute() {
        return Err(ToolError::InvalidInput(format!(
            "workspace root {:?} cannot be resolved to an absolute path",
            root.display()
        )));
    }
    walk(&canonical_root, &canonical_root, Path::new(user), 0)
}

/// The workspace-relative, `/`-separated form of a path that came from
/// [`resolve_within`], for tool effect records (ADR-012).
///
/// Effect paths are stored relative so a record means the same thing on
/// any machine and never leaks an absolute home directory. If `path`
/// does not sit under `root` (defensive — `resolve_within` guarantees
/// it does) the path is rendered as-is.
pub(crate) fn workspace_relative(root: &Path, path: &Path) -> String {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let relative = path.strip_prefix(&canonical_root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Maximum nested **dangling**-link resolution depth before the walk
/// fails loudly. The kernel reports ELOOP past 40 symlink levels;
/// dangling targets are resolved recursively here, so they need
/// their own bound.
const SYMLINK_LIMIT: usize = 8;

/// One component-wise pass over `user`. `base` must be canonical;
/// `depth` counts dangling-link recursions.
fn walk(
    base: &Path,
    canonical_root: &Path,
    user: &Path,
    depth: usize,
) -> Result<PathBuf, ToolError> {
    let escape = || {
        ToolError::InvalidInput(format!(
            "path \"{}\" escapes the workspace root",
            user.display()
        ))
    };
    let mut current = base.to_path_buf();
    // While `false`, `current` is fully resolved (no symlinks left).
    // Once `true`, nothing past the break point exists on disk and
    // the walk stays lexical until a `..` re-canonicalises its way
    // back onto an existing path.
    let mut missing = false;

    for component in user.components() {
        match component {
            Component::CurDir => {}
            Component::Prefix(prefix) => {
                // Windows drive/UNC prefix (e.g. `C:`). `RootDir` below
                // appends rather than replaces so the drive survives.
                current = PathBuf::from(prefix.as_os_str());
                missing = false;
            }
            Component::RootDir => {
                // On Unix this starts the absolute path from `/`. On
                // Windows `C:\x` arrives as Prefix(`C:`) then RootDir:
                // `push` keeps `C:/` instead of discarding the drive
                // (a bare `=` would resolve absolute Windows paths
                // against `/` and wrongly reject them as escapes).
                current.push(Path::new("/"));
                missing = false;
            }
            Component::ParentDir => {
                // Popping at the filesystem root is a no-op. Landing
                // on an existing path re-enters verified territory —
                // canonicalise instead of trusting `current`, which
                // may carry lexically-resolved dangling-link
                // components.
                current.pop();
                if let Ok(real) = current.canonicalize() {
                    current = real;
                    missing = false;
                }
            }
            Component::Normal(name) => {
                missing = step_normal(&mut current, missing, name, canonical_root, user, depth)?;
            }
        }
    }

    if current.starts_with(canonical_root) {
        Ok(current)
    } else {
        Err(escape())
    }
}

/// Advances the walk by one normal component, lstat-checking it:
/// symlinks are followed (existing targets become the new `current`;
/// dangling targets are resolved with the same component-wise walk,
/// bounded by [`SYMLINK_LIMIT`]), real components are appended as-is,
/// and a missing component flips the walk into its lexical regime.
///
/// Returns the component's `missing` verdict; `current` is updated in
/// place.
fn step_normal(
    current: &mut PathBuf,
    missing: bool,
    name: &std::ffi::OsStr,
    canonical_root: &Path,
    user: &Path,
    depth: usize,
) -> Result<bool, ToolError> {
    if missing {
        current.push(name);
        return Ok(true);
    }
    let next = current.join(name);
    match std::fs::symlink_metadata(&next) {
        Ok(meta) if meta.file_type().is_symlink() => match next.canonicalize() {
            Ok(resolved) => {
                *current = resolved;
                Ok(false)
            }
            Err(_) => {
                // Dangling link: the lstat succeeded but the target
                // is gone. Resolve the target with the same
                // component-wise walk, so `..` and nested links
                // inside the target obey real resolution rules.
                if depth >= SYMLINK_LIMIT {
                    return Err(ToolError::InvalidInput(format!(
                        "path \"{}\" exceeds {SYMLINK_LIMIT} levels of \
                         symbolic links",
                        user.display()
                    )));
                }
                let target = std::fs::read_link(&next).map_err(|err| {
                    ToolError::InvalidInput(format!(
                        "cannot resolve symlink in path \"{}\": {err}",
                        user.display()
                    ))
                })?;
                *current = walk(current, canonical_root, &target, depth + 1)?;
                // The target may resolve back to an existing
                // directory (e.g. `x/..` lands on the root). Only
                // stay lexical when the resolved path is really
                // missing — hardcoding `true` let a later symlink
                // skip its lstat check entirely.
                Ok(!current.try_exists().unwrap_or(false))
            }
        },
        // A real (non-symlink) existing component: still canonical,
        // since `current` was.
        Ok(_) => {
            *current = next;
            Ok(false)
        }
        Err(_) => {
            *current = next;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    #[test]
    fn relative_path_inside_resolves() {
        let (_guard, root) = temp_root();
        let resolved = resolve_within(&root, "src/main.rs").expect("inside");
        assert!(resolved.starts_with(&root));
        assert!(resolved.ends_with("src/main.rs"));
    }

    #[test]
    fn absolute_path_inside_resolves() {
        let (_guard, root) = temp_root();
        let abs = root.join("a.txt");
        let resolved = resolve_within(&root, abs.to_str().expect("utf8")).expect("inside");
        assert_eq!(resolved, abs);
    }

    #[test]
    fn absolute_path_outside_rejected() {
        let (_guard, root) = temp_root();
        let err = resolve_within(&root, "/etc/passwd").expect_err("outside");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn parent_dot_dot_escape_rejected() {
        let (_guard, root) = temp_root();
        let err = resolve_within(&root, "../../etc/passwd").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dot_dot_staying_inside_resolves() {
        let (_guard, root) = temp_root();
        let resolved = resolve_within(&root, "a/../b.txt").expect("inside");
        assert!(resolved.ends_with("b.txt"));
        assert!(!resolved.to_string_lossy().contains(".."));
    }

    #[test]
    fn symlink_inside_root_allowed() {
        let (guard, root) = temp_root();
        let target_dir = root.join("real");
        fs::create_dir(&target_dir).expect("mkdir");
        // The final component must exist: canonicalisation (and with
        // it symlink resolution) applies to existing paths only.
        fs::write(target_dir.join("x.txt"), b"x").expect("write");
        symlink(&target_dir, guard.path().join("link")).expect("symlink");
        let resolved = resolve_within(&root, "link/x.txt").expect("inside");
        assert!(resolved.starts_with(&root));
        assert!(resolved.starts_with(&target_dir));
    }

    #[test]
    fn symlink_escaping_root_rejected() {
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        fs::write(outside.path().join("x.txt"), b"x").expect("write");
        symlink(outside.path(), guard.path().join("escape")).expect("symlink");
        let err = resolve_within(&root, "escape/x.txt").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn missing_path_inside_root_resolves_lexically() {
        let (_guard, root) = temp_root();
        let resolved = resolve_within(&root, "no/such/file.txt").expect("inside");
        assert_eq!(resolved, root.join("no/such/file.txt"));
    }

    #[test]
    fn missing_path_with_dot_dot_escape_rejected() {
        let (_guard, root) = temp_root();
        let err = resolve_within(&root, "no/../../outside.txt").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn symlink_escape_with_missing_final_rejected() {
        // The symlink target exists but the final file does not: the
        // component walk must still catch the escape.
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        fs::write(outside.path().join("existing.txt"), b"x").expect("write");
        symlink(outside.path(), guard.path().join("escape")).expect("symlink");
        let err = resolve_within(&root, "escape/new.txt").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dot_dot_after_missing_through_symlink_rejected() {
        // High-severity regression: `link` points outside, `missing`
        // does not exist, and `..` walks back out of it. A purely
        // textual collapse of `missing/..` would land on `link/x`,
        // which the OS resolves *through* the symlink.
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        fs::write(outside.path().join("x"), b"secret").expect("write");
        symlink(outside.path(), guard.path().join("link")).expect("symlink");
        let err = resolve_within(&root, "link/missing/../x").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dangling_symlink_pointing_outside_rejected() {
        // lstat succeeds on a dangling link but the target is gone:
        // read_link still names where it would lead, and that is
        // outside the root. Relevant before write tools land.
        let (guard, root) = temp_root();
        symlink("/nonexistent-outside", guard.path().join("dangling")).expect("symlink");
        let err = resolve_within(&root, "dangling/new.txt").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dangling_symlink_pointing_inside_resolves() {
        let (guard, root) = temp_root();
        symlink("gone", guard.path().join("dangling")).expect("symlink");
        let resolved = resolve_within(&root, "dangling/new.txt").expect("inside");
        assert_eq!(resolved, root.join("gone").join("new.txt"));
    }

    #[test]
    fn trailing_slash_introduces_no_separator() {
        let (_guard, root) = temp_root();
        let resolved = resolve_within(&root, "a.txt/").expect("inside");
        assert_eq!(resolved, root.join("a.txt"));
    }

    #[test]
    fn dangling_link_dot_dot_escape_rejected() {
        // Round-3 probe: the dangling target resolves through a
        // symlink (`inlink`) nobody followed, and `..` pops back onto
        // an existing path. Without re-canonicalisation the walk used
        // to return `root/inlink/secret`, which the OS resolves
        // *through* `inlink` to a file outside the root.
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        fs::write(outside.path().join("secret"), b"s").expect("write");
        symlink(outside.path(), guard.path().join("inlink")).expect("symlink inlink");
        symlink("inlink/gone", guard.path().join("dangling")).expect("symlink dangling");
        let err = resolve_within(&root, "dangling/../secret").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn broken_link_to_existing_dir_keeps_symlink_checks() {
        // Round-4 probe: `d -> "x/.."` is a broken link whose target
        // resolves back to the *existing* root. Hardcoding
        // `missing = true` after the recursive walk let the later
        // `link2` symlink bypass its lstat check and be appended as
        // plain text; the OS then followed it on open. The kernel
        // itself would ENOENT on `d/link2/secret`.
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        fs::write(outside.path().join("secret"), b"SECRET\n").expect("write");
        symlink("x/..", guard.path().join("d")).expect("symlink d");
        symlink(outside.path(), guard.path().join("link2")).expect("symlink link2");
        let err = resolve_within(&root, "d/link2/secret").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dangling_target_dot_dot_through_symlink_rejected() {
        // The dangling link's target itself contains `..` across a
        // symlink: textual normalisation would collapse it in-root,
        // the kernel would resolve it outside.
        let (guard, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        symlink(outside.path(), guard.path().join("inlink")).expect("symlink inlink");
        symlink("inlink/../zz_no_such_root", guard.path().join("d2")).expect("symlink d2");
        let err = resolve_within(&root, "d2/new.txt").expect_err("escape");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("escapes")));
    }

    #[test]
    fn dangling_symlink_cycle_errors_loudly() {
        // Two dangling links pointing at each other: the recursive
        // target resolution must hit the depth limit instead of
        // recursing forever.
        let (guard, root) = temp_root();
        symlink("d2/x", guard.path().join("d1")).expect("symlink d1");
        symlink("d1/y", guard.path().join("d2")).expect("symlink d2");
        let err = resolve_within(&root, "d1/f.txt").expect_err("cycle");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("symbolic links")));
    }

    #[test]
    fn empty_workspace_root_is_rejected() {
        // An empty root cannot be canonicalized and must not silently
        // fall back to a lexical base that `starts_with` accepts
        // everything against.
        let err = resolve_within(Path::new(""), "any/path").expect_err("empty root");
        assert!(matches!(err, ToolError::InvalidInput(msg) if msg.contains("workspace root")));
    }
}
