//! Editor-forwarded `mcp_servers`: admission gating and mapping to
//! engine server declarations.

use agent_client_protocol::v1::schema as acp;

use crate::session::AcpState;

/// Resolves the `mcp_servers` a client forwarded in `session/new` into
/// the decl map the engine attaches, honoring the operator gate.
///
/// Off (the default): every forwarded server is ignored with one
/// warning — attaching a server the operator did not configure would
/// grant it process spawn and tool-driven network inside the sandbox.
/// On: stdio servers map to decls with the network grant implied by
/// admission (ACP carries no per-server field) and eager-only loading;
/// anything else is a hard `session/new` error rather than a silent
/// partial attach.
pub(crate) fn forwarded_set(
    state: &AcpState,
    request: &acp::NewSessionRequest,
) -> Result<std::collections::BTreeMap<String, loom_core::tools::mcp::McpServerDecl>, String> {
    if request.mcp_servers.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }
    if !state.options.allow_client_mcp {
        tracing::warn!(
            servers = request.mcp_servers.len(),
            "ignoring editor-forwarded mcp_servers; start loom-acp with --allow-client-mcp to attach them"
        );
        return Ok(std::collections::BTreeMap::new());
    }
    forwarded_decls(&request.mcp_servers)
}

/// Maps ACP `McpServer`s to loom decls. Stdio only: the http/sse
/// transports need an outbound connection the operator never granted.
pub(crate) fn forwarded_decls(
    servers: &[acp::McpServer],
) -> Result<std::collections::BTreeMap<String, loom_core::tools::mcp::McpServerDecl>, String> {
    let mut decls = std::collections::BTreeMap::new();
    for server in servers {
        let (name, command, env) = match server {
            acp::McpServer::Stdio(stdio) => {
                if stdio.name.trim().is_empty() || stdio.name.contains('\0') {
                    return Err(
                        "forwarded MCP server name must be non-empty and contain no NUL".to_owned(),
                    );
                }
                let command_name = stdio
                    .command
                    .to_str()
                    .filter(|command| {
                        !command.is_empty()
                            && !command.contains('\0')
                            && stdio.command.is_absolute()
                    })
                    .ok_or_else(|| {
                        format!(
                            "forwarded MCP server `{}` must have an absolute UTF-8 command",
                            stdio.name
                        )
                    })?;
                if stdio.args.iter().any(|argument| argument.contains('\0')) {
                    return Err(format!(
                        "forwarded MCP server `{}` has a command argument containing NUL",
                        stdio.name
                    ));
                }
                let mut command = vec![command_name.to_owned()];
                command.extend(stdio.args.iter().cloned());
                let mut env = std::collections::BTreeMap::new();
                for variable in &stdio.env {
                    if variable.name.is_empty()
                        || variable.name.contains('=')
                        || variable.name.contains('\0')
                        || variable.value.contains('\0')
                    {
                        return Err(format!(
                            "forwarded MCP server `{}` has an invalid environment variable",
                            stdio.name
                        ));
                    }
                    if has_env_key(&env, &variable.name) {
                        return Err(format!(
                            "forwarded MCP server `{}` repeats environment variable `{}`",
                            stdio.name, variable.name
                        ));
                    }
                    env.insert(variable.name.clone(), variable.value.clone());
                }
                (stdio.name.clone(), command, env)
            }
            acp::McpServer::Http(http) => {
                return Err(format!(
                    "forwarded MCP server `{}` uses the http transport; only stdio is supported",
                    http.name
                ));
            }
            acp::McpServer::Sse(sse) => {
                return Err(format!(
                    "forwarded MCP server `{}` uses the sse transport; only stdio is supported",
                    sse.name
                ));
            }
            // The schema enum is non-exhaustive: a future transport is
            // refused the same way, not silently dropped.
            _ => {
                return Err(
                    "forwarded MCP server uses an unsupported transport; only stdio is supported"
                        .to_string(),
                );
            }
        };
        let declaration = loom_core::tools::mcp::McpServerDecl {
            command,
            env,
            // Admission is the grant: ACP carries no per-server
            // network field, so an admitted server gets network.
            network: true,
            // Eager-only attach (`attach_forwarded`): no pins.
            always_load: Vec::new(),
            policy: loom_core::tools::mcp::McpToolPolicy::default(),
        };
        if decls.insert(name.clone(), declaration).is_some() {
            return Err(format!(
                "forwarded MCP server `{name}` is declared more than once"
            ));
        }
    }
    Ok(decls)
}

fn has_env_key(env: &std::collections::BTreeMap<String, String>, name: &str) -> bool {
    #[cfg(windows)]
    {
        env.keys()
            .any(|existing| existing.eq_ignore_ascii_case(name))
    }

    #[cfg(not(windows))]
    {
        env.contains_key(name)
    }
}
