# Loom Roadmap

Status of the Loom coding-agent runtime. Two lists: what is **shipped** and
verified in the codebase today, and what is **not yet shipped** — planned
work ordered roughly by priority, with the design seam each piece will plug
into. The companion `architecture.md` describes how the shipped system
works; this document tracks direction.

---

## Shipped

### Foundations

- **Workspace architecture.** Seven crates with strict dependency
  direction: `loom-kernel` (agent core, I/O-free behind traits),
  `loom-providers`, `loom-store`, `loom-tools`, `loom-engine`
  (composition root), `loom-cli`, and the `loom-core` facade for
  embedders. `unsafe_code = "forbid"` in kernel and tools.
- **Async runtime and cancellation.** Tokio with token-based
  cancellation end to end: model requests, tool calls, subprocess
  pipes, scans. Cancelled tasks commit completed work, append explicit
  cancelled fillers, and exit cleanly. Blocking work (walks, hashing,
  git, file IO) runs on the blocking pool.
- **Session and event model.** Append-only transcripts with
  UUIDv7 identifiers enforced on every input path; typed events with a
  versioned envelope, per-sink sequence numbers, and ordered delivery;
  strict sinks that cancel the task if an event is lost.

### The agent loop

- **Turn loop with protocol validation.** Stop-reason contract enforced
  (no tool calls on end-turn, at least one on tool-calls); turn-level
  atomicity with call↔result pairing validated before any append;
  persist-then-commit ordering so the database can lead memory but
  never lag.
- **Loop-owned failure policy.** A failure ledger refuses the exact
  repeat of a failed tool call (same name + canonical arguments) until
  any call succeeds, with a model-visible hint explaining why.
- **Task budgets.** Per-task ceilings on total, input, and output
  tokens, tool-call count, and wall time — checked per turn in a
  deterministic order, so in-flight work still completes and commits.
- **Retry classification.** Provider errors split into retryable
  (rate-limited, overloaded, transport, server) and permanent (auth,
  malformed request) with a retry policy at the loop.

### Context management

- **Deterministic projection.** System prompt → repository-facts block →
  transcript, with content-filtered turns dropped and tool-result
  side-channel state (effects, artifact refs) stripped. The same
  session compiles to byte-identical requests — the property the
  prefix cache and the replay eval both lean on.
- **Byte census.** Per-turn, per-source token estimates (system, facts,
  user, assistant, tool results, reasoning) with an authoritative
  total; surfaced to the loop, the pruning trigger, and eval reporting.
- **Repository intelligence.** One bounded, deterministic scan per task
  on the blocking pool: languages, build systems, source/test
  directories, config files, git branch and changed files (porcelain
  output unquoted, including octal escapes). Degrades to fewer facts,
  never a failed task.
- **Window-relative context pruning.** Trigger at 80% of the input
  budget (64K floor); protected tail of 3 turns / 20K tokens; four
  stub strategies (duplicate reads, superseded reads, stale errors,
  age-out of all remaining unprotected reads); frozen, replayable
  checkpoints with a delta-reclaim gate so a parked session costs zero
  extra cache misses and each trigger costs exactly one.

### Tools

- **Eight shipped tools** behind an object-safe trait with JSON-schema
  parameters compacted for token cost: `read_file` (windowed, 8 MiB
  cap), `search` (regex, gitignore-aware, binary-skipping,
  summary-throttled), `list_dir`, `read_artifact`, `run_command`,
  `verify`, `edit_file` (unique exact-match), `write_file`.
- **Path confinement.** Symlink-hardened component-wise resolution on
  every tool path: in-root symlink targets allowed, escapes rejected,
  dangling links and cycles bounded with loud errors. Regression-tested
  against the high-severity escape patterns.
- **Bounded output with artifacts.** 64 KiB cap per tool output with
  char-boundary-safe cuts and explicit omission markers; oversize
  spills to a content-addressed artifact store and the model gets a
  paged read tool to recover it. Command streams keep a 64 KiB head
  *and* tail — errors live at the tail.
- **Evidence shaping.** Failed builds/tests parsed into compact
  structured evidence (cargo JSON diagnostics, libtest, nextest JUnit)
  and linked to the recent change on the likely-causing path;
  unparseable failures degrade to bounded raw output, never hidden.
- **Batch scheduling.** Wave scheduler: concurrent read-only calls,
  write/read conflicts serialized by overlapping paths, results in call
  order, cancellation-safe, poison-safe.
- **Content-hash tool cache.** Read-only results cached against the
  content signature of every file read; any overlapping write
  invalidates. Hashing on the blocking pool, never under the lock;
  a cheap probe keeps first reads hashing-free.

### Persistence and credentials

- **SQLite store.** Single WAL-mode database (`loom.db`) for sessions,
  turns, tasks, and artifacts; schema v1 forward-only migrations;
  single-transaction turn appends; corruption-detecting loads; leases
  for exclusive task execution.
- **Credentials.** Multiple named accounts per provider with exactly
  one active (partial unique index); secrets live only in the OS
  keychain — never in the database (canary-tested); key, command, and
  environment resolution methods; clean promote/remove semantics.

### Product surface

- **CLI.** `run` with human rendering (reasoning on stderr, NDJSON for
  machines), `auth` management, exit-code discipline (130 on cancel).
  Engine is frontend-neutral: no printing outside the CLI renderer.
- **Providers.** Two adapters over one shared OpenAI-compatible wire
  and SSE streaming driver: DeepSeek (built-in capability table,
  reasoning effort and thinking controls) and OpenRouter (capabilities
  and pricing resolved from the provider's model catalog). Free and
  paid models travel the same path — pricing is data, never a branch —
  and the catalog is fetched and cached by the frontend, so the engine
  stays synchronous. Unknown models, missing catalogs, and windows
  below the 64K pruning floor all fail at setup, not mid-task.
  Provider-reported cost is carried on usage for paid spend.
- **Eval harness.** Replay-based equivalence testing (transcript and
  byte-identical request assertions), zero-dropped-event assertions,
  per-turn census and cache hit/miss reporting with aggregates, and a
  pinned benchmark task suite.
- **Verification bar.** ~465 tests (plus 13 live-API tests ignored by
  default), clippy clean, fmt clean.

---

## Not yet shipped

Ordered by expected value. Each item names the seam it plugs into; none
requires unwinding shipped invariants.

### Near term

1. **Command execution policy.** Today `run_command`/`verify` allow-list
   exactly `cargo`. Ship a configurable policy: allow-list per workspace
   (manifest-declared commands), per-command resource limits (wall time,
   output caps, environment scrubbing), and audit records for every
   executed command. Seam: the existing allow-list check and
   `CommandEffect` records.
2. **Process-group teardown.** A timeout kills the direct child
   (`kill_on_drop`) but not grandchildren (`rustc` under `cargo`).
   Proper teardown needs process groups (`killpg` — via a small
   `unsafe`-containing helper crate or a sandboxing dependency, since
   the workspace forbids `unsafe` in-tree). Seam: `exec.rs`, where the
   child is spawned.
3. **Verification beyond Cargo.** `verify` scopes to
   `cargo check`/`cargo test`. Extend to the ecosystems the facts
   scanner already detects (npm/pyproject/go/Make): command selection
   per detected build system, plus evidence parsers per ecosystem's
   failure format. Seam: the `cargo`-only scope check and the evidence
   parser registry.
4. **Budget-exceeded rescue.** The pruning wrapper cannot rescue an
   inner `BudgetExceeded` (the inner compiler errors before the wrapper
   can decide to prune). Ship a compile path that treats the inner
   budget error as a prune trigger — re-compile with aggressive stubs
   instead of failing the task. Seam: `PruningContextCompiler::compile`.
5. **CLI session resume.** The engine can load and resume a persisted
   session; the CLI has no `--resume`/`--session` surface for it. Add
   listing, selection, and continuation, plus transcript export.

### Mid term

6. **More providers.** DeepSeek and OpenRouter share one OpenAI-shaped
   wire; the abstraction is proven only for that shape. Add a
   non-OpenAI-shaped provider (Anthropic-style messages, or Gemini) to
   pressure the `ModelProvider` boundary itself, including
   capability-correct pruning floors per model. OpenRouter's catalog is
   the model to copy for capability discovery: fetch in the frontend,
   pass to the engine as data.
7. **Long-term memory.** Designed (session-persistent knowledge with
   explicit add/expire semantics) but not built. The `FactsSource` seam
   and the facts block projection are the insertion point; the hard
   part is eviction and staleness policy, not storage.
8. **Adaptive pruning thresholds.** Current constants (80% trigger,
   20K floors) come from one benchmark corpus. Ship measurement-driven
   tuning: per-model trigger curves using the census and cache
   hit/miss data the loop already records, evaluated by the replay
   harness.
9. **Search parity.** The hand-built gitignore chain diverges from the
   `ignore` crate on negation-precedence edge cases (documented in
   code). Either adopt the crate's matcher for chained files or pin
   the divergence with fixture tests from real repositories.
10. **Artifacts lifecycle.** Spilled artifacts accumulate in `loom.db`
    with no retention policy. Add reference-counting from live
    transcripts plus a GC (age- or size-bounded) and a size-reporting
    command.

### Later

11. **Parallel agent orchestration.** Multiple tasks over one session,
    worktree-per-agent isolation, and a merge path. The lease support
    in the store is the first brick; needs conflict policy and an
    orchestrator above the engine.
12. **Mid-task model selection.** The model is fixed per task by design
    (capabilities resolve once). Allow planned switches at task
    boundaries — cheap given the budget plumbing, but it invalidates
    the single-capability assumption baked into the loop's setup.
13. **Frontends.** The engine boundary is frontend-neutral (no
    printing, typed results, ordered events). A TUI or desktop client
    is additive once session resume ships.
14. **Distributed verification.** Farm `verify` runs (the expensive
    step) to remote or containerized runners with the same evidence
    contract, so local context budgets are never spent on build logs.

### Open questions

- Should the failure ledger loosen for idempotent read-only retries
  after non-deterministic errors (e.g. transient filesystem races),
  or does strictness keep paying for itself?
- Is the bytes÷3 token estimate worth replacing with a real tokenizer
  per provider, given the pruning trigger only needs consistency —
  but cross-model comparability in eval may eventually need real
  counts?
- Where does user-facing configuration live (allow-lists, provider
  defaults): per-workspace manifest, user config, or both with a
  defined precedence?

---

## Maintenance bar (ongoing)

- All tests green, clippy clean, `cargo fmt --check` clean.
- No new blocking calls on async workers; hashing never under locks.
- Projection stays byte-stable between events (eval asserts it — keep
  it that way).
- Any new model-visible byte passes the output budget with an explicit
  marker.
