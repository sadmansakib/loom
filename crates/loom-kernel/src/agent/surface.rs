//! The session tool surface: tools that load and evict between turns.
//!
//! MCP-style integrations advertise far more tools than any one task
//! uses, and eager registration would tax every request with schemas
//! the model never dispatches. The surface holds those tools aside; a
//! meta-tool queues the ones the model asks for
//! ([`ToolSurface::queue`]), and the agent loop reconciles the queue at
//! each turn boundary ([`ToolSurface::end_turn`]) so a queued tool is
//! dispatchable from the next compile onward.
//!
//! Ordering is append-only: the loop layers the surface on top of the
//! base registry ([`ToolRegistry::shallow_clone`]), with loaded tools
//! appended in load order. Appending never disturbs the provider's
//! cached prompt prefix. Eviction — the reverse — can invalidate that
//! prefix, so it only happens after [`SurfacePolicy`]'s idle threshold
//! has fully elapsed, and is the caller's decision to enable.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::tool::{Tool, ToolRegistry, ToolSpec};

/// Default idle turns before a loaded, unused-since tool evicts.
pub const DEFAULT_EVICT_AFTER_IDLE_TURNS: u32 = 8;

/// When the surface evicts a loaded tool. `0` disables eviction: tools
/// stay loaded for the session once queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePolicy {
    /// Turns a loaded tool may go unused before eviction. Zero never
    /// evicts.
    pub evict_after_idle_turns: u32,
}

impl Default for SurfacePolicy {
    fn default() -> Self {
        Self {
            evict_after_idle_turns: DEFAULT_EVICT_AFTER_IDLE_TURNS,
        }
    }
}

#[derive(Default)]
struct SurfaceState {
    /// Queued this turn by a meta-tool; promoted into `loaded` at the
    /// turn boundary.
    pending: Vec<Arc<dyn Tool>>,
    /// Active tools, in load order — the append-only tail of the
    /// per-turn tool list.
    loaded: Vec<Arc<dyn Tool>>,
    /// Turn index each loaded tool was last used (dispatched) on. A
    /// tool promoted this turn is stamped with the load turn.
    last_used: BTreeMap<String, u32>,
    /// The turn the loop most recently announced.
    turn: u32,
}

/// Shared, interior-mutable tool surface. The agent loop and the
/// meta-tools that feed it hold clones of the same `Arc`.
///
/// All methods are lock-and-return; none await, so a meta-tool can
/// call [`ToolSurface::queue`] from inside its `call` future without
/// holding the lock across an await.
pub struct ToolSurface {
    policy: SurfacePolicy,
    state: Mutex<SurfaceState>,
}

impl std::fmt::Debug for ToolSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Tool` isn't `Debug`; loaded names are the useful summary.
        let names = self.loaded_names();
        f.debug_struct("ToolSurface")
            .field("policy", &self.policy)
            .field("loaded", &names)
            .finish()
    }
}

impl ToolSurface {
    /// Creates a surface with the given eviction policy.
    #[must_use]
    pub fn new(policy: SurfacePolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(SurfaceState::default()),
        }
    }

    /// Queues a tool for loading at the end of the current turn. Fails
    /// (with the duplicate name) if the tool is already queued or
    /// loaded; the caller surfaces that to the model as a one-turn
    /// self-correction.
    pub fn queue(&self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let name = tool.spec().name.clone();
        if state.loaded.iter().any(|t| t.spec().name == name)
            || state.pending.iter().any(|t| t.spec().name == name)
        {
            return Err(name);
        }
        state.pending.push(tool);
        Ok(())
    }

    /// Announces the turn the loop is entering. Idempotent; must be
    /// called before [`ToolSurface::mark_used`] for that turn so
    /// stamps land on the right index.
    pub fn begin_turn(&self, turn: u32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.turn = state.turn.max(turn);
    }

    /// Records that a tool by this name was dispatched this turn.
    /// Unknown names are ignored — the base registry's tools are none
    /// of the surface's business.
    pub fn mark_used(&self, name: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let turn = state.turn;
        if state.last_used.contains_key(name) {
            state.last_used.insert(name.to_owned(), turn);
        }
    }

    /// Reconciles at the turn boundary: promotes queued tools to
    /// loaded (stamped with this turn), then evicts loaded tools idle
    /// for at least the policy threshold. Returns the names evicted,
    /// for logging.
    pub fn end_turn(&self) -> Vec<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let turn = state.turn;
        let promoted: Vec<Arc<dyn Tool>> = std::mem::take(&mut state.pending);
        for tool in promoted {
            state.last_used.insert(tool.spec().name.clone(), turn);
            state.loaded.push(tool);
        }
        if self.policy.evict_after_idle_turns == 0 {
            return Vec::new();
        }
        let threshold = self.policy.evict_after_idle_turns;
        // Snapshot before the mutable passes: names are unique in
        // `loaded` (queue refuses duplicates), so a name→turn map is
        // enough to decide both passes without re-borrowing `state`.
        let last_used: BTreeMap<String, u32> = state.last_used.clone();
        let evicted: Vec<String> = state
            .loaded
            .iter()
            .filter_map(|tool| {
                let name = tool.spec().name.clone();
                let idle = last_used
                    .get(&name)
                    .is_some_and(|&last| turn.saturating_sub(last) >= threshold);
                idle.then_some(name)
            })
            .collect();
        state
            .loaded
            .retain(|tool| !evicted.contains(&tool.spec().name));
        state.last_used.retain(|name, _| !evicted.contains(name));
        evicted
    }

    /// The names of currently loaded tools, in load order.
    pub fn loaded_names(&self) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.loaded.iter().map(|t| t.spec().name.clone()).collect()
    }

    /// Layers the loaded tools onto a clone of `base`, returning the
    /// per-turn registry view. Tools whose names collide with `base`
    /// are skipped (the base wins); the surface keeps tracking them,
    /// which is harmless — they can never dispatch through it.
    #[must_use]
    pub fn extend_registry(&self, base: &ToolRegistry) -> ToolRegistry {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut view = base.clone();
        for tool in &state.loaded {
            let _ = view.register_arc(Arc::clone(tool));
        }
        view
    }

    /// Specs of the loaded tools, in load order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.loaded.iter().map(|t| t.spec().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolContext, ToolError, ToolOutput, ToolSpec};
    use futures::future::BoxFuture;

    struct Probe {
        spec: ToolSpec,
    }

    impl Probe {
        fn new(name: &str) -> Self {
            Self {
                spec: ToolSpec {
                    name: name.to_owned(),
                    description: name.to_owned(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }
        }
    }

    impl Tool for Probe {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn is_read_only(&self) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _input: serde_json::Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async { Ok(ToolOutput::new("ok".to_owned())) })
        }
    }

    fn probe(name: &str) -> Arc<dyn Tool> {
        Arc::new(Probe::new(name))
    }

    #[test]
    fn queue_promotes_at_turn_boundary() {
        let surface = ToolSurface::new(SurfacePolicy::default());
        surface.begin_turn(1);
        surface.queue(probe("mcp_a_b")).expect("first queue");
        assert!(surface.loaded_names().is_empty(), "not yet visible");
        surface.end_turn();
        assert_eq!(surface.loaded_names(), vec!["mcp_a_b".to_owned()]);
    }

    #[test]
    fn duplicate_queue_fails() {
        let surface = ToolSurface::new(SurfacePolicy::default());
        surface.queue(probe("mcp_a_b")).expect("first");
        let err = surface.queue(probe("mcp_a_b")).unwrap_err();
        assert_eq!(err, "mcp_a_b");
        surface.end_turn();
        let err = surface.queue(probe("mcp_a_b")).unwrap_err();
        assert_eq!(err, "mcp_a_b");
    }

    #[test]
    fn idle_tools_evict_after_threshold() {
        let surface = ToolSurface::new(SurfacePolicy {
            evict_after_idle_turns: 2,
        });
        // Loaded at the end of turn 1; used on turn 2.
        surface.begin_turn(1);
        surface.queue(probe("mcp_a_b")).expect("queue");
        surface.end_turn();
        surface.begin_turn(2);
        surface.mark_used("mcp_a_b");
        surface.end_turn();
        assert_eq!(surface.loaded_names(), vec!["mcp_a_b".to_owned()]);
        // Turns 3 and 4 idle: 4 - 2 == 2 reaches the threshold.
        surface.begin_turn(3);
        surface.end_turn();
        assert_eq!(surface.loaded_names(), vec!["mcp_a_b".to_owned()]);
        surface.begin_turn(4);
        surface.end_turn();
        assert!(surface.loaded_names().is_empty());
    }

    #[test]
    fn zero_threshold_never_evicts() {
        let surface = ToolSurface::new(SurfacePolicy {
            evict_after_idle_turns: 0,
        });
        surface.begin_turn(1);
        surface.queue(probe("mcp_a_b")).expect("queue");
        surface.end_turn();
        for turn in 2..20 {
            surface.begin_turn(turn);
            surface.end_turn();
        }
        assert_eq!(surface.loaded_names(), vec!["mcp_a_b".to_owned()]);
    }

    #[test]
    fn extend_registry_appends_after_base() {
        let mut base = ToolRegistry::new();
        base.register(Box::new(Probe::new("base_tool")))
            .expect("base");
        let surface = ToolSurface::new(SurfacePolicy::default());
        surface.begin_turn(1);
        surface.queue(probe("mcp_a_b")).expect("queue");
        surface.queue(probe("mcp_a_c")).expect("queue");
        surface.end_turn();
        let view = surface.extend_registry(&base);
        let names: Vec<String> = view.specs().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["base_tool", "mcp_a_b", "mcp_a_c"]);
        // Base untouched.
        assert_eq!(base.len(), 1);
    }

    #[test]
    fn extend_registry_skips_base_collisions() {
        let mut base = ToolRegistry::new();
        base.register(Box::new(Probe::new("clash"))).expect("base");
        let surface = ToolSurface::new(SurfacePolicy::default());
        surface.begin_turn(1);
        surface.queue(probe("clash")).expect("queue");
        surface.end_turn();
        let view = surface.extend_registry(&base);
        assert_eq!(view.len(), 1);
    }
}
