# LSP integration design (slice E)

Research-pinned design for language-server integration, mirroring the
MCP process: evidence first, design second, implementation gated on it.
Neovim is the gold standard per the product mandate — but the honest
reading of "neovim-grade" is **breadth through a generic client plus a
config table, not per-language code**. That is exactly how nvim-lspconfig
(≈380 server configs) and mason.nvim (auto-discover/auto-enable) work.

## Evidence

### Prior art: agents on LSP

- **Serena** (oraios/serena, 22k+ stars, MCP server over LSP): the most
  mature LSP-for-agents system. Key lessons, verified from source review
  (pantheon agentic-context analysis):
  - Symbol **name-paths, not line numbers** — robust to line-shift
    between retrieval and edit; slashes (`Class/method`) survive
    intervening edits that invalidate coordinates.
  - Retrieval set: find_symbol, symbols_overview, find_referencing_symbols,
    find_declaration, find_implementations, diagnostics. Editing set:
    replace_symbol_body, insert_after/before, safe_delete, rename_symbol.
  - **Multi-stage result shortening** (references → no-context list →
    per-file counts → total count) — the token-economy trick that keeps
    large result sets affordable.
  - Two-tier symbol caching (content-hash keyed), server auto-restart on
    death, 240 s per-task timeout. 40+ languages via a generic LSP layer
    (SolidLSP); "LSP server quality is the ceiling" — accuracy is bounded
    by the underlying server.
  - v2 REPL lesson (compose operations programmatically) noted; loom's
    answer is the deterministic loop, not a REPL — recorded, not adopted.
- **Helix editor** uses `async-lsp` (oxalica): tower-layered async client,
  `MainLoop` + `ServerSocket` with typed request methods, middleware
  (concurrency, catch-unwind, router). Proven at editor scale.

### Protocol surface (LSP 3.17) — agent-relevant filter of the neovim set

Take (evidence: roadmap iter-6 "fewer exploratory model turns"; Serena's
default-enabled set; LazyVim keymaps `gd/gr/gI/gy/gD/K/ cr`):

| Capability | LSP method | Agent value |
|---|---|---|
| Definition / declaration | `textDocument/definition`, `/declaration` | skip file reads |
| Type definition / implementation | `/typeDefinition`, `/implementation` | trait/impl navigation |
| References | `textDocument/references` | blast-radius without grep |
| Document symbols | `textDocument/documentSymbol` | structure without reading |
| Workspace symbols | `workspace/symbol` | cross-repo symbol search |
| Hover | `textDocument/hover` | types + docs, no file read |
| Diagnostics | `textDocument/diagnostic` (pull) else `publishDiagnostics` (push cache) | errors without running the build |
| Rename | `textDocument/prepareRename` + `rename` → `WorkspaceEdit` | correct multi-file rename |
| Call hierarchy | `callHierarchy/prepare` + `incoming`/`outgoing` | callers/callees |

Skip v1, with reasons on record: completion / signatureHelp / inlayHints /
codeLens / folding / semanticTokens / documentHighlight (editor-interaction
surface, no agent value — each still costs capability negotiation and
handler surface); **codeAction + `executeCommand`** (server-chosen
arbitrary command execution = the MCP sampling/puppet class; revisit only
with an allowlist design); `willSaveWaitUntil`; formatting (E2 candidate —
useful but edit-shaped, ships with rename's edit-apply machinery).

### Token economics

- Fixed tool count regardless of server count (~8 tools) — unlike MCP
  there is **no schema explosion**, so no deferral machinery needed.
- Result bigness is the risk, not tool count: references and hover can be
  huge. Adopt Serena's shortening: locations rendered `path:line` (1-based),
  per-file counts when over budget, budget head caps everywhere
  (`ToolBudgets`, env-sweepable like every other budget).
- Provenance labeling `[lsp:<server>]` on every result — same
  results-are-data rule as MCP.

### Security model

- Language servers are long-lived arbitrary processes with deep workspace
  access: same trust position as MCP servers. Reuse the manifest pattern —
  `.loom/lsp.toml`, `[servers.<name>] {command, env, network=false
  default}` — explicit declaration is approval; no dynamic install, no
  registry browsing (loom never auto-downloads servers; the install story
  is the system package manager, like `cargo install rust-analyzer`).
- Spawn through the existing sandbox (Seatbelt/bwrap, net-off default,
  kill-on-drop, no env inheritance). Known wrinkle: servers spawn
  grandchildren (rust-analyzer runs `cargo check` for flycheck; gopls runs
  `go list`). Grandchildren inherit the sandbox profile; the E3 probe must
  verify flycheck works confined before we claim it. Honest fallback:
  manifest `network = true` / relaxed writable grants per declared server,
  same as commands.toml escalation.
- Injection: hover/docs/diagnostics text is untrusted content (the MCP
  Tool-Poisoning class applies to *results* here, not tool schemas — LSP
  servers define no agent-visible tool descriptions). Mitigation:
  `[lsp:*]` provenance, head caps, loop gates unchanged.
- Supply chain: no hash-pinning analog needed (no agent-visible schema
  surface), but the binary identity is pinned by manifest command + PATH.

## Design

### Breadth: built-in server table + manifest override (the nvim-lspconfig move)

`crates/loom-tools/src/lsp.rs` ships a `KNOWN_SERVERS` table (start:
rust_analyzer→`rust-analyzer`, gopls, clangd, pyright,
typescript_language_server→`typescript-language-server --stdio`, zls,
lua_language_server, bash_language_server, gopls-alikes; grow over time —
data, not code). Auto-attach rule: server applies when (a) `--lsp` given,
(b) its binary resolves on PATH, (c) the workspace contains files in its
language set. `.loom/lsp.toml` overrides/extends entries and adds servers;
explicit manifest entries always win over the table. Missing binary =
skip with a warning, never fail the run (fail-open per server, fail-closed
only for invalid manifest — same semantics as MCP).

### Client core

- Crates: `lsp-types` (typed protocol) + `async-lsp` 0.2 (tower client,
  helix-proven; `MainLoop::new_client` + `Router` + Concurrency/CatchUnwind
  layers, `ServerSocket` typed methods, `run_buffered` over
  `tokio_util::compat` streams).
- One process per attached server via tokio::process + `Sandbox::wrap`,
  kill_on_drop, per-request timeout (ToolContext timeout as wall bound,
  like MCP).
- Initialize: rootUri = workspace, workspaceFolders, client capabilities
  declaring only what we do — lazy didOpen/didChange/didClose (full-sync),
  `didChangeWatchedFiles` **without** dynamic registration (static
  capability; we send notifications from engine-reported changes),
  `positionEncoding: ["utf-8", "utf-16"]` preference, hover/references/
  documentSymbol/workspaceSymbol/prepareRename/rename/definition-family/
  callHierarchy/pullDiagnostics as *consumed* capabilities.
- Server→client requests routed and answered deterministically:
  `workspace/configuration` → manifest settings for that server;
  `client/registerCapability` → accept and record (watchers registered
  dynamically are satisfied from the same change feed);
  `window/workDoneProgress/create` → accept; `workspace/applyEdit` →
  decline with a typed error (rename/format flow applies edits itself);
  `window/*` notifications → tracing only. No server-initiated anything
  else (same posture as MCP: no puppet surface).

### Document sync (loom edits on disk; servers must see it)

- Lazy `didOpen`: before the first position query on a file, open it with
  current disk content, `version` tracked monotonically per session.
- Engine edit feed: tools receive `ToolContext::recent_changes`; on each
  LSP tool call, changed files are flushed — `didChange` (full text) for
  open files, `workspace/didChangeWatchedFiles` for the rest. Deletion →
  `DidChangeWatchedFiles` Deleted. Deterministic, no fs watcher threads.
- Diagnostics: prefer pull (`textDocument/diagnostic`); else serve the
  cached `publishDiagnostics` stream. Servers analyze asynchronously — the
  diagnostics tool bounds its wait (short poll with timeout) and reports
  `unresolved/settling` honestly rather than returning stale-as-fresh.

### Positions (the classic LSP trap)

LSP positions are `line` + UTF-16 code units by default (UTF-8 when
negotiated). Tool input is model-friendly and drift-tolerant: 1-based
`line`, optional 1-based `column` (characters), **or** a `symbol` substring
— when `column` is omitted the tool locates `symbol` in that line itself.
Conversion (char ↔ utf-8/utf-16) is explicit, tested with multibyte
fixtures (é, 中). Output rendered as `path:line:col`, 1-based.

### Tools (E1 read surface; fixed count, no deferral)

`lsp_goto` (target: definition|type_definition|implementation|declaration),
`lsp_references`, `lsp_symbols` (file outline, flattened with kinds),
`lsp_workspace_symbols`, `lsp_hover`, `lsp_diagnostics` (file or workspace),
plus E2 write surface: `lsp_rename` (prepareRename → rename → apply
WorkspaceEdit through loom's file-write machinery — documentChanges with
Create/Delete/RenameFile, edits applied bottom-up; effects recorded like
edit_file: files_written, notices) and `lsp_format`. E3:
`lsp_callers`/`lsp_callees`. Engine wiring mirrors MCP: `--lsp` opt-in
flag, attach in `open_async_with`, per-server fail-soft, facts-block menu
(`lsp_menu` KnownFacts slot, ~10 tokens: active servers + languages).

### Phasing + measurement

- **E1**: client core + sync + read tools + manifest/table + sandbox +
  facts menu. Tests: in-repo mock LSP server (re-exec test binary, like
  the MCP mock) covering handshake, sync-after-edit, position conversion,
  shortening; real-server smoke (rust-analyzer, gopls, clangd — present on
  this machine) behind an env flag, skipped where servers absent.
- **E2**: rename + format (WorkspaceEdit application; write-shaped —
  notices + effects; policy verdicts unchanged: they are loom tools).
- **E3**: call hierarchy + the probe. $0, nano gateway, rust fixture with
  a rename/callers-heavy task; arms: `--lsp` vs not. Bar: equal or better
  task success with fewer or equal exploratory turns (grep/read_file
  counts from schema-11 artifacts) and no regressions; logged in
  `docs/daily-driver.md` under the standing honesty rules. Sandbox
  grandchild (flycheck) verification is a gate for claiming diagnostics
  under confinement; if it fails, record and ship diagnostics as
  unsandboxed-by-manifest-declaration only.

## Honest residual risks

- Server quality is the ceiling (Serena's lesson) — loom's tools are only
  as correct as the attached server; mock tests pin our behavior, not the
  servers'.
- Stale-analysis windows after edits are inherent to async servers;
  bounded-wait + settling labels mitigate, never eliminate.
- Grandchild sandbox behavior (cargo/go subprocesses under Seatbelt) is
  unverified until the E3 probe; do not claim confined flycheck before it.
- Breadth claim is honest only as "generic client + config table +
  auto-discovery": ≈380 nvim-lspconfig servers exist; loom starts with a
  small table and grows it as data.
