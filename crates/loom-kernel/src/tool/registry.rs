//! The name-keyed set of tools the engine dispatches against.

use std::sync::Arc;

use super::context::Tool;
use super::spec::ToolSpec;

/// Failure mode of [`ToolRegistry::register`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ToolRegistryError {
    /// A tool with this name is already registered; registration is
    /// refused so `get` stays unambiguous.
    #[error("tool already registered: {name}")]
    DuplicateName {
        /// The duplicated tool name.
        name: String,
    },
}

/// The name-keyed set of tools a task may dispatch against.
///
/// Definitions only: it does not execute anything. The engine resolves
/// model-issued [`ToolCall`](crate::transcript::ToolCall)s through
/// [`ToolRegistry::get`] and offers [`ToolRegistry::specs`] to the
/// context compiler for building
/// [`ModelRequest::tools`](crate::model::ModelRequest::tools).
///
/// Storage preserves registration order, so `specs()` yields a stable
/// tool list — renaming or reordering tools mid-task would invalidate
/// the provider's cached prompt prefix.
///
/// Tools are held behind `Arc` so a registry can be cloned cheaply
/// ([`ToolRegistry::shallow_clone`]). The agent loop uses that to layer
/// a session's dynamic tool surface ([`crate::agent::ToolSurface`]) on
/// top of the base registry without disturbing it.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Tool` isn't `Debug`; tool names are the useful summary.
        let names: Vec<_> = self.tools.iter().map(|t| t.spec().name.as_str()).collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Adds a tool, refusing duplicates by name.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolRegistryError> {
        self.register_arc(tool.into())
    }

    /// Adds an already-shared tool, refusing duplicates by name.
    ///
    /// The dynamic tool surface hands the same `Arc` it tracks to the
    /// per-turn registry view, so dispatch and the surface stay in
    /// lockstep without re-boxing.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let name = tool.spec().name.clone();
        if self.get(&name).is_some() {
            return Err(ToolRegistryError::DuplicateName { name });
        }
        self.tools.push(tool);
        Ok(())
    }

    /// Looks a tool up by the name the model used.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .map(|tool| tool.as_ref())
    }

    /// All specs, in registration order — the stable ordering the model
    /// sees across requests.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.iter().map(|tool| tool.spec())
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The registered tools as shared handles, in order — for
    /// embedders that build sibling registries over the same tool
    /// set (routing's tier agents share the one registry the run
    /// built, scout registration included) without consuming it.
    pub fn arc_tools(&self) -> impl Iterator<Item = Arc<dyn Tool>> + '_ {
        self.tools.iter().cloned()
    }

    /// Consumes the registry, yielding the registered tools (as shared
    /// handles) in order. For embedders that filter a built registry
    /// (the bare-loop arm keeps the primitive tools only) rather than
    /// rebuilding one.
    pub fn into_arc_tools(self) -> impl Iterator<Item = Arc<dyn Tool>> {
        self.tools.into_iter()
    }

    /// Builds a registry from tools that are already unique by name.
    /// Order is preserved (the stable ordering the model sees).
    pub fn from_arc_tools<I>(tools: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn Tool>>,
    {
        let mut registry = Self::new();
        for tool in tools {
            let result = registry.register_arc(tool);
            debug_assert!(
                result.is_ok(),
                "from_arc_tools received a duplicate tool name: {result:?}"
            );
        }
        registry
    }
}
