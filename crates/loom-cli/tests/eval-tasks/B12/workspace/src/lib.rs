//! Effect-based wave scheduling for a batch of tool calls.
//!
//! ## Contract
//!
//! [`Access`] is what a call may touch:
//!
//! * [`Access::Known`] lists workspace paths the call reads or writes,
//!   exactly as declared. An empty list touches nothing.
//! * [`Access::Unknown`] is a wildcard: the call may touch any path.
//!
//! [`conflicts`] decides whether two calls in the same batch conflict —
//! one writes a path the other touches:
//!
//! * a write conflicts with a read or a write of the same path;
//! * two reads never conflict;
//! * [`Access::Unknown`] conflicts with every read and write;
//! * paths overlap when equal or when one is a strict ancestor
//!   directory of the other (a write under `src` conflicts with a read
//!   of the directory `src`).
//!
//! [`schedule`] assigns each call to a wave: within a wave every pair of
//! calls is conflict-free, and a conflict always places the later call
//! in a strictly later wave, so model order is preserved on shared
//! paths. Waves run in order; calls inside a wave run concurrently, so
//! only real effects — not call count — serialize a batch.

/// What a call may touch: declared paths, or anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    Unknown,
    Known(Vec<String>),
}

/// The declared effects of one call.
#[derive(Clone, Debug)]
pub struct CallAccess {
    pub reads: Access,
    pub writes: Access,
}

/// Whether two calls conflict: one writes a path the other touches.
pub fn conflicts(a: &CallAccess, b: &CallAccess) -> bool {
    let has_write = |access: &Access| match access {
        Access::Unknown => true,
        Access::Known(paths) => !paths.is_empty(),
    };
    has_write(&a.writes) || has_write(&b.writes)
}

/// Whether two workspace paths overlap: equal, or one is a strict
/// ancestor directory of the other (segment-wise).
pub fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || is_ancestor(a, b) || is_ancestor(b, a)
}

/// Whether `dir` is a strict ancestor directory of `path`.
fn is_ancestor(dir: &str, path: &str) -> bool {
    let mut segments = path.split('/');
    let mut dir_segments = dir.split('/');
    loop {
        match (dir_segments.next(), segments.next()) {
            (Some(d), Some(p)) if d == p => continue,
            (None, Some(_)) => return true,
            _ => return false,
        }
    }
}

/// Assigns each call to a wave: the longest path over conflict edges.
/// Two calls share a wave only when conflict-free, and a conflict
/// always puts the later call in a strictly later wave. Indices inside
/// a wave are ascending.
pub fn schedule(calls: &[CallAccess]) -> Vec<Vec<usize>> {
    let mut level = vec![0usize; calls.len()];
    for later in 0..calls.len() {
        for earlier in 0..later {
            if conflicts(&calls[earlier], &calls[later]) {
                level[later] = level[later].max(level[earlier] + 1);
            }
        }
    }
    let mut waves: Vec<Vec<usize>> = Vec::new();
    for (index, &wave) in level.iter().enumerate() {
        if waves.len() <= wave {
            waves.resize(wave + 1, Vec::new());
        }
        waves[wave].push(index);
    }
    waves
}
