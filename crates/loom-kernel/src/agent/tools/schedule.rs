//! Wave scheduling for [`super::ToolBatch`] (§10, ADR-018): each call's
//! touched paths are derived from the tool engine (never model
//! declarations), and calls are assigned to conflict-free waves that
//! preserve model order on shared paths.

use std::collections::HashSet;

use crate::tool::{PathAccess, PathTouch, ToolRegistry, paths_overlap};
use crate::transcript::ToolCall;

use super::ToolExposure;

/// The paths one call will touch, split into read/write sets for the
/// scheduler. [`Access::Unknown`] is a wildcard: possible conflict with
/// anything that touches a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CallAccess {
    reads: Access,
    writes: Access,
}

/// One side of a [`CallAccess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Access {
    /// Touches no path of this kind.
    Nothing,
    /// Cannot be described — a wildcard.
    Unknown,
    /// Exactly these workspace-relative paths.
    Known(HashSet<String>),
}

/// Derives a call's access from its tool: a not-exposed/unknown call never
/// executes and so touches nothing; a read-only tool contributes its declared
/// reads; a mutating tool contributes its declared writes and treats its own
/// reads as unknown (it may read anything before writing).
pub(super) fn call_access(
    call: &ToolCall,
    registry: &ToolRegistry,
    exposure: &ToolExposure,
) -> CallAccess {
    let tool = exposure
        .contains(&call.name)
        .then(|| registry.get(&call.name))
        .flatten();
    let Some(tool) = tool else {
        return CallAccess {
            reads: Access::Nothing,
            writes: Access::Nothing,
        };
    };
    let (reads, writes) = split_access(tool.touched_paths(&call.arguments));
    if tool.is_read_only() {
        // A read-only tool never writes, whatever it declares.
        CallAccess {
            reads,
            writes: Access::Nothing,
        }
    } else {
        // A mutating tool contributes exactly the paths it declares (its
        // reads are usually empty); undeclared paths stay `Unknown` via
        // `split_access` and therefore serialize.
        CallAccess { reads, writes }
    }
}

/// Splits declared touches into read/write sets; `None` (undeclared) is a
/// wildcard on both sides.
fn split_access(declared: Option<Vec<PathTouch>>) -> (Access, Access) {
    let Some(touches) = declared else {
        return (Access::Unknown, Access::Unknown);
    };
    let mut reads = HashSet::new();
    let mut writes = HashSet::new();
    for touch in touches {
        match touch.access {
            PathAccess::Read => {
                reads.insert(touch.path);
            }
            PathAccess::Write => {
                writes.insert(touch.path);
            }
        }
    }
    (known(reads), known(writes))
}

/// A non-empty set becomes [`Access::Known`]; an empty one is
/// [`Access::Nothing`] (touches nothing).
fn known(paths: HashSet<String>) -> Access {
    if paths.is_empty() {
        Access::Nothing
    } else {
        Access::Known(paths)
    }
}

/// Whether two calls conflict: one writes a path the other touches.
pub(super) fn conflicts(a: &CallAccess, b: &CallAccess) -> bool {
    writes_overlap(&a.writes, &b.reads)
        || writes_overlap(&a.writes, &b.writes)
        || writes_overlap(&b.writes, &a.reads)
}

/// Whether `writer` writes anything `other` touches.
fn writes_overlap(writer: &Access, other: &Access) -> bool {
    match (writer, other) {
        (Access::Nothing, _) | (_, Access::Nothing) => false,
        (Access::Unknown, _) | (_, Access::Unknown) => true,
        (Access::Known(written), Access::Known(touched)) => written
            .iter()
            .any(|w| touched.iter().any(|t| paths_overlap(w, t))),
    }
}

/// Assigns each call to a wave: the longest path over conflict edges. Two
/// calls share a wave only when conflict-free, and a conflict always puts the
/// later call in a strictly later wave, preserving model order on shared
/// paths. Waves run in order; calls within a wave run concurrently.
pub(super) fn schedule(accesses: &[CallAccess]) -> Vec<Vec<usize>> {
    let mut level = vec![0usize; accesses.len()];
    for later in 0..accesses.len() {
        for earlier in 0..later {
            if conflicts(&accesses[earlier], &accesses[later]) {
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
