//! Operator context actions (`/context`'s pin and drop): the live sets
//! the pruning compiler consults when it freezes stub decisions.
//!
//! Pin and drop are per-*session* policy, not per-task state: the
//! operator acts between tasks and the next task's fresh checkpoint
//! honors the sets. Pinning keeps a result out of every pruning pass;
//! dropping stubs it even below the window trigger and outside the
//! reclaim gate — the operator asked, so cache economics do not veto.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

/// A snapshot of the operator's pinned and dropped call ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOps {
    /// Call ids that no pruning pass may stub.
    pub pinned: BTreeSet<String>,
    /// Call ids the operator dropped; the compiler stubs them on the
    /// next decision pass regardless of the trigger.
    pub dropped: BTreeSet<String>,
}

impl PruneOps {
    /// Whether no operator action is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pinned.is_empty() && self.dropped.is_empty()
    }
}

/// Cloneable handle to the engine's [`PruneOps`]. Clones share one set,
/// so the engine's API and a frontend's snapshot see the same policy.
#[derive(Debug, Clone, Default)]
pub struct PruneOpsHandle {
    ops: Arc<Mutex<PruneOps>>,
}

impl PruneOpsHandle {
    /// Pins `call_id` to the prefix: later passes never stub it. Also
    /// clears a pending drop of the same call.
    pub fn pin(&self, call_id: impl Into<String>) {
        let mut ops = self.lock();
        let call_id = call_id.into();
        ops.dropped.remove(&call_id);
        ops.pinned.insert(call_id);
    }

    /// Drops `call_id`: its result is stubbed on the next decision pass
    /// even below the window trigger. Also clears a pin of the same
    /// call.
    pub fn drop(&self, call_id: impl Into<String>) {
        let mut ops = self.lock();
        let call_id = call_id.into();
        ops.pinned.remove(&call_id);
        ops.dropped.insert(call_id);
    }

    /// A point-in-time copy for the compiler or a frontend.
    #[must_use]
    pub fn snapshot(&self) -> PruneOps {
        self.lock().clone()
    }

    /// A poisoned lock is recovered: the sets are advisory policy, and
    /// a panic elsewhere must not disable pruning.
    fn lock(&self) -> std::sync::MutexGuard<'_, PruneOps> {
        self.ops.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_and_drop_are_mutually_exclusive_and_shared_across_clones() {
        let handle = PruneOpsHandle::default();
        let copy = handle.clone();
        handle.pin("call_1");
        copy.drop("call_1");
        let ops = handle.snapshot();
        assert!(ops.pinned.is_empty());
        assert!(ops.dropped.contains("call_1"));
        copy.pin("call_1");
        let ops = handle.snapshot();
        assert!(ops.dropped.is_empty());
        assert!(ops.pinned.contains("call_1"));
    }
}
