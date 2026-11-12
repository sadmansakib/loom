//! Workspace path touches and their overlap geometry (ADR-018): the
//! inputs to dependency-aware scheduling, result caching, and pruning.

/// How a call touches a path, for dependency-aware scheduling (ADR-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathAccess {
    /// The call reads the path. Concurrent reads never conflict.
    Read,
    /// The call creates, overwrites, or deletes the path. A write
    /// conflicts with any other access to the same path.
    Write,
}

/// A workspace-relative path a call will touch and how (ADR-018).
///
/// Declared by the tool *before* execution on the raw arguments — never
/// model-declared — and reused by scheduling (V13), result caching (V14)
/// and pruning. Compared as given, so a tool must report the same string
/// the model used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTouch {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// Read or write.
    pub access: PathAccess,
}

impl PathTouch {
    /// A read of `path`.
    #[must_use]
    pub fn read(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: PathAccess::Read,
        }
    }

    /// A write to `path`.
    #[must_use]
    pub fn write(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: PathAccess::Write,
        }
    }
}

/// Whether two workspace-relative paths overlap: identical, or one is an
/// ancestor directory of the other (segment-wise). A read of `src` and a
/// write to `src/a.rs` contend; `src/a.rs` and `src/b.rs` do not. `.` (the
/// workspace root) is an ancestor of everything. Shared by scheduling
/// (V13) and result caching (V14).
///
/// Comparison is on normalized spellings: repeated `/`, leading `./`, and
/// trailing `/` are equivalent (review E2: one touch declaring `./a` and
/// another `a` named the same file but used to run concurrently and miss
/// in the cache, because the raw strings differ).
pub(crate) fn paths_overlap(a: &str, b: &str) -> bool {
    let a = normalize_rel(a);
    let b = normalize_rel(b);
    a == b || is_ancestor(&a, &b) || is_ancestor(&b, &a)
}

/// Collapses empty (`//`) and `.` path segments, so `./a`, `a//`, and
/// `a` all map to `a` and `.` maps to `""` (the root).
fn normalize_rel(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(segment);
    }
    out
}

/// Whether `dir` is a strict ancestor directory of `path`.
pub(crate) fn is_ancestor(dir: &str, path: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        return !path.is_empty() && path != ".";
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}
