//! The sandboxed stdio LSP client (docs/lsp-design.md §4/§5/§7).
//!
//! One [`LspServer`] per attached server process: the child is spawned
//! through the same OS-sandboxed exec path as every loom command, the
//! wire protocol runs on async-lsp's `MainLoop` (the Helix client
//! pattern), and all server-initiated requests are answered
//! deterministically: `workspace/configuration` from the manifest,
//! capability registration is accepted and recorded, work-done progress
//! is accepted, `workspace/applyEdit` is DECLINED (servers advise, loom
//! writes), and `window/*` chatter goes to tracing.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_lsp::concurrency::ConcurrencyBuilder;
use async_lsp::panic::CatchUnwindBuilder;
use async_lsp::router::Router;
use async_lsp::{
    AnyNotification, AnyRequest, ErrorCode, MainLoop, ResponseError, ServerSocket, lsp_types,
};
use lsp_types::notification::{DidOpenTextDocument, Initialized, PublishDiagnostics};
use lsp_types::request::{
    ApplyWorkspaceEdit, RegisterCapability, WorkDoneProgressCreate, WorkspaceConfiguration,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::lsp::manifest::{KnownServer, ServerCandidate};
use crate::sandbox::{Sandbox, SandboxGrants};

/// Wall-time cap for the initialize handshake.
const INIT_TIMEOUT_SECS: u64 = 60;
/// Wall-time cap for one LSP request.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Shared per-server state, owned by the router and read by tools.
pub(crate) struct ServerState {
    /// Push diagnostics cache: file path key -> latest diagnostics.
    pub diagnostics: Mutex<BTreeMap<String, Vec<lsp_types::Diagnostic>>>,
    /// Manifest-provided settings served to `workspace/configuration`.
    pub settings: serde_json::Value,
    /// Capabilities the server registered dynamically (recorded, unused).
    pub registered: Mutex<Vec<String>>,
}

/// A live LSP server connection.
pub struct LspServer {
    /// Final server name (menu label).
    pub name: String,
    /// Table entry when this server came from KNOWN_SERVERS.
    pub known: Option<&'static KnownServer>,
    /// The server's declared capabilities (from initialize).
    pub capabilities: lsp_types::ServerCapabilities,
    pub(crate) socket: ServerSocket,
    _child: tokio::process::Child,
    _drive: tokio::task::JoinHandle<()>,
    pub(crate) state: Arc<ServerState>,
    /// Open documents: workspace-relative-or-absolute path -> version.
    open_files: Mutex<BTreeMap<String, u32>>,
    /// True when the server negotiated UTF-8 position encoding.
    utf8_positions: bool,
    /// Workspace root (file paths resolve against it).
    root: std::path::PathBuf,
}

impl LspServer {
    /// The push-diagnostics cache snapshot for one file.
    pub fn cached_diagnostics(&self, path: &str) -> Vec<lsp_types::Diagnostic> {
        self.state
            .diagnostics
            .lock()
            .map(|m| m.get(path).cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Files currently open (didOpen sent) — exposed for tests.
    pub fn open_file_paths(&self) -> Vec<String> {
        self.open_files
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Language id for a file path, from the table's suffix map.
    pub fn language_id(&self, path: &str) -> Option<String> {
        let suffix = path.rsplit('.').next()?.to_ascii_lowercase();
        let suffix = format!(".{suffix}");
        self.known?
            .suffixes
            .iter()
            .find(|s| **s == suffix)
            .map(|s| s.trim_start_matches('.').to_string())
    }

    /// True when this server owns files with this path's suffix.
    pub fn owns_path(&self, path: &str) -> bool {
        self.language_id(path).is_some()
    }

    /// Resolves a tool-supplied path against the workspace root.
    pub fn resolve_path(&self, path: &str) -> std::path::PathBuf {
        resolve(&self.root, path)
    }

    /// The document URI for a tool-supplied path.
    pub fn document_uri(&self, path: &str) -> Result<lsp_types::Url, String> {
        url_for(&resolve(&self.root, path))
    }

    /// Canonical key for the open-files map: the resolved (absolute)
    /// path string. Tool callers pass surface paths ("src/lib.rs"),
    /// edit application passes URI-derived absolute paths — both must
    /// land on the same key or version tracking misses (E2).
    pub(crate) fn open_key(&self, path: &str) -> String {
        resolve(&self.root, path).to_string_lossy().into_owned()
    }

    /// True when the server negotiated UTF-8 position encoding.
    pub(crate) fn uses_utf8_positions(&self) -> bool {
        self.utf8_positions
    }

    /// The currently open version for a path (None when not open) —
    /// the stale-computation guard for server-computed edits (E2).
    pub(crate) fn open_version(&self, path: &str) -> Option<u32> {
        let key = self.open_key(path);
        self.open_files
            .lock()
            .ok()
            .and_then(|m| m.get(&key).copied())
    }

    /// Informs the server that loom itself wrote `path` (E2): open
    /// files get a full-text didChange with a bumped version so
    /// subsequent server computations see loom's writes; files not
    /// open are left alone (the next ensure_open reads disk truth).
    pub(crate) async fn note_local_write(&self, path: &str, text: String) {
        use lsp_types::notification::DidChangeTextDocument;
        let key = self.open_key(path);
        let next = match self.open_files.lock() {
            Ok(mut open) => match open.get_mut(&key) {
                Some(version) => {
                    *version = version.saturating_add(1);
                    *version
                }
                None => return, // not open: nothing to sync
            },
            Err(_) => return,
        };
        let full = resolve(&self.root, path);
        let Ok(uri) = url_for(&full) else {
            return;
        };
        if let Err(e) =
            self.socket
                .notify::<DidChangeTextDocument>(lsp_types::DidChangeTextDocumentParams {
                    text_document: lsp_types::VersionedTextDocumentIdentifier {
                        uri,
                        version: i32::try_from(next).unwrap_or(1),
                    },
                    content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text,
                    }],
                })
        {
            tracing::warn!(server = %self.name, "didChange after local write failed: {e}");
        }
    }
}

/// Resolves a workspace path against the server root.
fn resolve(root: &Path, path: &str) -> std::path::PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn url_for(path: &Path) -> Result<lsp_types::Url, String> {
    lsp_types::Url::from_file_path(path)
        .map_err(|_| format!("cannot express {path:?} as a file URI"))
}

fn line_at(text: &str, line_1based: u32) -> String {
    text.lines()
        .nth(line_1based.saturating_sub(1) as usize)
        .unwrap_or("")
        .to_string()
}

impl LspServer {
    /// Ensures `didOpen` has been sent for the file, reading current
    /// disk content (docs/lsp-design.md §5: lazy open, disk truth).
    pub(crate) async fn ensure_open(&self, path: &str) -> Result<(), String> {
        let key = self.open_key(path);
        {
            let open = self.open_files.lock().map_err(|e| e.to_string())?;
            if open.contains_key(&key) {
                return Ok(());
            }
        }
        let full = resolve(&self.root, path);
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        let uri = url_for(&full)?;
        let language_id = self
            .language_id(path)
            .unwrap_or_else(|| "plaintext".to_string());
        self.socket
            .notify::<DidOpenTextDocument>(lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri,
                    language_id,
                    version: 1,
                    text,
                },
            })
            .map_err(|e| format!("didOpen failed: {e}"))?;
        self.open_files
            .lock()
            .map_err(|e| e.to_string())?
            .insert(key, 1);
        Ok(())
    }

    /// Feeds loom's recent file changes (paths) into the server: open
    /// files get full-text `didChange`, everything else a
    /// `didChangeWatchedFiles` event (§5).
    pub(crate) async fn sync(&self, changed_paths: &[String]) {
        use lsp_types::notification::{DidChangeTextDocument, DidChangeWatchedFiles};
        for path in changed_paths {
            let full = resolve(&self.root, path);
            let key = self.open_key(path);
            let is_open = self
                .open_files
                .lock()
                .map(|m| m.contains_key(&key))
                .unwrap_or(false);
            let version = if is_open {
                self.open_files
                    .lock()
                    .map(|mut m| {
                        let v = m.get(&key).copied().unwrap_or(0).saturating_add(1);
                        m.insert(key.clone(), v);
                        v
                    })
                    .unwrap_or(1)
            } else {
                0
            };
            if is_open {
                match tokio::fs::read_to_string(&full).await {
                    Ok(text) => {
                        let uri = match url_for(&full) {
                            Ok(u) => u,
                            Err(e) => {
                                tracing::warn!(server = %self.name, "{e}");
                                continue;
                            }
                        };
                        if let Err(e) = self.socket.notify::<DidChangeTextDocument>(
                            lsp_types::DidChangeTextDocumentParams {
                                text_document: lsp_types::VersionedTextDocumentIdentifier {
                                    uri,
                                    version: i32::try_from(version).unwrap_or(1),
                                },
                                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                                    range: None,
                                    range_length: None,
                                    text,
                                }],
                            },
                        ) {
                            tracing::warn!(server = %self.name, "didChange failed: {e}");
                        }
                    }
                    Err(_) => {
                        // Deleted on disk: fall through to a watched-file event.
                        if let Ok(uri) = url_for(&full) {
                            let _ = self.socket.notify::<DidChangeWatchedFiles>(
                                lsp_types::DidChangeWatchedFilesParams {
                                    changes: vec![lsp_types::FileEvent {
                                        uri,
                                        typ: lsp_types::FileChangeType::DELETED,
                                    }],
                                },
                            );
                        }
                        if let Ok(mut open) = self.open_files.lock() {
                            open.remove(&key);
                        }
                    }
                }
            } else if let Ok(uri) = url_for(&full) {
                let typ = if full.exists() {
                    lsp_types::FileChangeType::CHANGED
                } else {
                    lsp_types::FileChangeType::DELETED
                };
                if let Err(e) = self.socket.notify::<DidChangeWatchedFiles>(
                    lsp_types::DidChangeWatchedFilesParams {
                        changes: vec![lsp_types::FileEvent { uri, typ }],
                    },
                ) {
                    tracing::warn!(server = %self.name, "didChangeWatchedFiles failed: {e}");
                }
            }
        }
    }

    /// Builds an LSP position for a 1-based surface position in `path`,
    /// encoding per the negotiated position encoding.
    pub(crate) async fn surface_position(
        &self,
        path: &str,
        line: u32,
        column: Option<u32>,
    ) -> Result<lsp_types::Position, String> {
        let full = resolve(&self.root, path);
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        let text_line = line_at(&text, line);
        let col_chars = column.unwrap_or(1);
        if self.utf8_positions {
            Ok(crate::lsp::position::surface_to_utf8(line, col_chars))
        } else {
            Ok(crate::lsp::position::surface_to_utf16(
                line, col_chars, &text_line,
            ))
        }
    }

    /// Runs one typed request with the per-request wall cap.
    pub(crate) async fn request<R>(&self, params: R::Params) -> Result<R::Result, String>
    where
        R: lsp_types::request::Request,
    {
        let fut = self.socket.request::<R>(params);
        match tokio::time::timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS), fut).await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(format!("lsp request failed: {e}")),
            Err(_) => Err(format!("lsp request {} timed out", R::METHOD)),
        }
    }
}

/// Client capabilities advertised at initialize (§7): utf-8 positions,
/// the read features the tools use, configuration answered from the
/// manifest, edits declined.
fn client_capabilities() -> lsp_types::ClientCapabilities {
    use lsp_types::{
        ClientCapabilities, DiagnosticClientCapabilities, DocumentSymbolClientCapabilities,
        GeneralClientCapabilities, GotoCapability, HoverClientCapabilities, PositionEncodingKind,
        ReferenceClientCapabilities, TextDocumentClientCapabilities, WorkspaceClientCapabilities,
        WorkspaceEditClientCapabilities, WorkspaceSymbolClientCapabilities,
    };
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF8]),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(false),
            }),
            references: Some(ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![lsp_types::MarkupKind::PlainText]),
            }),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(false),
                related_document_support: Some(false),
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            apply_edit: Some(false),
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                resource_operations: Some(vec![
                    lsp_types::ResourceOperationKind::Create,
                    lsp_types::ResourceOperationKind::Rename,
                    lsp_types::ResourceOperationKind::Delete,
                ]),
                ..Default::default()
            }),
            symbol: Some(WorkspaceSymbolClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Byte sequence that begins an LSP frame header.
const HEADER_START: &[u8] = b"Content-Length:";
/// Upper bound on tolerated preamble noise before the first frame;
/// beyond this the stream is treated as broken and passed through.
const MAX_PREAMBLE: usize = 64 * 1024;

/// Finds the offset where the first LSP frame header begins: either at
/// stream start or immediately after a newline.
fn find_header_start(buf: &[u8]) -> Option<usize> {
    if buf.starts_with(HEADER_START) {
        return Some(0);
    }
    buf.windows(HEADER_START.len() + 1)
        .position(|w| w[0] == b'\n' && &w[1..] == HEADER_START)
        .map(|i| i + 1)
}

/// An [`AsyncRead`] that drops everything before the first LSP frame
/// header. Strictly framed servers emit none, but some real ones (and
/// test harnesses) print banner lines to stdout first; async-lsp's
/// parser rejects any such line, so the client skips the preamble
/// itself. Content after the first header passes through untouched.
struct PreambleSkip<R> {
    inner: R,
    /// Buffered bytes while still searching for the first header.
    scan: Vec<u8>,
    /// Bytes to emit before touching `inner` (post-header remainder).
    prefix: Vec<u8>,
    /// True once the header was found (or the scan gave up).
    done: bool,
}

impl<R> PreambleSkip<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            scan: Vec::new(),
            prefix: Vec::new(),
            done: false,
        }
    }
}

impl<R: futures::AsyncRead + Unpin> futures::AsyncRead for PreambleSkip<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        let this = self.get_mut();
        // Emit any post-header remainder first.
        if !this.prefix.is_empty() {
            let n = buf.len().min(this.prefix.len());
            buf[..n].copy_from_slice(&this.prefix[..n]);
            this.prefix.drain(..n);
            return Poll::Ready(Ok(n));
        }
        if this.done {
            return std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        // Search mode: pull small chunks until a header start appears.
        let mut scratch = [0u8; 1024];
        match std::pin::Pin::new(&mut this.inner).poll_read(cx, &mut scratch) {
            Poll::Ready(Ok(0)) => {
                // EOF without any frame: pass the EOF through.
                this.done = true;
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Ok(n)) => {
                this.scan.extend_from_slice(&scratch[..n]);
                if let Some(idx) = find_header_start(&this.scan) {
                    this.prefix = this.scan.split_off(idx);
                    this.done = true;
                } else if this.scan.len() > MAX_PREAMBLE {
                    // Give up skipping; let the parser surface the error.
                    this.prefix = std::mem::take(&mut this.scan);
                    this.done = true;
                } else {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                let n = buf.len().min(this.prefix.len());
                buf[..n].copy_from_slice(&this.prefix[..n]);
                this.prefix.drain(..n);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
}

/// Spawns one server candidate and completes the LSP handshake.
///
/// The child is sandboxed exactly like every other loom-spawned command
/// (network off unless the manifest grants it), inherits no
/// environment beyond PATH plus the manifest allowlist, and dies with
/// the session (`kill_on_drop`).
pub(crate) async fn spawn_server(
    candidate: &ServerCandidate,
    workspace: &Path,
    settings: serde_json::Value,
) -> Result<Arc<LspServer>, String> {
    let sandbox = Sandbox::resolve(
        workspace,
        SandboxGrants {
            enabled: true,
            network: candidate.network,
            extra_writable: Vec::new(),
        },
    );
    let (program, rest) = candidate
        .command
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let (wrapped_program, wrapped_args) = sandbox.wrap(program, rest);

    let mut command = tokio::process::Command::new(&wrapped_program);
    command
        .args(&wrapped_args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(&candidate.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot spawn {}: {e}", candidate.name))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "no stdin".to_string())?
        .compat_write();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout".to_string())?
        .compat();
    let stdout = PreambleSkip::new(stdout);

    let state = Arc::new(ServerState {
        diagnostics: Mutex::new(BTreeMap::new()),
        settings,
        registered: Mutex::new(Vec::new()),
    });

    let router_state = state.clone();
    let (mainloop, socket) = MainLoop::new_client(|_socket| {
        tower::ServiceBuilder::new()
            .layer(CatchUnwindBuilder::default())
            .layer(ConcurrencyBuilder::default())
            .service(build_router(router_state.clone()))
    });
    let drive = tokio::spawn(async move {
        if let Err(e) = mainloop.run_buffered(stdout, stdin).await {
            tracing::warn!("lsp mainloop ended: {e}");
        }
    });

    let root_uri = url_for(workspace)?;
    let folder_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    #[allow(deprecated)] // root_uri still reached by older servers; folders are primary
    let params = lsp_types::InitializeParams {
        capabilities: client_capabilities(),
        root_uri: Some(root_uri.clone()),
        workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
            uri: root_uri,
            name: folder_name,
        }]),
        ..Default::default()
    };

    let init = {
        let fut = socket.request::<lsp_types::request::Initialize>(params);
        match tokio::time::timeout(std::time::Duration::from_secs(INIT_TIMEOUT_SECS), fut).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => return Err(format!("{}: initialize failed: {e}", candidate.name)),
            Err(_) => return Err(format!("{}: initialize timed out", candidate.name)),
        }
    };
    socket
        .notify::<Initialized>(lsp_types::InitializedParams {})
        .map_err(|e| format!("{}: initialized notify failed: {e}", candidate.name))?;

    let utf8_positions = matches!(
        &init.capabilities.position_encoding,
        Some(encoding) if encoding == &lsp_types::PositionEncodingKind::UTF8
    );

    Ok(Arc::new(LspServer {
        name: candidate.name.clone(),
        known: candidate.known,
        capabilities: init.capabilities,
        socket,
        _child: child,
        _drive: drive,
        state,
        open_files: Mutex::new(BTreeMap::new()),
        utf8_positions,
        root: workspace.to_path_buf(),
    }))
}

/// Deterministic answers for every server-initiated request (§7).
fn build_router(state: Arc<ServerState>) -> Router<Arc<ServerState>> {
    let diagnostics_state = state.clone();
    let mut router = Router::new(state);
    router
        .request::<WorkspaceConfiguration, _>(
            |state: &mut Arc<ServerState>, params: lsp_types::ConfigurationParams| {
                let state = state.clone();
                async move {
                    // Serve the manifest settings per requested item.
                    Ok(vec![state.settings.clone(); params.items.len().max(1)])
                }
            },
        )
        .request::<RegisterCapability, _>(
            |state: &mut Arc<ServerState>, params: lsp_types::RegistrationParams| {
                let state = state.clone();
                async move {
                    for reg in &params.registrations {
                        if let Ok(mut registered) = state.registered.lock() {
                            registered.push(reg.method.clone());
                        }
                    }
                    Ok(())
                }
            },
        )
        .request::<WorkDoneProgressCreate, _>(
            |_state: &mut Arc<ServerState>,
             _params: lsp_types::WorkDoneProgressCreateParams| async move { Ok(()) },
        )
        .request::<ApplyWorkspaceEdit, _>(
            |_state: &mut Arc<ServerState>,
             _params: lsp_types::ApplyWorkspaceEditParams| async move {
                // loom declines: servers advise, loom's write tools act.
                Err(ResponseError::new(
                    ErrorCode::REQUEST_FAILED,
                    "loom declines server-initiated edits; route changes through its write tools",
                ))
            },
        )
        .notification::<PublishDiagnostics>(
            move |_state: &mut Arc<ServerState>,
                  params: lsp_types::PublishDiagnosticsParams|
                  -> std::ops::ControlFlow<Result<(), async_lsp::Error>> {
                let path = params
                    .uri
                    .to_file_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| params.uri.to_string());
                if let Ok(mut cache) = diagnostics_state.diagnostics.lock() {
                    if params.diagnostics.is_empty() {
                        cache.remove(&path);
                    } else {
                        cache.insert(path, params.diagnostics);
                    }
                }
                std::ops::ControlFlow::Continue(())
            },
        )
        .unhandled_request(|_state, req: AnyRequest| async move {
            tracing::debug!(method = %req.method, "declining unhandled lsp request");
            Err(ResponseError::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("unhandled request: {}", req.method),
            ))
        })
        .unhandled_notification(|_state, note: AnyNotification| {
            // window/logMessage, telemetry, progress: chatter -> tracing.
            // Continue (never break the loop): loom tolerates chatter.
            tracing::debug!(method = %note.method, "lsp notification");
            std::ops::ControlFlow::Continue(())
        });
    router
}

#[cfg(test)]
mod tests {
    use super::find_header_start;

    #[test]
    fn header_at_stream_start() {
        assert_eq!(find_header_start(b"Content-Length: 2\r\n\r\n{}"), Some(0));
    }

    #[test]
    fn header_after_preamble_lines() {
        let stream = b"running 1 test\ntest x ... \nContent-Length: 2\r\n\r\n{}";
        let expected = stream
            .windows(b"Content-Length:".len())
            .position(|w| w == b"Content-Length:")
            .expect("header present");
        assert_eq!(find_header_start(stream), Some(expected));
    }

    #[test]
    fn no_header_returns_none() {
        assert_eq!(find_header_start(b"just noise\nmore noise\n"), None);
    }

    #[test]
    fn mid_word_match_does_not_count() {
        assert_eq!(find_header_start(b"xContent-Length: 2"), None);
    }
}
