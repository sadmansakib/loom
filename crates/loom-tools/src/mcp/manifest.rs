//! The `.loom/mcp.toml` manifest: the server allowlist and per-server
//! tool policy, parsed with the same strictness as the command manifest.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::policy::PolicyVerdict;

use super::McpError;

/// The parsed `.loom/mcp.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpManifest {
    /// Declared servers, by name. `BTreeMap` so attach order (and
    /// therefore registry order) is deterministic across runs.
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerDecl>,
}

/// One declared server.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerDecl {
    /// The argv to spawn, e.g. `["npx", "-y", "@modelcontextprotocol/server-github"]`.
    /// Non-empty and required.
    pub command: Vec<String>,
    /// Explicit environment for the server. The child environment is
    /// otherwise scrubbed (plus a `PATH` passthrough so argv[0] resolves).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Allow network access inside the sandbox. Default false.
    #[serde(default)]
    pub network: bool,
    /// Tools pinned eagerly even under D2 deferred loading. Parsed in D1,
    /// where everything is eager; reserved for D2.
    #[serde(default)]
    pub always_load: Vec<String>,
    /// Per-server tool policy.
    #[serde(default)]
    pub policy: McpToolPolicy,
}

/// Per-server policy over ingested tools.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolPolicy {
    /// Verdict applied to tools flagged by ingestion hygiene (imperative
    /// descriptions). `allow` loads the tool with a visible warning;
    /// `ask`/`deny` exclude it (ask fails closed until a frontend exists).
    #[serde(default)]
    pub unknown: PolicyVerdict,
    /// Glob patterns over server-side tool names; matches are excluded.
    #[serde(default)]
    pub deny: Vec<String>,
}

impl McpManifest {
    /// Parses manifest TOML.
    pub fn parse(text: &str) -> Result<Self, McpError> {
        let manifest: Self =
            toml::from_str(text).map_err(|error| McpError::Invalid(error.to_string()))?;
        for (name, decl) in &manifest.servers {
            if decl.command.is_empty() {
                return Err(McpError::Invalid(format!(
                    "server '{name}': command must be a non-empty argv list"
                )));
            }
        }
        Ok(manifest)
    }
}
