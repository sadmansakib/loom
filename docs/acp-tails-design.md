# ACP open tails: `session/load` and forwarded `mcp_servers`

Status: shipped 2026-11-07 (roadmap item 37, closing the two recorded tails).

## Problem

loom-acp (roadmap item 37) shipped with two deliberately absent surfaces,
recorded in the crate docs:

1. **`session/load`** — not advertised, so an editor that reconnects (or
   restarts) cannot resume a loom conversation. Every connection started
   fresh even though the engine persists every session to SQLite.
2. **Forwarded `mcp_servers`** — `session/new` accepts the field (the
   wire type has it) but loom ignored it; only `.loom/mcp.toml`
   manifest servers attach. Editors forward their own MCP server list
   (Zed, Claude Code-style clients) and expect the agent to attach it.

## Design

### Tail 1 — `session/load`

**Identity seam: the ACP session id *is* the kernel session id.**
`SessionId` (loom-kernel) wraps a UUID v7, enforced on every input path
(`FromStr`, `TryFrom<Uuid>`, `Deserialize`). The ACP id is a string.
`session/new` now mints the kernel `SessionId` first and derives the
ACP id from it (`kernel.as_uuid().to_string()`), instead of minting two
unrelated ids. Consequences:

- The id the client holds is exactly the SQLite session key, so a new
  `session/load` handler parses the ACP id string back into a `SessionId`
  (`Uuid::parse_str` + `TryFrom`, which rejects non-v7 values with an
  `invalid_request` error).
- `Engine::load_session(&id)` (the roadmap item 15 seam, already public)
  returns the persisted transcript; `run_task` continues from whatever
  it returns. Unknown id or persistence off → `Ok(None)` → the same
  `invalid_request` "unknown session" error shape `session/prompt`
  already uses for unknown sessions.
- A load of an id that is already live in this server is an
  `invalid_request` error — overwriting would orphan a live engine.
- The `initialize` response now advertises
  `AgentCapabilities::load_session = true` so compliant clients offer
  resume at all.

The ask bridge, engine config, and prompt path are reused verbatim: a
loaded session registers a `SessionCore` under the *same* ACP id, so
`session/prompt` and `session/cancel` need no changes.

### Tail 2 — forwarded `mcp_servers`

**Trust model.** The manifest is an explicit operator allowlist in the
same trust position as the command manifest. Editor-forwarded servers
have no such anchor: the trust moves to the operator who starts
`loom-acp` with the new `--allow-client-mcp` flag (default off). The
flag is the whole grant: when off, forwarded servers are ignored with a
warning; when on, they attach.

**Transport scope: stdio only.** `acp::McpServer::Stdio` maps onto
`McpServerDecl` (command argv from `command` + `args`, env from the
`env` list). `Http`/`Sse` variants answer `invalid_request` ("only
stdio MCP servers are supported"); `AgentCapabilities` keeps
`mcp_capabilities.http`/`sse` unset, which matches.

**Network.** ACP has no per-server network field, and editor servers
are typically network services. The operator grant therefore implies
network: forwarded decls get `network = true`. Everything else about
the spawn path is unchanged — the same OS sandbox, scrubbed
environment, `PATH` passthrough, and `LOOM_WORKSPACE` the manifest
servers use (`McpServerProcess::spawn`).

**No lockfile writes.** Manifest attach fails closed on an unreadable
lock and re-verifies tool-definition pins across runs. Forwarded
servers get an ephemeral in-memory lock: hygiene runs identically
(description checks, name sanitization, dedup), but pin verification is
per-connection only — there is no prior pin to verify and nothing is
written to `.loom/mcp-lock.toml`. The editor connection is not the
workspace; coupling editor config to the workspace lockfile would let a
client session mutate operator state. This trade-off is why the feature
sits behind an explicit flag.

**Seams.**

- loom-tools grows `attach_forwarded(decls, workspace, budgets)`: the
  fetch → handshake → hygiene → dedup → catalog/surface pipeline shared
  with `attach_mcp`, extracted into `attach_declared(...)` and called
  with an ephemeral lock. `attach_mcp` behavior is byte-identical (it
  keeps loading and re-writing the real lockfile).
- `Engine::open_with_extra_tools` becomes public (documented as the
  frontend seam for handing in an MCP attachment) so loom-acp can open
  the engine with the forwarded attachment. No `EngineConfig` change;
  the manifest path (`EngineConfig::mcp`) is untouched.
- loom-acp depends on nothing new: `loom_core::tools::mcp` already
  re-exports loom-tools (ADR-021 holds).

## Tests

Offline only (MONEY rule): scripted provider over `Channel::duplex`,
tempdir workspaces, and the test-binary re-entry mock MCP server.

- loom-tools `mcp_stdio.rs`: forwarded attach spawns without a manifest;
  it writes no lockfile; hygiene still applies.
- loom-acp unit: stdio servers map to decls (argv, env, network grant);
  http/sse are rejected; id strings round-trip with the v7 rule.
- loom-acp e2e: `session/load` on a second server instance sharing the
  SQLite db resumes a session persisted by the first (the resumed
  prompt's model request still contains the earlier turn); unknown and
  non-v7 ids error; forwarded servers attach and their tool executes
  end-to-end through `session/prompt` when allowed, and do not attach
  when the flag is off.

## Out of scope (recorded)

- Http/Sse MCP transports (rmcp client support does not exist in
  loom-tools yet).
- Per-server policy for forwarded servers (ACP carries no policy
  fields; the manifest keeps that surface).
- Cross-run pin verification for forwarded servers (ephemeral lock, by
  design above).
- `set_model` / `set_session_mode` (still deliberately absent).

## Consequence found while building

`session/new` and `session/load` open the engine through the async
catalog-resolving path (`open_async_with_forwarded_mcp` /
`open_async_with`), matching `loom run` — a forwarded-server attach has
to be async (spawn + handshake), and there is no sync-with-forwarded
variant to invent. Two consequences:

- A table-provider session may spend its catalog timeout on a
  network-blocked editor machine before falling back to built-in
  capability rows; the fetch failure is swallowed for non-catalog
  providers exactly as in the CLI.
- Spawning the open surfaced a latent engine bug: `resolve_catalog`'s
  future was `!Send` because an `Auth` handle (wrapping a sqlite
  connection, `!Sync`) stayed alive across the catalog fetch await —
  invisible until an embedder spawned the open on a multithread
  runtime. `catalog::load` / `load_custom` now open and drop the
  handle inside a scope that ends before the fetch, so the futures
  are `Send`.
