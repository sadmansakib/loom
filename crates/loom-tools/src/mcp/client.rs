//! The stdio JSON-RPC client: one [`McpServerProcess`] per spawned
//! server, multiplexed requests over its line-delimited transport, and
//! the raw `tools/list` fetch.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};

use crate::sandbox::{Sandbox, SandboxGrants};

use super::{
    HANDSHAKE_TIMEOUT, LINE_MAX_BYTES, LIST_MAX_PAGES, PROTOCOL_VERSION, STDERR_TAIL_BYTES,
};
use super::{McpError, McpServerDecl};

// ---------------------------------------------------------------------------
// stdio JSON-RPC client
// ---------------------------------------------------------------------------

/// Type alias: responses routed by request id.
type Pending = std::sync::Mutex<BTreeMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// A live stdio JSON-RPC connection to one MCP server process.
///
/// The process spawns through the OS sandbox with a scrubbed environment
/// (`PATH` is passed through so argv[0] resolves; everything else comes
/// from the manifest) and is killed when the handle drops.
pub struct McpServerProcess {
    name: String,
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Pending>,
    next_id: AtomicU64,
    /// Last `STDERR_TAIL_BYTES` of server stderr, retained for
    /// diagnostics (`stderr_tail`); stderr never reaches the model.
    stderr_tail: Arc<std::sync::Mutex<Vec<u8>>>,
    /// Set when the server emits `notifications/tools/list_changed`.
    /// D1 never acts on it mid-session (definitions are pinned); D2
    /// re-verifies against the lockfile.
    pub list_changed: Arc<AtomicBool>,
}

impl McpServerProcess {
    /// Spawns (but does not handshake) one server process.
    ///
    /// # Errors
    /// Fails when the sandbox cannot be built for the server's
    /// declaration or the process itself cannot be spawned; the
    /// `McpError` carries the OS error message.
    pub fn spawn(name: &str, decl: &McpServerDecl, workspace: &Path) -> Result<Self, McpError> {
        let sandbox = Sandbox::resolve(
            workspace,
            SandboxGrants {
                enabled: true,
                network: decl.network,
                extra_writable: Vec::new(),
            },
        );
        let (program, args) = sandbox.wrap(&decl.command[0], &decl.command[1..]);
        let mut command = Command::new(&program);
        command
            .args(&args)
            .env_clear()
            .envs(&decl.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // argv[0] resolution happens inside the child; without a PATH the
        // common `npx`/`node`/`uvx` argv forms cannot resolve at all.
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        // The workspace root, so proxy commands (remote-MCP relays) can
        // resolve workspace-relative paths without hardcoding. Mirrors
        // Claude Code's `CLAUDE_PROJECT_DIR` for stdio servers; a
        // manifest `env` entry for the same name wins.
        if !decl.env.contains_key("LOOM_WORKSPACE") {
            let workspace_abs =
                std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            command.env("LOOM_WORKSPACE", workspace_abs);
        }
        let mut child = command.spawn().map_err(|error| McpError::Server {
            server: name.to_owned(),
            message: format!("cannot spawn {}: {error}", decl.command[0]),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Server {
            server: name.to_owned(),
            message: "child stdin was not piped".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Server {
            server: name.to_owned(),
            message: "child stdout was not piped".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::Server {
            server: name.to_owned(),
            message: "child stderr was not piped".to_owned(),
        })?;

        // Stderr ring buffer (diagnostics only): the tail survives for
        // `stderr_tail()` without letting a chatty server grow memory.
        let stderr_tail = spawn_stderr_tail(stderr);

        let stdin: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(stdin));
        let pending: Arc<Pending> = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let list_changed = Arc::new(AtomicBool::new(false));
        spawn_reader(
            stdout,
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&list_changed),
            name.to_owned(),
        );

        Ok(Self {
            name: name.to_owned(),
            child,
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            stderr_tail,
            list_changed,
        })
    }

    async fn write_line(&self, value: &Value) -> Result<(), McpError> {
        write_raw(&self.stdin, value)
            .await
            .map_err(|error| McpError::Server {
                server: self.name.clone(),
                message: format!("cannot write to server stdin: {error}"),
            })
    }

    /// Last `STDERR_TAIL_BYTES` of server stderr (lossy UTF-8),
    /// diagnostics only — never surfaced to the model.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map_or_default(|tail| String::from_utf8_lossy(&tail).into_owned())
    }
}

/// Pumps server stderr into a bounded ring buffer: the tail survives
/// for `stderr_tail()` without letting a chatty server grow memory.
fn spawn_stderr_tail(stderr: ChildStderr) -> Arc<std::sync::Mutex<Vec<u8>>> {
    let ring: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
    let pump = Arc::clone(&ring);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 1024];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut tail) = pump.lock() {
                        tail.extend_from_slice(&chunk[..n]);
                        let len = tail.len();
                        if len > STDERR_TAIL_BYTES {
                            tail.drain(..len - STDERR_TAIL_BYTES);
                        }
                    }
                }
            }
        }
    });
    ring
}

/// Reads server stdout lines, routes responses to pending requests,
/// answers the spec-required `ping`, and records
/// `notifications/tools/list_changed`. Runs until EOF, then fails
/// every outstanding request so callers wake.
fn spawn_reader(
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Pending>,
    list_changed: Arc<AtomicBool>,
    server_name: String,
) {
    tokio::spawn(async move {
        // Bounded line framing: oversized lines are discarded whole
        // (the codec resynchronizes at the next newline) instead of
        // buffering unbounded bytes.
        let mut framed = tokio_util::codec::FramedRead::new(
            stdout,
            tokio_util::codec::LinesCodec::new_with_max_length(LINE_MAX_BYTES),
        );
        while let Some(item) = futures::StreamExt::next(&mut framed).await {
            let Ok(line) = item else {
                tracing::warn!(
                    server = %server_name,
                    "MCP server emitted a line over the size cap; discarded"
                );
                continue;
            };
            // Non-JSON lines (harness noise, logs) are skipped.
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Classify by `method` first: a server-initiated request
            // carries BOTH `id` and `method`, and routing on `id`
            // first would misdeliver it to a pending client request
            // that happens to hold the same numeric id.
            match value.get("method").and_then(Value::as_str) {
                Some(method) => {
                    if let Some(id) = value.get("id").cloned() {
                        // loom answers only the spec-required `ping`;
                        // everything else (sampling, elicitation,
                        // roots) is refused — those capabilities are
                        // never declared, so a spec-compliant server
                        // never calls them in the first place.
                        let response = if method == "ping" {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {},
                            })
                        } else {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32601,
                                    "message": format!("loom does not implement '{method}'"),
                                },
                            })
                        };
                        let _ = write_raw(&stdin, &response).await;
                    } else if method == "notifications/tools/list_changed" {
                        list_changed.store(true, Ordering::Relaxed);
                    }
                    // Other notifications are ignored.
                }
                None => {
                    if let Some(id) = value.get("id").and_then(Value::as_u64)
                        && let Ok(mut map) = pending.lock()
                        && let Some(sender) = map.remove(&id)
                    {
                        let _ = sender.send(Ok(value));
                    }
                }
            }
        }
        // EOF: fail every outstanding request so callers wake.
        if let Ok(mut map) = pending.lock() {
            let senders: Vec<_> = std::mem::take(&mut *map).into_values().collect();
            for sender in senders {
                let _ = sender.send(Err("server closed its stdout".to_owned()));
            }
        }
    });
}

/// Writes one JSON-RPC line to a shared server stdin. Used by both
/// [`McpServerProcess::write_line`] and the reader task (which answers
/// server-initiated requests without owning the process handle).
async fn write_raw(stdin: &Mutex<ChildStdin>, value: &Value) -> std::io::Result<()> {
    let mut line =
        serde_json::to_string(value).map_err(|error| std::io::Error::other(error.to_string()))?;
    line.push('\n');
    let mut guard = stdin.lock().await;
    guard.write_all(line.as_bytes()).await?;
    guard.flush().await
}

impl McpServerProcess {
    /// Sends one request and awaits its response.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, sender);
        }
        self.write_line(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        let outcome = tokio::time::timeout(timeout, receiver).await;
        let Ok(Ok(Ok(value))) = outcome else {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&id);
            }
            // The request is abandoned; tell the server to stop
            // spending on it (spec: `notifications/cancelled`).
            // Best effort — a dead stdin is not a second error.
            let _ = self
                .write_line(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"id": id},
                }))
                .await;
            return Err(McpError::Server {
                server: self.name.clone(),
                message: format!("request '{method}' timed out or lost its reader"),
            });
        };
        if let Some(error) = value.get("error") {
            return Err(McpError::Server {
                server: self.name.clone(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_owned(),
            });
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The initialize handshake plus the `initialized` notification.
    /// The server's negotiated `protocolVersion` must match exactly —
    /// spec says a client that does not support the negotiated version
    /// SHOULD disconnect, and loom only exercises `tools/*` on
    /// 2025-06-18, so anything else fails the attach closed.
    ///
    /// # Errors
    /// Fails when the `initialize` request times out or the server's
    /// response is malformed, or when the negotiated `protocolVersion`
    /// is not exactly [`PROTOCOL_VERSION`].
    pub async fn handshake(&self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "loom",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        let negotiated = result.get("protocolVersion").and_then(Value::as_str);
        if negotiated != Some(PROTOCOL_VERSION) {
            return Err(McpError::Server {
                server: self.name.clone(),
                message: format!(
                    "server negotiated protocol version {} but loom speaks \
                     {PROTOCOL_VERSION}; refusing attach",
                    negotiated.unwrap_or("<missing>")
                ),
            });
        }
        self.write_line(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .await
    }

    /// Pages `tools/list` into raw tool definitions.
    pub(super) async fn tools(&self) -> Result<Vec<RawTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        // Bounded pagination: a server that loops on nextCursor is a bug,
        // not a reason to hang attach.
        for _ in 0..LIST_MAX_PAGES {
            let mut params = serde_json::json!({});
            if let Some(ref c) = cursor {
                params["cursor"] = c.clone();
            }
            let result = self
                .request("tools/list", params, HANDSHAKE_TIMEOUT)
                .await?;
            if let Some(items) = result.get("tools").and_then(Value::as_array) {
                for item in items {
                    tools.push(RawTool {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        description: item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input_schema: item
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    });
                }
            }
            cursor = result.get("nextCursor").cloned();
            if cursor.is_none() {
                break;
            }
        }
        if cursor.is_some() {
            // Loud, not silent: tools beyond the page cap are invisible
            // to the session, and the operator should know.
            tracing::warn!(
                server = %self.name,
                pages = LIST_MAX_PAGES,
                "tools/list pagination cap reached; further pages ignored"
            );
        }
        Ok(tools)
    }

    /// Calls one server-side tool by name. The result is the raw
    /// `tools/call` result object (`content` array, optional `isError`).
    pub(super) async fn call(
        &self,
        name: &str,
        arguments: &Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
            timeout,
        )
        .await
    }

    /// Whether the child has exited (diagnostics only).
    pub fn try_wait_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// One raw (unsanitized) `tools/list` entry.
pub(super) struct RawTool {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) input_schema: Value,
}
