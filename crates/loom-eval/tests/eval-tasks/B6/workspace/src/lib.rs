//! Capabilities resolved from a provider model catalog.
use serde::Deserialize;

/// One raw catalog entry, as served by the provider.
#[derive(Debug, Clone, Deserialize)]
pub struct RawModel {
    pub id: String,
    pub context_length: u64,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

/// What the agent loop needs to know before it schedules calls.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    pub max_context_tokens: u64,
    pub tool_calls: bool,
    pub parallel_tool_calls: bool,
}

impl RawModel {
    fn supports(&self, parameter: &str) -> bool {
        self.supported_parameters.iter().any(|listed| listed == parameter)
    }

    /// Capabilities for this model.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: self.context_length,
            tool_calls: self.supports("tools"),
            parallel_tool_calls: self.supports("tools"),
        }
    }
}
