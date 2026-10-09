# Loom Architecture

Loom is a coding-agent runtime written in Rust. It drives a large language
model through a turn-based loop, gives the model a bounded,
confinement-enforced toolkit for reading and editing a workspace, verifies
its own edits by running builds and tests, and persists every session to a
local SQLite database. Everything runs locally; the only network traffic is
the model provider API.

This document describes the system as built: crate layout, runtime model,
the agent loop, context compilation and pruning, the tool layer, persistence,
credentials, and the evaluation harness. It is the authoritative overview;
the code is the source of truth for details.

---

## 1. Workspace layout

Seven crates in one Cargo workspace, edition 2024, `unsafe_code = "forbid"`
in the crates that matter most. Dependencies point strictly downward — the
kernel never links a filesystem store or a CLI, the engine never prints.

| Crate | Role | Depends on |
|---|---|---|
| `loom-core` | Facade re-exporting the public surface for embedders | engine, kernel, providers, tools |
| `loom-kernel` | The agent itself: loop, context compilation, session/transcript model, tool abstraction, tool cache, artifacts, events, budgets | no workspace crates — it defines the side-effect traits (`ModelProvider`, `SessionStore`, `ArtifactStore`, `Tool`, …) itself |
| `loom-engine` | Composition root: builds a configured `Agent` from paths, auth, a provider factory, tool registry, and stores; repository facts scanner | kernel, store, providers, tools |
| `loom-providers` | Model provider adapters (DeepSeek, OpenRouter over one shared OpenAI-compatible wire; Anthropic on its own messages-API driver) | kernel model types |
| `loom-store` | SQLite persistence: sessions, turns, tasks, artifacts, auth accounts | kernel store traits |
| `loom-tools` | The shipped tool suite (read/search/edit/write/list/run/verify + artifact read) | kernel tool traits |
| `loom-cli` | Binary: argument parsing, output rendering, run/eval/auth commands | engine |

The kernel defines traits for everything with side effects
(`SessionStore`, `ArtifactStore`, `ModelProvider`, `FactsSource`,
`ContextCompiler`, `Tool`), with in-memory defaults for tests. `loom-store`
and `loom-tools` supply the production implementations; the engine wires
them together. This keeps the agent loop unit-testable with replayed model
responses and memory stores — the eval harness depends on exactly that
property.

---

## 2. Runtime model

- **Tokio, multi-threaded runtime**, owned by the CLI. The CLI's consumer
  thread owns stdout/stderr and receives rendered events via a channel, so
  engine code never touches the terminal. Shutdown uses
  `shutdown_timeout(5s)`; Ctrl-C cancels the task's cancellation token and
  the process exits 130 after the turn in flight is committed.
- **Cancellation** flows through `CancellationToken`s, one per task. Every
  long-running await — model requests, tool calls, subprocess pipes, facts
  scan — is selected against the token. In-flight work is never torn down
  mid-commit: the loop commits what completed, appends explicit
  `cancelled` fillers for what did not, and only then reports
  `TaskError::Cancelled`.
- **Blocking discipline.** Filesystem walks, SHA-256 hashing, git
  subprocesses, and file reads/writes in tools run inside
  `tokio::task::spawn_blocking`. The two deliberate exceptions are the
  SQLite store and the auth store, which use synchronous `rusqlite` on a
  mutex-guarded connection — they are called at turn boundaries (persist
  once per turn) where the cost is bounded and the transactionality is
  worth more than async purity. This is documented at each call site.
- **Sandboxed subprocesses for model-driven work.** The subprocesses Loom
  launches for tools are `git` (repository facts, with
  `-c core.quotepath=false`), `run_command` programs (§6.8) — each wrapped
  in an OS sandbox, where the model *may* run a shell (`bash -c …`)
  that inherits the confinement — and the fixed per-ecosystem commands
  of `verify` (§6.2), which never come from model input. The one
  unsandboxed `sh -c` is the user-configured credential command (§8),
  never model-reachable. `run_command` program names must be bare — a
  path-qualified name is rejected before any process exists. Every child
  has provider credential variables scrubbed from its environment.
  Children are spawned with `kill_on_drop(true)` in their own process
  group; a timeout kills the whole group, so grandchildren (`rustc` under
  `cargo`) cannot outlive the kill.

---

## 3. Sessions, tasks, and the transcript

A **Session** is an append-only transcript of items plus task records. All
identifiers (`SessionId`, `TaskId`, `ArtifactId`) are UUIDv7 — time-ordered,
non-guessable, enforced on every input path (parse, convert, deserialize),
with no `Default` so identity is never silently minted.

Transcript items (`loom-kernel/src/transcript.rs`) are a kind-tagged serde
enum:

- `User` — prompt text.
- `Assistant` — content, optional byte-exact `reasoning`, tool calls, stop
  reason. Rejected at construction if it pairs tool calls with an
  end-turn stop reason.
- `ToolResult` — call id, status (`Success` / `Error(failure kind)`),
  content, truncation flag, Loom-side `effects`, optional artifact ref.

Invariants the session layer enforces (and tests):

1. **No dangling calls.** A turn is built through `plan_turn`, which
   validates call↔result pairing 1:1 and in order *before anything is
   appended*. A mismatch (unanswered call, surplus result) aborts the
   whole turn — the transcript is never left half-written, and the error
   names the first offending item.
2. **Append-only.** Nothing rewrites history; pruning and projection are
   per-request views, never mutations (§5).
3. **First ending wins.** A task record's terminal status is a distinct
   type; a second finish attempt cannot overwrite the recorded end reason.

**Events** (`event.rs`) are the observation stream: `EventEnvelope`
(`schema_version: 2`, per-sink monotonically increasing `sequence`,
snake_case `type` tag). Version 2 adds `Usage.cost_usd`, the
provider-reported charge; it is `serde(default)`, so version-1 payloads
still deserialize. A lock spans numbering and send, so arrival order
equals sequence order even with concurrent emitters. The channel sink is
unbounded — a slow consumer queues rather than back-pressures the agent;
only a closed receiver drops (counted, warned once). `StrictSink` wraps a
sink and cancels the task on first drop; the eval harness requires zero
 dropped events.

---

## 4. The agent loop

`Agent::run_task` (kernel `agent/mod.rs`) is the whole game. Per task:

1. **Setup**: resolve capabilities once (unknown model fails before the
   first request), derive the context budget from the window, scan
   repository facts **once** on the blocking pool (§5.2).
2. **Turn loop** (`1..=max_turns`), each turn:
   a. Check the task budget envelope (deterministic order: total → input →
      output → tool calls → wall time). The unit in flight when a ceiling
      is crossed still completes and stays committed.
   b. Compile the model request from durable state (§5).
   c. Send it through the provider with a retry policy; transport/server
      errors are retryable, auth and request errors are not.
   d. Validate the stop-reason protocol: `EndTurn` must carry no tool
      calls, `ToolCalls` must carry at least one. Violations are
      `Protocol` errors — nothing is committed for that turn.
   e. Execute the tool batch (§6), then **commit** the turn.
3. **Persist-then-commit**: every mutation goes to the store *before* it is
   applied in memory. `ensure_session`, item append, task upsert, and turn
   append all happen first; a store failure surfaces as
   `TaskError::Persistence` and leaves the in-memory session untouched.
   A crash between the two leaves the database ahead of memory — always
   the recoverable direction.

**Failure ledger.** The loop, not the model, decides whether a failed tool
call may be retried: an exact repeat (same tool name, same canonical
arguments) is refused with a `RepeatedCall` failure until any tool call
succeeds. This turns "try the same thing again" into a compile error for
the model and forces a change of approach. Canonical arguments rely on
serde_json's sorted-key serialization (BTreeMap ordering — no crate in the
workspace enables `preserve_order`; the eval harness's byte-identical
request check would catch it first if one ever did).

**Terminal states**: `EndTurn` returns the reply — after the
**completion gate** (roadmap item 8): if files changed since the last
green check, the loop refuses once with the diff summary and the original
instruction instead of accepting the end turn.
`UnrecoverableStop`
(max_tokens / context_limit / content_filter) commits the truncated or
filtered reply first, so it stays inspectable; `MaxTurns` at the ceiling
with tool calls pending; budget exhaustion; cancellation.

---

## 5. Context pipeline

### 5.1 Projection (`DefaultContextCompiler`)

The compiler turns the session into a `ModelRequest` — a pure function of
its inputs, no I/O, no clocks:

```
system prompt
facts block (if non-empty)          ← stable for the whole task
transcript, in order:
  user items            → verbatim
  assistant items       → content + reasoning (byte-exact), tool calls
  tool results          → content + status; effects and artifact refs
                          are stripped (Loom-side state, never
                          model-visible)
```

Content-filtered assistant turns are dropped from the projection. `max_tokens`
is the lesser of the model's output cap and the reserved output budget.

Determinism is the point: the same session compiles to byte-identical
requests, which is what makes provider prefix caching effective and what
the eval harness asserts.

### 5.2 Repository facts

Once per task, `FsFactsSource` (engine `repo.rs`) produces a bounded,
deterministic snapshot on the blocking pool: language distribution
(extension counts, ranked), build systems, `src`/`tests` directories,
notable config files, and git state (branch via `rev-parse --abbrev-ref`,
changed files via `status --porcelain` — C-quoted paths are unquoted,
including octal byte escapes). The walk is depth-4, entry-capped at
20,000, hidden/gitignored dirs excluded. Everything degrades: no git, no
readable tree → fewer facts, never a failed task). The snapshot is frozen
for the task, so the facts block never churns the cached prefix mid-task.

Bootstrap facts (roadmap item 4) extend the same block: a depth-4 /
200-line file tree, toolchain probes (`cargo`/`rustc`, `node`/`npm`,
`python3`, `go`), and the head (4 KiB) of `AGENTS.md`, falling back to
`CLAUDE.md` — project instructions enter the cached prefix once, not as
per-turn context.

### 5.3 Census

Every compiled request carries a `ContextCensus`: estimated tokens per
source (system, facts, user history, assistant history, tool results,
reasoning) plus an authoritative total. Estimation is an injected
`TokenEstimator`, chosen per provider at setup: `O200kEstimator` (real
o200k BPE via tiktoken-rs) for the OpenAI-compatible providers, and
`AnthropicEstimator` — the same o200k count scaled 1.25× and rounded up,
because the 4.7+ tokenizer bills more tokens for the same text — for
Anthropic. The kernel itself keeps only the conservative
bytes÷3 `HeuristicEstimator` as the default, so the kernel stays
tokenizer-free. Attribution is careful to avoid double counting
(reasoning is counted once, facts are subtracted from system). The census
is how the loop, the pruning trigger, and the eval reporting all see the
same numbers.

### 5.4 Pruning (`PruningContextCompiler`)

A decorator around the default compiler that keeps sessions inside the
model's input budget by stubbing *old tool results* — never user items,
never assistant content or reasoning.

- **Trigger**: window-relative — 80% of the model's input budget, floored
  at 64K tokens. A 1M-token model never prunes a 64K transcript, and a
  session cannot park above a flat threshold and re-decide every turn.
- **Protected tail**: the last 3 turns *and* the trailing ~20K tokens,
  whichever reaches further back.
- **Strategies**, in fixed order: (1) duplicate read-only calls — keep the
  newest; (2) reads superseded by a later write to an overlapping path;
  (3) error results older than 4 turns; (4) age-out of *every* remaining
  unprotected read-only result, largest first — a gate, never a cap, so
  the projection cannot grow without bound.
- **Checkpoint freeze.** Decisions are computed only when the trigger
  fires, frozen into a `PruneCheckpoint` (serializable, replayable), and
  applied verbatim on later turns. Between triggers the projection is
  byte-stable. A new checkpoint supersedes the frozen one only when fresh
  decisions reclaim meaningfully *more* (≥ 20K additional tokens) than the
  frozen set already reclaims. Each trigger costs exactly one prefix-cache
  miss; a parked session costs zero.
- Stubbed results keep their call id and carry a deterministic stub body
  telling the model how to recover the content (re-run the call).
- **Turn-age masking** (roadmap item 10, default K=8): read-only results
  older than 8 turns are stubbed *below* the window trigger, through the
  same checkpoint/reclaim gate — bounded growth insurance for long
  sessions on large windows.
- **Forced prune.** A projection that overruns the budget outright —
  before the trigger has frozen anything that covers it — is rescued by
  deciding stubs on the spot and recompiling a stubbed view of the same
  transcript (same rules, same estimator). `BudgetExceeded` propagates
  only when nothing is prunable or the pruned view still does not fit;
  the trigger floor assumes windows of at least 64K tokens.
- There is no summarization stage: user items, assistant content, and
  reasoning are never reduced. A session whose non-prunable history
  outgrows the window fails closed (roadmap: handoff compaction).

---

## 6. Tool layer

### 6.1 Contract

A tool (`loom-kernel/src/tool.rs`) is an object-safe trait: a `ToolSpec`
(name, description, JSON-schema parameters derived via `schemars` with a
compaction pass — refs inlined, defaults dropped, single-value enum
oneOfs collapsed), a `call` future, `is_read_only`, and — key to
scheduling, pruning, and caching — `touched_paths(input)`: the exact paths
a call will read or write (`None` = unknown = treated as a wildcard;
a parse failure returns an empty set: a call that cannot run touches
nothing).

`ToolContext` carries the workspace root, cancellation token, per-call
timeout, artifact store, the tool cache, and a `recent_changes` log
(latest 32 recorded file writes/deletes, deduped) that evidence rendering
uses to link failures to the change that likely caused them.

### 6.2 The shipped tools

| Tool | Kind | Notes |
|---|---|---|
| `read_file` | read | UTF-8, offset/limit windows (default 2000 lines, 8 MiB scan cap), numbered lines |
| `search` | read | regex, gitignore-aware (hand-built ancestor chain, nearest file wins), 1 MiB/file cap, binary skip, summary mode >100 matches |
| `list_dir` | read | one level, byte-sorted, symlinks not followed for typing |
| `read_artifact` | read | paged read of a spilled artifact (§6.4) |
| `run_command` | mutating | any bare program inside an OS sandbox (§6.8; writes confined to workspace + temp + cargo home, network off), manifest rules escalate; no shell, both pipes drained concurrently, head+tail bounded capture with explicit omission markers |
| `verify` | mutating | `check`/`test` scope for the ecosystem detected from root markers (cargo > npm > python > go > make) or an explicit `ecosystem` argument; the command comes from a fixed per-ecosystem table, never from model input; ecosystems without a build-agnostic scope (npm `check`) refuse rather than guess; success → the pass summary line; failure → structured evidence (§6.5) |
| `edit_file` | mutating | exact old-text match, must be unique (0 or ≥2 matches = invalid input), 8 MiB file cap; atomic `edits: [...]` batch form across files (all-or-nothing, in order); every result echoes the edited region (±3 numbered lines, 8 KiB cap) so no re-read is needed; a whitespace-trim repair fires when exactly one trimmed match exists (recorded in `effects.repairs`); optional `verify` runs the detected ecosystem's check/test inside the same call (action fusion), skipping with a note when the scope is unsupported |
| `write_file` | mutating | create/overwrite, parents created; optional `verify` as above |

### 6.3 Confinement

Every path a tool touches resolves through `confinement.rs`: a
component-wise walk that `lstat`s each component, canonicalizes symlinks
(in-root targets allowed, out-root escape rejected), handles `..` through
missing paths by re-canonicalizing, resolves dangling links with a depth
limit of 8 (cycles are a loud error, never a wrong path), and strips the
canonical root to produce a workspace-relative path. The high-severity
regression tests — `..` after a missing path through a symlink, dangling
links, link cycles, links pointing outside the root — pin this behavior.

### 6.4 Output budgets and artifacts

Every tool output passes `bound()`: a 64 KiB cap, cut on a char boundary.
Oversize content spills to the `ArtifactStore` (content-addressed:
id + kind + bytes + SHA-256, written before the reference is issued) and
the model sees a bounded recovery note naming the artifact — which
`read_artifact` can page back in. Truncation is always explicit: markers
name exactly how many bytes were dropped, and `run_command`'s streams keep
a 64 KiB head *and* a 64 KiB tail each, because errors live at the tail.

### 6.5 Evidence shaping

Failed builds and tests are not shown as raw logs. Parsers extract
structure: cargo's `--message-format=json` diagnostics (primary span),
libtest human output (failing test, panicked-at location, message),
nextest JUnit, go vet/test, pytest, and python compileall — each with a
stdout-then-stderr fallback. The renderer emits a bounded block — up to 20 diagnostics /
20 failures, 240-char clips — and links each failure to the recorded
recent change on an overlapping path ("relevant change: src/lib.rs").
Unparseable failures fall back to bounded raw output. A failure is never
hidden, only compressed.

### 6.6 Batch scheduling

Tool calls in one assistant turn run as waves. Read-only calls run
concurrently (bounded); a call that *writes* a path overlapping another
call's reads or writes serializes behind it. Unknown touched-paths act as
a wildcard and serialize the batch. Results always return in call order
regardless of completion order; cancellation preserves completed slots and
poisons nothing; slot locks are panic-safe. Each mutating call's effects
are recorded (`note_effects`) and invalidate overlapping cache entries.

### 6.7 Tool cache

A per-agent-run content-addressed cache for read-only calls: the entry key
is the tool name plus canonical arguments; the value is signed by the
content (metadata + SHA-256) of every file the call read. A later lookup
with identical arguments is a hit only if all signatures still match; any
write to an overlapping path invalidates the entry. Signature computation
is blocking work and runs on the blocking pool — never under the cache
mutex, never on an async worker. A cheap `has_entry` probe lets the
common first-read path skip hashing entirely. A hit while the original
result is still in the projection unpruned returns
`unchanged since call <id>` — one line instead of the content again.

### 6.8 Sandboxed shell and command policy

`run_command` runs any bare program name — inside an OS sandbox
(`loom-tools/src/sandbox.rs`). The sandbox is the security boundary;
the workspace's `CommandPolicy` (`loom-tools/src/policy.rs`),
resolved once at `Engine::open`, is the *escalation* layer on top.

**Sandbox** (default for every program, policy or not):
- macOS: `sandbox-exec` with a generated Seatbelt profile; Linux:
  `bubblewrap` (`bwrap`). Both are pure process wrappers — no
  `unsafe` (workspace forbids it, ruling out Landlock).
- File writes denied by default; granted only to the canonical
  workspace, the system temp dir, and the cargo home
  (`CARGO_HOME` or `~/.cargo`, kept writable so builds work
  offline). Everything else stays readable. The workspace is the one
  bind that must exist (a missing workspace is a misconfiguration and
  fails loudly); every other writable grant is bound tolerantly, so a
  host with no `~/.cargo` and no `CARGO_HOME` — containers, CI —
  still gets a sandbox instead of a failed spawn.
- Network denied by default.
- The wrapper leads the process group (existing bounded capture and
  group kill unchanged), and the confinement is inherited by every
  child — a model-run `bash -c` pipeline cannot escape what its
  parent was granted. How the group survives differs by platform, and
  it is not symmetric: `sandbox-exec` `exec`s in place, so on macOS the
  child keeps the wrapper's pgid and `killpg` reaches the whole tree.
  `bwrap --new-session` calls `setsid()` in its sandboxed child, which
  moves the COMMAND into a new session and pgid where `killpg` on the
  wrapper's group cannot follow it. Linux therefore relies on
  `--unshare-pid`: the COMMAND becomes pidns init, `--die-with-parent`
  gives it `PR_SET_PDEATHSIG`, so killing the wrapper (timeout,
  cancellation) tears the namespace down and SIGKILLs every process
  inside it. Dropping either flag re-opens the leak; `sandbox.rs`
  unit-tests the argv on every platform so a macOS box cannot stay
  green while the Linux flags are missing.
- Bare names are resolved on `PATH` before wrapping, so a missing
  program fails with the same legible error as before.

**Escalation** (`.loom/commands.toml`, `[[command]]` rules keyed by
bare program name): `network = true` lifts the network deny;
`extra_writable = ["/abs/path"]` adds writable roots (absolute paths
only, validated); `sandbox = false` runs unsandboxed — for trusted,
sandbox-incompatible tools only. The old limits still apply:
`timeout_secs` (≤ 3600; on expiry the whole process group is killed
and the call fails visibly), `max_output_bytes` (per-stream head/tail
window, ≤ 1 MiB), and `env_scrub` (extra variables removed from the
child). A credential scrub floor (`DEEPSEEK_API_KEY`,
`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `GOOGLE_API_KEY`) applies to every child regardless
of manifest — a manifest can add to it, never remove from it. An
invalid manifest fails `Engine::open`
(`EngineError::CommandPolicy`) rather than falling back to the
default. The tool spec names the escalated programs, so it is stable
for the engine's lifetime. Every executed command is recorded as a
`CommandEffect` (program, args, exit status) and timeouts as the
call's error status: the session transcript is the audit log.

---

## 7. Persistence (`loom-store`)

One SQLite file — `loom.db` by convention. Its location is the
**frontend's** decision: `EngineConfig.db_path` carries an explicit path
and the engine never consults the environment for it. The CLI's policy:
`LOOM_DB` → `LOOM_DATA_DIR` → `XDG_DATA_HOME/loom` →
`~/.local/share/loom`, kept outside the workspace (the workspace is
walked by `search` and may be committed). The engine opens it twice —
once as the session store, once as the artifact store, same file, same
schema.

- **Schema v1**, forward-only migrations; a file from a newer schema is
  refused, not silently upgraded.
- Pragmas per connection: WAL, `synchronous=NORMAL`, `foreign_keys=ON`,
  `busy_timeout=5s`.
- Single `Connection` behind a mutex. Turn append is one transaction —
  items, task upsert, and the turn row commit atomically, so a crash
  leaves either a whole turn or no turn.
- **Load** rebuilds the session by strict index order; index gaps or
  malformed turn shapes surface as `Corrupt`, never as silently reordered
  history.
- **Leases** support exclusive task execution (acquire / renew / release
  with expiry; renewing a lease nobody holds fails; an expired lease can
  be acquired by a new owner).

## 8. Credentials (`loom-engine/auth.rs`)

Accounts are rows in the same database (UUIDv7 ids, provider, label,
method: API key / command / environment), with a partial unique index
enforcing **at most one active account per provider**. Secrets never
touch the database — the macOS Keychain (via `keyring`) is the only
secret store; login writes the keychain first and deletes it if the
account insert fails, so there are no orphan secrets, and a canary test
asserts the database never contains the key. Resolution order:
`--account` flag → active account → environment. `Method::Command` runs
a user-configured command through `sh -c` and takes its trimmed stdout as
the key. Removing the active account promotes the newest remaining one;
the (non-fatal) inability to delete a secret is reported as a warning,
not a failure.

## 9. Engine and CLI

`Engine::open` is the composition root: canonicalize the workspace,
resolve credentials, select and validate the provider through the
provider factory (capabilities and reasoning settings checked against the
adapter), build the tool registry (stable order, read-only tools first),
wrap the context compiler with the pruning decorator, attach the facts
source, and open the stores. It returns an `Engine` or a fully
taxonomized `EngineError` — never a partially wired agent. The engine
never prints; it reports through typed results and the event stream.

Provider selection lives in one private engine module
(`provider.rs`), the only place that names concrete adapters. It matches
the configured provider name against the adapters' `PROVIDER_NAME`
constants, rejects models whose context window is below the pruning
floor (64K tokens) as `ModelWindowTooSmall`, and fails loudly with
`UnknownProvider` or `CatalogMissing` rather than falling back to a
default provider or model. The engine may read the process environment
for credentials but never consults it for the database location — that
is the frontend's decision, passed in as `EngineConfig::db_path`.

The CLI is thin: `loom run` (interactive render — text to stdout,
reasoning to a dim stderr, NDJSON mode for machines), `loom auth`
(login/list/set-active/remove), `loom stats` (per-session analytics from
stored transcripts: turns, per-tool call quality, edit acceptance,
repeats, repairs, spill savings), and the eval entry points. Exit codes:
0 success, non-zero failure, 130 cancelled.

## 10. Providers

Three adapters ship:

- **DeepSeek** — a built-in capability table, because its API serves no
  capability metadata; reasoning effort and a thinking on/off switch are
  sent explicitly on every request.
- **OpenRouter** — capabilities and pricing resolved from the provider's
  model catalog (`GET /models`). Free and paid models travel the same
  path: pricing is data (zero for free, non-zero for paid), never a
  branch. The catalog is fetched and cached by the *frontend* and handed
  to the engine as `EngineConfig::catalog`, so the engine
  stays synchronous and offline-capable. Attribution headers are
  optional configuration.
- **Anthropic** — the messages API, not the OpenAI wire: system as a
  top-level param, `tool_use`/`tool_result` content blocks,
  `thinking_delta` reasoning capture, and Anthropic's error taxonomy.
  It deliberately runs its own streaming state machine instead of the
  shared `WireEndpoint` driver: refactoring the OpenAI wire's delicate
  paths without a funded key to re-verify DeepSeek/OpenRouter live was
  judged higher-risk than ~100 duplicated lines (the module docs carry
  the rationale). Capabilities come from a small static table because
  Anthropic's models endpoint exposes no capability fields; the table
  carries the current pinned ids (`claude-fable-5-1`,
  `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` at
  1M/128K, plus the 4.x legacy entries), unknown ids fail loud, and
  dateless aliases are refused on purpose — they float to new
  snapshots with different windows. `temperature` is never sent:
  Claude 4.7 and later reject a non-default value with a 400. Cache
  breakpoints are placed from the kernel's provider-agnostic
  `stable_prefix_messages` hint: the compiler promises how many leading
  messages are byte-stable across requests, and the adapter marks the
  end of that region `cache_control: ephemeral`. Token estimation for
  Anthropic models is scaled up from the o200k count (see
  `loom-engine::estimate`): the 4.7+ tokenizer bills more tokens for
  the same text and Anthropic publishes no local vocabulary, so the
  estimate errs high — pruning a little early beats a request the
  provider rejects.

The shared layer is deliberate, not incidental. `http` owns transport
plumbing (liveness timeouts, bounded error-body reads, `Retry-After`);
`openai_wire` owns the OpenAI-compatible request shape and chunk
accumulation; `wire_stream` owns the SSE state machine — decoding,
timeout enforcement, drop-to-cancel, unterminated-`[DONE]` tolerance,
and stop-signal normalization. An adapter supplies only configuration and
error classification through a `WireEndpoint` trait, so the delicate
streaming behaviour cannot drift between providers. The finalize policy
also classifies mid-stream error objects, which OpenRouter can emit.

`ModelCapabilities` are explicit per model (window, output cap, tool
support) and resolved at setup, so an unknown model fails before the
first request, not mid-task. Errors classify into retryable
(rate-limited, overloaded, transport, server) and permanent (auth,
quota-exhausted, invalid request, context overflow, protocol); the loop's
retry policy consumes the former. `ModelEvent::Completed` is the sole
authority for usage numbers — the loop never double-counts from
streaming deltas — and it carries `cost_usd` when the provider reports
actual spend, so paid usage is visible without adapter-specific
accounting in the loop.

## 11. Evaluation harness

The eval suite (`docs/eval.md` is the protocol guide) has two halves.

- **Offline proofs**, run on every commit: recorded provider exchanges
  replay through the real compiler and loop, asserting the replayed
  transcript matches the recording verbatim, requests are byte-identical
  across replays (the determinism contract), and zero events were
  dropped. Corpus invariants are proven too: every seed fails exactly its
  fail-to-pass set and passes exactly its pass-to-pass set, and the gate
  rejects an untouched workspace.
- **Live measurement**: N runs per task (default 5) against a real
  provider. Field tasks (B2–B4, B6–B17, mined from Loom's own fix
  history) are graded by outcome — SWE-bench-shaped gates of
  `fail_to_pass`, `pass_to_pass`, and `protected` paths; lab tasks (B1,
  B5) pin a system prompt as narrow regression instruments. Reports carry
  per-run turns, census, est/billed tokens, cached input tokens, cost,
  wall time, and tool-cache hit/miss, with aggregates per task and a
  suite headline (gated pass rate). Strict mode asserts every gate green;
  tolerant mode survives failures for baselines.

Compare suites by medians over N ≥ 5; the observed noise floor is ±10%.

---

## 12. Invariants worth keeping

**Correctness**
- Append-only transcripts; projection never mutates history.
- Persist before in-memory commit; database may lead memory, never lag.
- Call↔result pairing validated before any append.
- Stop-reason protocol enforced; nothing committed on violation.
- First task ending wins; cancellation commits completed work and names
  the cancelled remainder.

**Security**
- Path confinement on every tool path, symlink-hardened, regression-tested
  against escapes.
- No unsandboxed model-reachable subprocess: every model-run program
  (a shell included) is a bare name inside the OS sandbox (writes
  confined, network off), with process-group kill and a credential env
  scrub floor on every child; the one `sh -c` is the user-configured
  credential command, never model-reachable.
- Secrets only in the OS keychain; canary-tested.
- `unsafe_code = "forbid"` in kernel and tools.

**Efficiency**
- Byte-stable projection between events → prefix cache holds; one cache
  miss per pruning trigger, gated by a delta-reclaim floor.
- Every model-visible byte budgeted: 64 KiB tool outputs with explicit
  markers, head+tail stream capture, spilled artifacts for oversize,
  compact tool schemas, compressed failure evidence.
- Blocking work on the blocking pool; hashing never under a lock.
- Facts scanned once per task; capabilities resolved once per task.

**Testing** — the workspace carries 594 tests (13 live-API tests
ignored by default): unit tests per module,
protocol and invariant tests against in-memory seams, security regression
tests for confinement, canary tests for secrets, integration tests that
run real `git`/`cargo`, the replay eval, and corpus-invariant proofs. CI bar: all tests green,
clippy clean, `cargo fmt --check` clean.
