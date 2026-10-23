# MCP integration design (slice D)

Research-grounded design for attaching Model Context Protocol servers to
loom. Two constraints dominate every other decision: tool definitions are
the largest token sink in agentic contexts, and MCP servers are untrusted
code speaking with a channel the model treats as instructions.

Evidence base (papers + industry measurements, collected 2026-10-23):

- Token tax: a five-server setup (GitHub/Slack/Sentry/Grafana/Splunk) is 58
  tools ≈ **55K tokens before the first user message**; Anthropic has
  observed **134K tokens** of tool definitions; opencode users measured a
  single `lark-mcp-docx` adding **+147K tokens**; 50+ tools ≈ 67K tokens
  (Anthropic engineering blog 2025-11, opencode issues #8277/#17482).
- Selection accuracy degrades past **30–50 tools**; deferred search-loading
  raised Opus 4.5 tool-selection accuracy **79.5% → 88.1%** while cutting
  tool tokens ~85% (search tool itself ≈ 500 tokens) — Anthropic Tool
  Search Tool. RAG-MCP (arXiv 2505.03275) independently shows retrieval
  over a tool index cutting prompt tokens >50% and tripling selection
  accuracy (43.1% vs 13.6% baseline).
- Security: MCPLIB (arXiv 2508.12538) catalogs 31 attacks; MCPSecBench
  (arXiv 2508.13220) finds **protocol-side attacks (schema inconsistency,
  DNS rebinding, MITM) succeed 100%** on every major client, and existing
  defenses block <30%. Tool poisoning, rug pulls, and shadowing (Invariant
  Labs, Apr 2025; arXiv 2506.02040) exfiltrate data via descriptions the
  user never sees; 5.5% of 1,899 sampled servers carry poisoning
  (arXiv 2506.13538). Parasitic Toolchain attacks (arXiv 2509.06572) trace
  the root cause to **no context-tool isolation and no least-privilege
  enforcement**. CVE-2025-54136 confirms production tool-approval does not
  survive server-side redefinition.

## Design

### 1. Transport: stdio only, v1

Local subprocess servers over stdio. No HTTP/SSE/Streamable-HTTP transport.
Reason: the 100%-ASR protocol class (rebinding, MITM) is transport-level;
stdio inherits loom's existing process sandbox instead of opening a network
endpoint. Remote servers can be reached later via an explicit
user-declared proxy command — still a sandboxed local process.

### 2. Manifest allowlist, no dynamic install

Servers are declared in `.loom/mcp.toml` (same trust position as
`commands.toml`):

```toml
[servers.github]
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "from-keychain" }   # explicit, no env inheritance
network = false                             # off by default; grant per server
always_load = []                            # tool names exempt from deferral

[servers.github.policy]
unknown = "deny"                            # verdict layer per server
deny = ["delete_*"]                         # glob blocklist on tool names
```

No registry browsing, no `loom mcp add <url>`, no auto-discovery. The
upload→download→attack supply-chain path (arXiv 2506.02040) requires a
manual edit of a reviewed file. Undeclared servers never spawn.

### 3. Ingestion hygiene — descriptions are untrusted input

At `tools/list` time, deterministic checks (no model, no network):

1. **Invisible-character strip**: remove control, bidi, and zero-width
   code points; collapse runs of whitespace (the WhatsApp exfil hid
   payloads past invisible scrollbars — trailing-space padding is data).
2. **Length caps**: description ≤ 512 chars, schema ≤ 4 KiB per tool,
   ≤ 64 tools per server. Over-budget = truncate + warn, not silently
   pass.
3. **Cross-server reference detection**: a description naming another
   server's tools or `mcp_*` meta-tools is rejected (shadowing attacks;
   mcp-san E002 class). Self-contained tools only.
4. **Imperative-content heuristic**: descriptions that address the agent
   ("ignore previous", "you must", "before/after calling X, do Y") are
   flagged; per verdict policy the tool is denied or loaded with a
   visible warning. Fail closed.
5. **Hash pinning**: SHA-256 over each tool's name+description+schema,
   recorded in `.loom/mcp.lock` on first approval. Any later mismatch
   (including via `list_changed` notifications) = server quarantined
   until the user re-approves. This is the rug-pull/sleeper defense
   (Invariant, CVE-2025-54136).

### 4. Token model — every MCP tool deferred by default

Loom principle 2 applied literally: the model never sees a tool schema it
did not ask for.

- **Facts-block index** (skills pattern): server name, tool names, one-line
  descriptions, ≤ 64 entries total — measured ~150–500 tokens for the
  whole catalog.
- **`mcp_search_tools(query)`**: deterministic scoring (word/subsequence
  match over names + descriptions, ranked; no embeddings, no model
  invocation). Returns tool references with full descriptions for the
  top matches.
- **`mcp_load_tools(server, [tools])`**: registers the full schemas into
  the session tool surface; callable from the next turn.
- **Offboarding**: a loaded tool is evicted after K turns without use
  (default 8, tunable in `ToolBudgets`) — mirroring turn-age masking.
  `always_load` in the manifest pins exceptions (Anthropic keeps 3–5
  core tools loaded; loom's core tools are native, so the default pin
  set is empty).
- **Cache discipline**: the session tool surface is emitted in a stable,
  append-only sorted order so loading a tool invalidates the cache prefix
  only at the insertion point, never rewrites earlier context.

Expected effect for the five-server example: ~55K → ~0.5K upfront + ~3K
per loaded cluster, matching Anthropic's measured 85%+ reduction with the
same accuracy win.

### 5. Runtime isolation — least privilege per server

- Server processes spawn through the existing sandboxed exec path
  (Seatbelt/bwrap): network off unless granted, workspace-only writable,
  process-group kill, no env inheritance.
- Per-server budgets extend `ToolBudgets`: call ceiling per task, result
  head bytes (existing tool-output budget machinery), server wall-time.
- **No sampling, no elicitation, no server-initiated requests in v1.**
  The server cannot make loom prompt the user or spend a model invocation
  (puppet-attack surface). `list_changed` is handled as a re-verify (see
  hash pinning), nothing else.
- Tool results enter the transcript as data with a provenance label
  (`[mcp:github]`), under the same head-truncation and credential-scrub
  rules as every other tool output.

### 6. Residual risk, stated honestly

Indirect injection through tool *results* remains possible (MCP-UPD,
arXiv 2509.06572): no client today isolates context per tool origin, and
loom will not claim to either. Mitigations in v1: provenance labeling,
budgets, cross-server reference rejection, the completion gate (verdicts
come from tests, never model claims), and deny/ask verdicts on
write-shaped tools. Anything stronger (per-server context partitioning)
is a measured follow-up, not a v1 promise.

## Measurement (before merge is called done)

$0 probe, daily-driver style: a mock MCP server shaped like the worst
case (35 GitHub-like tools, realistic schemas) plus the fixture tasks.
Record: upfront tool tokens eager vs deferred, turns spent on
search/load, task outcomes. Success = same task success at ≥ 10× less
upfront tool-token cost, no search-deadlocks. Logged in
`docs/daily-driver.md` under the same honesty rules.

## Phasing

1. **D1 — protocol + hygiene**: stdio client (initialize, tools/list,
   tools/call), manifest, ingestion checks, lockfile pinning, tests with
   an in-repo mock server. No engine changes yet (tools attach but are
   eager). Fails closed everywhere.
2. **D2 — deferred surface**: facts index, `mcp_search_tools` /
   `mcp_load_tools`, session tool-surface mutation between turns, idle
   eviction, cache-stable ordering. Defaults byte-identical without
   `--mcp`.
3. **D3 — probe + budgets**: mock heavy server, token/turn measurements,
   per-server budget knobs, CHANGELOG + run log entries.

Each phase is independently shippable; D2 is the one that changes the
engine's static-registry assumption and gets the careful tests.
