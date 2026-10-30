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

Eight crates in one Cargo workspace, edition 2024, `unsafe_code = "forbid"`
in the crates that matter most. Dependencies point strictly downward — the
kernel never links a filesystem store or a CLI, the engine never prints.

| Crate | Role | Depends on |
|---|---|---|
| `loom-core` | Facade re-exporting the public surface for embedders, plus the shared data-dir policy (`paths`) | engine, kernel, providers, store, tools |
| `loom-kernel` | The agent itself: loop, context compilation, session/transcript model, tool abstraction, tool cache, artifacts, events, budgets | no workspace crates — it defines the side-effect traits (`ModelProvider`, `SessionStore`, `ArtifactStore`, `Tool`, …) itself |
| `loom-engine` | Composition root: builds a configured `Agent` from paths, auth, a provider factory, tool registry, and stores; repository facts scanner; owns the OpenRouter catalog policy (`engine::catalog`), the trace export format (`engine::trace`), and its aggregation (`engine::trace_stats`) | kernel, store, providers, tools |
| `loom-providers` | Model provider adapters (DeepSeek, OpenRouter, GLM, NVIDIA, Groq over one shared OpenAI-compatible wire; Custom for any user-supplied OpenAI-compatible endpoint; Anthropic on its own messages-API driver) | kernel model types |
| `loom-store` | SQLite persistence: sessions, turns, tasks, artifacts, auth accounts | kernel store traits |
| `loom-tools` | The shipped tool suite (read/search/edit/write/list/run/verify + artifact read) | kernel tool traits |
| `loom-eval` | The measurement harness: SWE-bench Verified runs in container isolation (`swebench` module + `swebench-run` orchestrator) — the agent binary executes inside official per-instance testbed images, grading belongs to the official `swebench` harness; plus the offline self-optimization loop (`selfopt` module + bin) over sweep artifacts | `loom-core` only (facade; ADR-021) — containers via docker + the `loom` CLI, model calls via the rolling `engine` alias |
| `loom-cli` | Binary: argument parsing, output rendering, run/auth/stats commands | `loom-core` only (ADR-021) |

The kernel defines traits for everything with side effects
(`SessionStore`, `ArtifactStore`, `ModelProvider`, `FactsSource`,
`ContextCompiler`, `Tool`), with in-memory defaults for tests. `loom-store`
and `loom-tools` supply the production implementations; the engine wires
them together. This keeps the agent loop unit-testable with replayed model
responses and memory stores — the eval harness depends on exactly that
property.

---

## 2. Runtime model

- **Tokio, multi-threaded runtime**, owned by the CLI. The CLI's
  consumers — the stdout/stderr renderer, plus the NDJSON trace writer
  when `--trace` is on — receive events from a kernel `FanoutSink`, one
  channel each, so engine code never touches the terminal and a stalled
  disk cannot stall rendering. Shutdown uses
  `shutdown_timeout(5s)`; Ctrl-C steers a `TaskControl` — one press
  pauses (the in-flight turn commits, then the loop stops), a second
  aborts mid-flight — and the process exits 130 either way.
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
(`schema_version: 8`, per-sink monotonically increasing `sequence`,
snake_case `type` tag). Version 2 adds `Usage.cost_usd`, the
provider-reported charge; version 3 adds `ContextPruned` (prune
attribution); version 4 adds `AwaitingInput` (engine idle marker after a
task ends, naming the session — the signal a resume loop keys on);
version 5 adds `TaskPaused` (graceful between-turns stop, distinct from
`TaskCancelled`'s mid-flight abort); version 6 adds `ContextHandoff`
(one-shot LLM handoff compaction); version 7 adds `FastPathHit` (a
zero-request solve booked as a transcript-shaped turn); version 8 adds
the surfaced-decision kinds — `FastPathDeclined` (the coarse reason a
fast path fell through: no intent matched, or the solver's guards said
no), `CapabilitiesResolved` (which authority — catalog row, provider
table, or replay recording — fixed the model's capabilities, emitted
once per engine), and `HarnessNotice` (per-command deviations the tools
layer reports: sandbox passthrough, a stream truncated by a
pipe-holding descendant; one event per distinct notice per batch). All
new fields are additive, so older payloads still
deserialize. A lock spans numbering and send, so arrival order
equals sequence order even with concurrent emitters. The channel sink is
unbounded — a slow consumer queues rather than back-pressures the agent;
only a closed receiver drops (counted, warned once). `FanoutSink`
composes several sinks into one so multi-consumer frontends (the CLI's
renderer + trace writer, future GUI/TUI) each get their own numbered
stream; its drop count sums the children. `StatusRecorder` is a sink
that folds the stream into a pollable `StatusSnapshot` — current turn,
in-flight tools (correlated by call id), running request attempt, task
phase, and the idle session after `AwaitingInput` — the progress
surface a GUI or TUI polls while a task runs; clones share one
snapshot, so the driving API and the poller hold the same state.
`StrictSink` wraps a
sink and cancels the task on first drop — event delivery is lossless
or the run dies.

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
serde_json's sorted-key serialization (BTreeMap ordering — no crate in
the workspace enables `preserve_order`).

**Terminal states**: `EndTurn` returns the reply — after the
**completion gate** (roadmap item 8): if files changed since the last
green check, the loop refuses once with the diff summary and the original
instruction instead of accepting the end turn.
`UnrecoverableStop`
(max_tokens / context_limit / content_filter) commits the truncated or
filtered reply first, so it stays inspectable; `MaxTurns` at the ceiling
with tool calls pending; budget exhaustion; cancellation; and a graceful
pause (§9) — a between-turns stop whose in-flight turn committed in
full.

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
requests, which is what makes provider prefix caching effective.

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
is how the loop and the pruning trigger see the
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
- **History elision** (roadmap item 12, deterministic half): outside the
  protected tail, tool-call *arguments* older than 8 turns collapse to
  signatures (long strings become `[pruned N chars]`; short identifying
  fields like paths survive), user items superseded by a later user
  item age out into a one-line stub, and — where the engine's provider
  gate allows it — assistant *reasoning* traces older than 8 turns drop
  from the projection (`reasoning = None`). The session brief (ordinal
  0) and the *last* user item — the live task's instructions — are
  exempt by construction. Reasoning elision is provider-gated because
  replaying reasoning verbatim is a wire contract, not an
  optimisation: DeepSeek rejects tool-bearing requests whose prior
  assistant turns lack `reasoning_content`, GLM only loses a cache
  benefit, and Anthropic never forwards reasoning on replay — so the
  engine enables it for GLM and Anthropic only, and the kernel default
  keeps it off. Elisions freeze into the same checkpoint and re-apply
  verbatim.
- **Assistant-text elision** (roadmap item 12, CliffCompaction-style,
  arXiv 2609.26779): outside the protected tail, an assistant message
  older than 8 turns whose text exceeds 400 chars collapses to its
  first 200 chars plus a ` [pruned N chars]` marker — a deterministic
  signature, idempotent, frozen and re-applied through the same
  checkpoint. This runs before any LLM is involved because the
  evidence says aged assistant prose is the dominant reclaimable mass
  and its leading signature preserves most of its value.
- **LLM handoff, last resort.** When even the pruned view cannot fit
  (non-prunable history outgrew the window), the loop builds a
  goal-directed digest of the session — task, brief, recent assistant
  messages, recent failed tool results, touched files — and makes one
  provider call (no tools, 1024-token cap, plain-text end-turn only)
  for a handoff summary. The frozen checkpoint then projects a *view*
  transcript: brief, the handoff text as a fresh user item, and the
  tail after the freeze point; ordinal-keyed elisions are skipped in
  this view (call-id elisions still apply, they are
  position-independent), and the checkpoint re-freezes with the
  handoff so replays are byte-stable. One handoff per task; a second
  overrun fails closed. Emits `ContextHandoff` (event schema v6);
  `AgentConfig::handoff_compaction` gates it, and the engine's bare
  arm turns it off along with loop shaping.
- **Fast-path seam (roadmap item 3).** Before the loop starts,
  `Engine::run_task` offers the instruction to a deterministic
  analyzer (`loom-tools/src/fastpath.rs`): rename, constant change,
  and format intents parsed by anchored grammars — never fuzzy. A hit
  is a proposal, not a trust decision: guard checks (occurrence
  bounds, identifier collision, exactly-one const site, CRLF and
  from-value mismatches declined), snapshot of every file about to
  change, apply, then verify through the same policy-gated sandbox as
  `run` (`cargo fmt --check` + `cargo check` for the format path).
  Any failure restores the snapshot and returns `None` — the
  never-worse contract is that a miss leaves the workspace
  byte-identical and falls through to the agent loop untouched. A hit
  is booked by the kernel as a zero-request solve
  (`Agent::record_fast_path`: user item, `FastPathHit` — event schema
  v7 — assistant item with the note, `TaskCompleted` with all-zero
  counters), so transcripts and summaries stay honest about the model
  never being called. A miss surfaces too: the attempt returns a coarse
  decline reason (`FastPathAttempt`), and the engine emits
  `FastPathDeclined` (schema v8) after the task's terminal events —
  `no-intent-match` versus `intent-{kind}-unsolved` — so the trace
  shows the harness tried and stepped aside, not that the decision
  never happened. `EngineOverrides::fast_paths` (default on) gates
  the seam; the bare arm turns it off with loop shaping.
- **Fast-path seam, second slice.** The same seam now also parses
  dependency-update intents (`update/bump <crate> to <version>`) and
  routes format intents by ecosystem: `detect_ecosystem` picks the
  toolchain, so `gofmt` phrasing drives `gofmt -l` + `go build ./...`
  verification on Go manifests while the Cargo path keeps `cargo fmt
  --check` + `cargo check`. Dependency rewrites are strict: only the
  `[dependencies]`/`[dev-dependencies]`/`[build-dependencies]` tables,
  only simple `name = "x.y.z"` entries (inline tables, dotted keys,
  `.workspace` entries decline), indent and final newline preserved,
  then `cargo metadata --no-deps --offline` verifies the manifest
  before the change sticks. Ecosystems without a deterministic
  offline verifier for the edit class (npm, Python, Make) decline by
  design — the never-worse contract outranks coverage.

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
Its asynchronous counterpart `Engine::open_async` adds exactly one step
in front — obtaining the OpenRouter model catalog through `engine::
catalog` (fetch with the provider's own credential, cache beside the
database, stale-cache fallback) when the config does not already carry
one — so no frontend re-implements that sequencing.

Capability resolution is catalog-first for every provider, not just
OpenRouter: whatever provider the config names, `open_async_with`
fetches the catalog with the OpenRouter credential (any OpenRouter
account the keychain holds; only OpenRouter runs scope the account and
base URL), and `resolve_capabilities` prefers the catalog's row for
the model, falling back to the provider's built-in table when the
catalog is unknown, unreachable, or silent about that model. Catalog
wins when both know the model — a catalog refresh adds models and
corrects windows with no code change — and the failure direction is
safe: a stale table only serves as the floor. An OpenRouter run whose
catalog fetch fails propagates the error (the catalog is that
provider's source of truth); every other provider degrades silently
to its table. No model id is a default anywhere: the CLI requires
`--provider` and `--model` explicitly, and the eval harness takes
them from its environment.

The user-level instructions file is a frontend concern, not an engine
one (ADR-008: the engine never reads the environment). `loom run`
resolves `global_instructions_path()` — `$LOOM_CONFIG_DIR`, else
`$XDG_CONFIG_HOME/loom`, else `~/.config/loom`, else `.` — reads its
`AGENTS.md` (missing file is a clean no-op; a 10 KiB byte bound with
char-boundary-aware truncation keeps the prompt finite), and composes
the system prompt once, before the engine opens: the built-in
instruction or an explicit `--system-prompt` carries the base, and the
global file appends as a final `## Global instructions (<path>)`
section — it exists because the user asked for it, so it outranks
everything but is the first thing `--no-global-instructions` removes.
Composing at process start keeps the prompt byte-stable for the whole
session (§8.1.1). Because the engine is never consulted, embedders and
the eval harness (which build their own `EngineConfig`) are unaffected
by any machine's global file; project-level `AGENTS.md` remains a
facts-scan concern (§5.2) and needs no CLI involvement.

Both opens take `EngineOverrides` (via `open_with`/`open_async_with`;
the plain forms pass an empty one): every field is optional and `None`
keeps the production choice — a prebuilt `provider` (replaying a
recording, no credential involved), a `provider_hook` wrapping
whichever provider results (recording, shadowing), a `registry`, a
`compiler`. The overrides exist for embedders: until 2026-10-20 the
eval harness was the canonical one, opening `open_async_with` with a
recording hook and running through the exact wiring the CLI gets.
Since the SWE-bench migration the harness drives the CLI binary
inside testbed containers instead (§11), so `loom-core` embedders are
the remaining consumers.

The driving API is resume-first: besides `run_task` on a live session
handle, `Engine::continue_session(id, input, …)` loads a persisted
session and runs the next task on it, returning the advanced `Session`
and the task outcome together in a `Continuation` — a failed, cancelled,
or paused task still advances the session, so only load errors are
`Err` (`SessionNotFound` when the id does not resolve). After every
task's terminal event the engine emits `AwaitingInput` naming the idle
session: a stream-only frontend keys its resume loop on that marker
without polling. Kernel-level drivers emit no such marker — it is an
engine concept, so a hand-rolled kernel loop sees no new events
(an engine-driven frontend sees the marker).

Stopping is two-granularity: `TaskControl` (the `run_task`/
`continue_session` parameter) offers `cancel` — the hard stop that
aborts mid-flight, drops the stream, and fills unstarted tool calls
while keeping every already-committed item — and `pause`, a graceful
stop observed only at turn boundaries: the in-flight turn (model reply
and tool batch) commits in full, then the loop ends with
`TaskStatus::Paused`/`end_reason: "paused"` and a `TaskPaused` event.
The session survives either stop; the CLI maps one Ctrl-C to pause and
a second to cancel.

Progress polling follows the same stream-first shape — there is no
separate status channel to keep coherent. `kernel::event::StatusRecorder`
is a sink that folds the events a task already emits (`TurnStarted`,
`ModelRequestStarted`/`ModelCompleted`, `ToolStarted`/`ToolCompleted`,
the terminal events, `AwaitingInput`) into a pollable `StatusSnapshot`:
current turn, in-flight tools, running attempt, phase, and the idle
session id. Frontends clone the recorder, fan one clone out next to
their rendering sink, and poll the retained handle on their own cadence;
one recorder tracks one task, so concurrent runs each get their own.

Above the engine sits one composition: best-of-N parallel rollouts
(roadmap item 21, `engine::worktrees`, CLI `--best-of N`). The module
checks the workspace is inside a git repository (fail-closed otherwise —
parallel rollouts need one `HEAD` to branch from), materializes N
detached worktrees under `.loom/worktrees/`, and runs N full child
engines concurrently (semaphore-bounded, default 3): each child gets the
same task, the same frozen facts compiler, a worktree-local database,
and its own provider built from a resolved-once credential, so a
catalog-driven adapter is fetched once and every child behaves exactly
like a normal run. Selection is deterministic code, not a judge model:
candidates rank by completion (`end_turn`/`fast_path`), then fewest
failure markers (loop nudges, failed tool calls, task failures — the
`micro` taxonomy Harness² uses for its all-rollouts-fail fallback), then
turns, then model requests. The winner's patch (`git diff --cached`,
captured from every child even on failure, the same extraction the eval
harness uses) is applied to the parent workspace with `git apply`; a
failed apply keeps every worktree for forensics rather than guessing.
Cancellation propagates to all children through their `TaskControl`s,
and a run that produces no non-empty patch reports no winner instead of
inventing one. The honest cost: N rollouts multiply model spend, which
is why the flag is opt-in and the report carries each child's usage;
the delegated-economics trigger (item 19) still decides when N>1 pays.

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
reasoning to a dim stderr, NDJSON mode for machines; `--trace DIR`
exports the task's evidence — see below), `loom auth`
(login/list/set-active/remove), `loom stats` (per-session analytics from
stored transcripts: turns, per-tool call quality, edit acceptance,
repeats, repairs, spill savings; the listing/filter/fold data step is
`kernel::analytics::sessions_overview`, so every frontend computes
identical numbers; `--trace DIR` switches the source to
exported traces, where the manifests carry the provider's own billed
counts and the event streams carry `context_pruned` reports — per-task
and per-trigger prune attribution without server-side persistence; the
walk/fold/aggregation is `engine::trace_stats`, the CLI only renders). The data-dir resolution the frontends share
(`LOOM_DATA_DIR` → XDG → `~/.local/share`) is `loom_core::paths`, a
suggested default the engine itself never consults (ADR-008). Exit
codes: 0 success, non-zero failure, 130
cancelled.

`loom run --trace DIR` writes one directory per task (roadmap item
20.1): `events.ndjson` (the numbered event stream, one envelope per
line), `transcript.json` (the committed session items and task
records), and `manifest.json` (harness version, event schema version,
run configuration, outcome with a stable `end_reason` token, the task
summary counters, and the final context census). The format is owned by
`engine::trace` (`TraceWriter`) as a data contract: every frontend —
CLI, TUI, GUI — writes byte-identical traces, so any consumer (the
`engine::trace_stats` aggregation included) can read any trace. The
export is best-effort by design: write failures are counted into the
manifest (`events_write_failures`) and warned on stderr, never mapped to
a run outcome — a trace is an artifact of the run, not a precondition.
Mechanism-level attribution ships with event schema v3: a pruning pass
that freezes a new checkpoint emits one `ContextPruned` event carrying
the trigger arm (`window` / `age_mask` / `rescue`) plus per-call
mechanism and estimated reclaim, so a trace consumer can attribute
savings per mechanism by summing `reclaimed_tokens` across those events
(deduplicating by `call_id` — turns that merely re-apply a frozen
checkpoint emit nothing).

The boundary itself is guarded, not just described: `loom-cli` reaches
workspace code through `loom-core` alone (ADR-021), and
`crates/loom-cli/tests/facade.rs` fails the suite the moment the binary
gains a direct dependency on any other workspace crate — the rule runs
with `cargo test --workspace` in CI, not on review vigilance. What
stays in the CLI is presentation only: argument parsing (`args.rs`),
the terminal renderer (`output.rs`), tracing setup and exit codes
(`main.rs`), the auth prompts (`auth_command.rs`), and the Ctrl-C
signal mapping — domain logic lives behind the facade where every
frontend shares it. Cross-process frontends are deliberately not part
of today's design: everything runs in-process, and the seam a future
server process would cut along already exists in draft form — the
serde-envelope event stream is versioned (`EVENT_SCHEMA_VERSION`) and
self-describing, `StatusSnapshot` is serializable, and the trace
formats are byte-stable data contracts.

## 10. Providers

Five adapters ship:

- **DeepSeek** — a built-in capability table, because its API serves no
  capability metadata; reasoning effort and a thinking on/off switch are
  sent explicitly on every request. Its usage block names the cache-hit
  count `prompt_cache_hit_tokens` instead of the OpenAI-standard
  `prompt_tokens_details.cached_tokens`; that dialect is read by a
  `dialect_usage` override in this file, not by the shared wire.
- **OpenRouter** — capabilities and pricing resolved from the provider's
  model catalog (`GET /models`). Free and paid models travel the same
  path: pricing is data (zero for free, non-zero for paid), never a
  branch. The catalog policy — fetch with the credential the provider
  itself would use, cache beside the state database with a 24-hour TTL,
  fall back to a stale cache when the network is unreachable — is owned
  by the engine (`engine::catalog`), driven by `Engine::open_async`; a
  frontend holding a snapshot already can pass it as
  `EngineConfig::catalog` to the synchronous `Engine::open`.
  Attribution headers are optional configuration.
- **GLM** — Z.ai's **metered** API (`api.z.ai/api/paas/v4`), never the
  Coding Plan endpoint. Same OpenAI-compatible wire as DeepSeek with
  GLM's `thinking` parameter: the 5.3 family forces thinking on
  (`disabled` fails the request), so a configured effort of "none" is
  clamped to `low` rather than disabling; glm-5.2 accepts both.
  `clear_thinking: false` requests Preserved Thinking — the wire layer
  already replays assistant `reasoning_content` verbatim, which is the
  client-side obligation that mode carries. Its usage block sticks to
  the OpenAI-standard shapes, so this adapter adds no usage-dialect
  code at all. Verified by wiremock; no direct Z.ai key is
   configured on this machine. The model itself is reachable today
   through OpenRouter (`z-ai/glm-5.3`), which serves the same
   adapter family and returns HTTP 200.
- **NVIDIA** — NIM hosted inference (`integrate.api.nvidia.com/v1`),
  OpenAI-compatible. Model ids are `publisher/model` (`z-ai/glm-5.3`,
  `z-ai/glm-5.3-flash`); a built-in capability table covers the live
  GLM-5.3 deployments — the DeepSeek V4 rows that shared the catalog
  hit end-of-life (HTTP 410, live-checked 2026-10-09) and are
  deliberately absent. Reasoning depth rides the top-level
  `reasoning_effort` field: GLM-5.3 deployments clamp an effort of
  "none" to `low` (thinking is always on there too); any future
  non-forced deployment passes `none` through as real non-think mode.
  No `thinking` object — that is Z.ai first-party dialect. NIM's chat
  template defaults `clear_thinking` to false (preserved thinking),
  which the wire layer already satisfies by replaying
  `reasoning_content` verbatim. Like GLM, no usage-dialect code.
  Live-verified 2026-10-09 against `z-ai/glm-5.3` (two-turn session:
  tool call, reasoning stream, usage, cache-write accounting); the
  free trial tier requires no credit card and is the standing
  zero-cost candidate for the eval backlog (roadmap item 1).
  SUPERSEDED 2026-10-18: that endpoint stopped serving
  completions. `/v1/models` still answers with 81 models (including
  `z-ai/glm-5.3`), but `POST /v1/chat/completions` returns no
  response — HTTP 000, no payload, at both 45s and 120s client
  timeouts. A healthy catalog with hanging completions is an
  outage, not a config error. It caused every `model_error` in the
  on-disk eval corpus (11 of 115 runs), so NIM was no longer the
  zero-cost candidate; the eval's OpenRouter account was the live
  path until the local suite was removed (2026-10-20; §11).
- **Groq** — hosted inference (`api.groq.com/openai/v1`),
  OpenAI-compatible. Model ids are `publisher/model`. The built-in
  capability table covers the three agentic deployments live on the
  account this was written against — `openai/gpt-oss-120b`,
  `openai/gpt-oss-20b` (131,072 context, 65,536 max output, reasoning,
  tool calls, but **no** parallel tool calls: both serialize to one
  call per turn) and `qwen/qwen3.8-27b` (131,072 context, 16,384 max
  output, reasoning, tool calls, parallel calls verified live).
  Everything else resolves to "unknown", including the
  `llama-3.1-8b-instant` / `llama-3.3-70b-versatile` rows Groq
  decommissioned for free and developer tiers on 2026-08-16.
  Two wire deviations, both live-verified 2026-10-18, both routed
  through the shared dialect hooks rather than forked into a driver:
  Groq rejects an assistant message carrying `reasoning_content`
  (HTTP 400, `property 'reasoning_content' is unsupported`) and
  streams the same fragment under the plain key `reasoning`, so the
  endpoint declares a `reasoning_field` that both the request builder
  and the chunk accumulator honor; and in-stream failures arrive on an
  `x_groq` envelope rather than the standard `error` key, which the
  accumulator folds into the same wire-error slot. Status mapping adds
  Groq's two bespoke codes, 498 (flex tier at capacity, retryable
  overload) and 499 (caller cancelled, terminal). Reasoning depth rides
  the top-level `reasoning_effort` field; `gpt-oss` accepts only
  `low`/`medium`/`high` and 400s on `none` and `max`, so the adapter
  clamps the same way GLM clamps "none" — thinking is always on there.
  Two measurement caveats worth knowing before comparing numbers:
  Groq rewrites `temperature: 0` to `1e-8` (not true greedy), and
  `gpt-oss` chunks carry a `delta.channel` key the accumulator
  ignores. Free plan: 30 rpm, 1,000 requests/day, 8,000 tokens/min,
  200,000 tokens/day — tokens, not requests, are the binding ceiling,
  and the free tier is unbilled.
- **Custom** — any OpenAI-compatible endpoint the user points at:
  `--provider custom --base-url <chat API> [--models-url <catalog API>]`
  plus a key. Named gateways (`loom provider add <name> --base-url …`,
  persisted in `providers.json` beside the database) ride the same
  adapter under their own name, credential env `{NAME}_API_KEY`, and
  one catalog cache per name (`<name>-models.json`). There is no
  default base URL; missing one fails the open
  with a pointed error. The catalog reader is schema-tolerant across
  the shapes seen in the wild — `{data: [...]}` (OpenAI, OpenRouter,
  most gateways), `{models: [...]}`, or a bare array — and sniffs
  field spellings: context windows (`context_length`, `context_window`,
  `max_context_tokens`, `max_context_length`, `context_len`,
  `max_model_len`, `max_input_tokens` — numbers or numeric strings)
  and output caps (`max_completion_tokens`, `max_output_tokens`,
  `top_provider.max_completion_tokens`, `max_tokens`). Tool support:
`supported_parameters` listing `tools`/`tool_calls`, or
`supports_tools`-style booleans; parallel needs `parallel_tool_calls`;
reasoning needs `reasoning` or `reasoning_effort`. Omitted fields
default to the safe side: 131,072-token context (above the 64K pruning
floor), tools on, parallel and reasoning off, no output cap. An id the
catalog does not list fails loudly (typo protection), and an advertised
window below the floor is rejected at open. `--models-url` overrides
the catalog location (default `{--base-url}/models`); the cached
snapshot is keyed to that URL, so switching endpoints never serves
another gateway's model list, and a fetch failure falls back to a
stale cache for the same URL only. Chat requests speak the plain
OpenAI shape — no dialect fields — and OpenRouter-format conventions
that cost nothing to honor are honored: `usage.cost` surfaces as
provider-reported cost, and a `finish_reason` of `error` maps to a
retryable server failure. `CUSTOM_API_KEY` or `loom auth login custom`
supplies the key. The pi agent's nano-llm gateway is the reference
case — no dedicated adapter: point `--base-url` at its `/v1` root
(the value pi keeps in `NANO_LLM_BASE_URL`), leave `--models-url` on
the `{--base-url}/models` default, and store the key once with
`loom auth login custom`.
- **Anthropic** — the messages API, not the OpenAI wire: system as a
  top-level param, `tool_use`/`tool_result` content blocks,
  `thinking_delta` reasoning capture, and Anthropic's error taxonomy.
  Only *leading* system messages fold into the top-level `system` param;
  a system message that appears after the conversation has started
  stays in place as an in-turn `role: "system"` message. That in-turn
  form is the 5.5-era per-turn steering channel (e.g. an
  `output_config.effort` note): it applies to the next assistant turn
  without rewriting the cached prefix, whereas hoisting it into the
  top-level param would bust every cache breakpoint. Usage accounting
  carries the two Anthropic-specific fields the wire reports:
  `cache_creation_input_tokens` (cache writes, billed above base rate)
  and `thinking_tokens` (`usage.output_tokens_details`, the attribution
  split of output tokens while thinking is always-on-adaptive on 5.5).
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
Usage dialects follow the same rule: the shared wire structs carry only
OpenAI-standard shapes, everything else lands in a flatten-caught map,
and each adapter's `dialect_usage` hook reads its own keys (DeepSeek's
`prompt_cache_hit_tokens`, OpenRouter's `cost`) — a provider that sticks
to the standard shapes, like GLM, writes no dialect code.

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

The eval suite (`docs/eval.md` is the protocol guide) measures Loom
against **SWE-bench Verified** in container isolation. The in-house
corpus, gates, replay proofs, editor bench, and live benchmarks were
removed on 2026-10-20: they measured Loom against its own fix history
rather than against the field, and nothing outside the repo consumed
their numbers.

- **Isolation**: the `swebench-run` orchestrator (`loom-eval`) runs
  each instance in its official testbed docker image —
  `sweb.eval.x86_64.<instance_id>`, the repository already checked out
  at the issue's `base_commit` with the correct interpreter and
  dependencies. A linux build of the `loom` CLI, the frozen prompt, and
  an output dir are the only mounts; the host never touches a
  checkout, the agent never touches the host.
- **The run**: inside the container `loom run --json --no-save` works
  the `/testbed` tree with full tool access under the usual sandbox;
  the patch is extracted in-container (`git add -A` +
  `git diff --cached`) even when the run fails, so partial work is
  scored honestly.
- **Grading**: Loom emits the official predictions JSONL and hands it
  to `swebench eval verified` — the `resolved` verdict comes only from
  the official harness, so no local gate can be tuned to flatter the
  number. The sweep headline is the resolved rate; per-run reports
  (turns, tokens, cost, exit codes, patch sizes) land in
  `target/eval/swebench/` for context.

The dataset (500 instances) is fetched once from HuggingFace and
cached with a row-count integrity check; explicit instance ids must
resolve or the sweep aborts — selection never shrinks silently.

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

**Testing** — the workspace carries 694 tests (13 live-API tests
ignored by default): unit tests per module,
protocol and invariant tests against in-memory seams, security regression
tests for confinement, canary tests for secrets, and integration tests
that run real `git`/`cargo`. CI bar: all tests green,
clippy clean, `cargo fmt --check` clean.
