# ACP server mode design (roadmap item 37)

Loom as an external agent for ACP clients (Zed today, JetBrains and
others shipping): one new binary, `loom-acp`, that speaks the Agent
Client Protocol over stdio and drives the same engine `loom run`
drives. Nothing about the existing CLI changes.

## The stdout question, answered first

The premise to check: "should the server replace stdout with the
protocol?" It is not a design preference — the transport spec makes it
mandatory. From the ACP transports page (agentclientprotocol.com,
verified 2026-11-07):

- stdio is the only defined transport (a Streamable-HTTP remote
  transport exists only as an RFD draft). The client launches the
  agent as a subprocess; the agent reads JSON-RPC from stdin and
  writes JSON-RPC to stdout, one message per `\n`-terminated line.
- "The agent **MUST NOT** write anything to its `stdout` that is not
  a valid ACP message." One stray byte breaks the client's line
  parser — Zed shows a protocol error and kills the thread.
- stderr is the sanctioned log channel ("the agent MAY write UTF-8
  strings to its standard error for logging purposes").

So the real design rule is scoped: stdout belongs to the protocol
**in the `loom-acp` process only**, and the engineering problem is
guaranteeing that nothing else in the process can write to stdout.
Loom's existing discipline makes this cheap (verified in-tree):

- tracing already goes to stderr (`loom-cli/src/main.rs`: "stderr
  tracing subscriber … keep stdout for the result");
- stdout today carries only the result stream (assistant text or
  NDJSON events, `output.rs`) — and in server mode that stream is not
  printed at all, it becomes `session/update` notifications on the
  protocol channel;
- tool subprocess stdout is captured in-process by `run_command`
  (never inherited), so a `cargo build` cannot leak into our stdout;
- panics print to stderr.

`loom run`'s stdout behavior is untouched: the contract applies to
the new binary alone.

## Verified facts (2026-11-07)

| Fact | Source |
| --- | --- |
| Transport: newline-delimited JSON-RPC 2.0 over stdio; stdout protocol-only; stderr for logs | agentclientprotocol.com/protocol/v1/transports |
| Protocol v1 is stable; v2 is draft (`unstable_protocol_v2`) | crate docs, docs.rs |
| Crate `agent-client-protocol` 5.0.0 (2026-11-02), Apache-2.0, official org `agentclientprotocol/rust-sdk`, ~6M downloads, semver-stable v1 | crates.io API |
| Normal deps: own `derive` + `schema` crates, anyhow, futures, futures-concurrency, rustc-hash, serde, serde_json, tracing — all already in loom's graph except the crate's own two (G8 acceptable: they ARE the typed protocol) | crates.io deps API |
| No default features; `stdio` feature provides the `Stdio` transport for the agent side; `process` is client-side (unneeded) | docs.rs |
| Agent shape: `Agent.v1().name(..).on_receive_request(handler, on_receive_request!()).connect_to(Stdio::new())` | SDK example `simple_agent.rs` |
| Zed config: `agent_servers: { "Loom": { type: "custom", command, args, env } }`; Zed may forward its MCP servers over ACP; `dev: open acp logs` for debugging | zed.dev/docs/ai/external-agents |
| Engine seams: `Engine::open_with(config, EngineOverrides { provider: Some(..), .. })` (an overridden provider skips credential build — the offline-test seam), `run_task(&session, input, &dyn EventSink, &TaskControl) -> TaskSummary`, `TaskControl::{cancel, pause, steer}` (clones share state) | crates/loom-engine/src/engine.rs, loom-kernel/src/agent/mod.rs |
| Ask seam: `AskResolver = Arc<dyn Fn(&str) -> BoxFuture<'static, bool> + Send + Sync>`; engine default none → `Ask` fails closed | crates/loom-tools/src/policy.rs |

## Architecture

New sibling crate `crates/loom-acp`, one binary (`loom-acp`). Deps:
`loom-core` (facade — same rule as the CLI), `agent-client-protocol`
(features `stdio`), `tokio`, `serde_json`, `tracing`/`tracing-subscriber`
(stderr). ADR-021 is untouched: `loom-cli` still depends on
`loom-core` only, so the facade test stays green — the ACP crate IS
the cross-process frontend `architecture.md` anticipated ("the seam a
future server process would cut along already exists in draft form").

Process model: one `loom-acp` process per editor connection; one
`Engine` per ACP `session/new` (workspace = the request's `cwd`);
one kernel `Session` + `TaskControl` per ACP session id, held in a
map. `prompt` runs `run_task` to completion and answers with the
stop reason; `cancel` fires `TaskControl::cancel`. Sessions persist
through the engine's store, so an ACP session survives process
restarts the same way CLI sessions do (session/load maps later, if a
client needs it).

### stdout purity, enforced

- The binary installs a tracing subscriber on stderr (RUST_LOG,
  default `warn`) — mirroring `loom-cli`.
- No renderer is constructed; the code path that prints to stdout in
  the CLI does not exist here. The only `write` to stdout is the
  SDK's `Stdio` transport.
- Tests assert the property end to end: a test spawns the built
  `loom-acp` binary, performs the `initialize` handshake over real
  pipes, and asserts **every byte on stdout is a valid JSON-RPC
  line** (and that nothing arrives on stderr at default log level).

### Event → session/update mapping

The engine's `EventSink` is implemented by a bridge that forwards
`EventKind` to `session/update` notifications. The mapping is total
and explicit:

| Kernel event | ACP `SessionUpdate` |
| --- | --- |
| `TextDelta` | `agent_message_chunk` (text content block) |
| `ReasoningDelta` | `agent_thought_chunk` |
| `ToolStarted` | `tool_call` (id = call id, title = tool name, kind per table below, raw JSON content block) |
| `ToolCompleted` | `tool_call_update` (state `completed`/`failed`, content head as text block) |
| `TodoUpdated` | `plan` (entries in list order; loom status → pending/in-progress/completed) |
| — (prompt end) | prompt **response** `stop_reason`, not an update |

Tool-kind table (best effort, `other` as fallback): `read_file` /
`list_dir` / `artifact_read` → `read`; `search` → `search`;
`write_file` / `edit_file` / `replace_symbol` → `edit` (ACP v1 has no
`create` kind); `run_command` / `verify` → `execute`; `todo` →
`think`; everything else → `other`.

### The ask bridge seam (discovered during the build)

The engine's built-in registry constructed `run_command` with no ask
resolver, so an `Ask` verdict failed closed with "missing frontend" —
there was no way for any embedder to become the approver. The build
adds one seam: `EngineOverrides.ask: Option<AskResolver>` (builder
`with_ask_resolver`), threaded by `build_registry` into
`RunCommandTool::with_ask_resolver`. `None` keeps the fail-closed
default byte-identical; `loom-acp` supplies a resolver that sends the
client `session/request_permission` (one allow, two rejects) and
treats only an explicit allow as approval — deny, prompt-turn
cancellation, and transport failure all fail closed, which is exactly
the engine's own no-resolver behavior.

### Handlers must not block the dispatch loop (discovered during the build)

The SDK processes every inbound message on ONE async task: while a
handler future is pending, no further messages are read. Awaiting the
engine run inside the `session/prompt` handler therefore starves
`session/cancel` — the cancel arrives and nothing dispatches it (the
first test run deadlocked exactly there). The contract
(`jsonrpc.rs`: "Handlers Must Not Block"): long work runs via
`cx.spawn(...)`, which registers a connection task and returns
immediately. `session/prompt` and `session/new` both spawn their
handlers this way and respond from inside the task; `initialize` and
`session/cancel` stay inline (trivial).

### Feature unification flips serde_json map order (discovered during the build)

`agent-client-protocol` enables serde_json's `preserve_order`
feature. Cargo unifies features across everything one invocation
builds, so any `--workspace` build — tests, or a workspace-wide
release build — had silently switched loom-kernel's `serde_json`
maps from sorted key order to insertion order. The failure ledger's
own doc comment had pinned this as a forbidden state ("the
`preserve_order` feature must stay off anywhere in the workspace"),
and the first full-suite run failed exactly there: the canonical
call-identity keys for repeat refusal and analytics repeat stats
became insertion-order-sensitive. The fix makes canonicalization
order-independent (`loom-kernel::canonical::canonical_json`, keys
byte-sorted recursively): byte-identical in default (sorted-map)
builds — so SWE-bench paths and `cargo build -p loom-cli` outputs
are unchanged — and now immune to whatever any future dependency
does to serde_json features.

Deliberately **not** mapped (internal bookkeeping with no editor
rendering; the honest minimal surface): `TurnStarted`,
`ContextPruned`, `FastPathHit`/`FastPathDeclined`,
`CapabilitiesResolved`, `HarnessNotice`, `ContextHandoff`,
`ModelRequestStarted`, `ModelRetry`, `ModelCompleted`,
`AutofixApplied`, `SteeringApplied`, `TurnBudgetExtended`,
`LoopNudge`, `TaskFailed` (the stop reason carries the outcome),
`AwaitingInput` (the prompt response boundary). The table lives in
code comments next to the mapping so drift is caught in review.

Stop reasons: `TaskSummary` end `end_turn` → `end_turn`; task error
`Cancelled` → `cancelled`; `MaxTurns`/budget exhaustion →
`max_tokens`; `Paused` → `end_turn` (the turn committed; the client
sees a clean stop); anything else (model error, store failure) →
`error`.

### Ask → request_permission

The crate installs an `AskResolver` that sends ACP
`session/request_permission` (options: one `command` option per
asked program, kind `execute`) and maps the client's
`PermissionUpdate::outcome` — allow → `true`, deny (once or always)
→ `false`. A client that never answers resolves to `false` when the
connection drops (fail closed, matching the engine default). This
keeps policy asks meaningful in an editor: the approval button in
Zed is the ask.

### Model and configuration

The server selects provider/model exactly like `loom run` defaults:
`LOOM_PROVIDER`/`LOOM_MODEL` envs with CLI-flag overrides
(`--provider`, `--model`, `--base-url`), auth via the same account
store/env order (ADR-009). No editor-side model config: ACP's
optional `set_model`/`set_session_mode` requests stay unimplemented
(the SDK's default handler answers method-not-found), on the
protocol's own "agents own their model" guidance.

## Tests (offline; no network, no editor)

1. **Mapping unit tests**: fixture `EventKind`s → expected
   `SessionUpdate` JSON shapes (message chunk, thought chunk, tool
   call with kind table rows, tool call update, plan).
2. **In-process end-to-end**: the server handlers driven over the
   SDK's runtime-neutral `Channel` transport (no stdio): initialize →
   new_session → prompt with a scripted `ModelProvider` injected via
   `EngineOverrides` → assert the ordered notification stream and the
   final `end_turn` response. A second case with a tool-writing
   script asserts `tool_call` → `tool_call_update` ordering.
3. **Ask bridge test**: an `Ask` policy verdict surfaces as
   `session/request_permission`; allow → command runs, deny → the
   model sees the refusal.
4. **Cancel test**: `cancel` mid-scripted-stream → `cancelled` stop
   reason, session survives (second prompt works).
5. **Stdio smoke test**: spawn the real binary, initialize over
   pipes, assert stdout purity (all lines valid JSON-RPC, no stray
   bytes) and clean shutdown on stdin close. Marks the binary as
   built via `env!("CARGO_BIN_EXE_loom-acp")`.

## Residual risks

- The SDK is v5 with a young (2026-11-02) major; pin exact minor in
  Cargo.toml and treat upgrades as review-worthy.
- `session/request_permission` requires a cooperating client; fail
  closed covers the rest.
- Zed's MCP forwarding arrives as `session/new` `mcp_servers`; v1
  scope: acknowledged but not attached (loom has its own MCP story,
  slice D) — recorded as an open tail, not silently ignored in
  design docs.
