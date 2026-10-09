# Loom — Product & Engineering Roadmap

Loom is a headless, provider-agnostic coding-agent runtime written in Rust.
Its central thesis:

> **Maximize successful work per model invocation while minimizing
> context, tokens, and model turns.**

The model is the reasoning component. Loom is the deterministic controller
around it.

This document has three layers:

1. **Vision** — goals, principles, and the iteration sequence (stable;
   changes need an ADR-level reason).
2. **Status** — what is shipped and verified in the codebase today.
3. **Backlog** — numbered, evidence-gated items (1–23), each mapped to an
   iteration, the goals it serves, and the seam it plugs into.

It is a roadmap, not a promise. The companion `architecture.md` describes
how the shipped system works; `eval.md` describes how claims are measured.

---

## Goals

Stable product-level goals. Every backlog item names the goals it serves;
an item that serves none does not ship. Goals are evaluated **together**:
a change that cuts tokens but materially reduces task success is not an
improvement.

1. **G1 — Maximize successful work per model invocation.** Each turn
   should be actionable and productive; do not chase low token counts at
   the expense of success.
2. **G2 — Minimize unnecessary turns, context, and cost.** Keep only
   decision-relevant context in view; preserve stable prompt prefixes
   where caching pays.
3. **G3 — Move deterministic work into the runtime.** Discovery,
   scheduling, formatting, compilation, verification, retries, and output
   reduction belong to software whenever it is reliable.
4. **G4 — Increase the practical capability of every model tier.** Help
   less capable models succeed through bounded task shaping, relevant
   evidence, constrained tools, deterministic checks, and useful recovery
   feedback. This improves practical effectiveness, not the model's
   underlying intelligence.
5. **G5 — Adapt the workflow to the selected model and task.** No single
   context strategy, task granularity, delegation policy, or verification
   loop is assumed best for every model.
6. **G6 — Delegate only when delegation has positive expected value.**
   Account for independence, parallelism, specialization, token/turn
   cost, synthesis, conflicts, and remaining budget before spawning.
7. **G7 — Remain provider-agnostic and incrementally extensible.** Keep
   provider and tool specifics out of the core; extend components instead
   of rewriting them.
8. **G8 — Maintain excellent Rust engineering practices.** Idiomatic
   ownership, structured async, bounded concurrency, explicit errors,
   focused modules, minimal justified abstractions, strong tests.
9. **G9 — Measure real outcomes.** Task success, tokens, turns, latency,
   cost, and recovery quality against a baseline; token savings alone do
   not count.
10. **G10 — Use execution evidence to improve Loom over time.** Learn
    better context, scheduling, delegation, and routing policies from
    traces — without uncontrolled self-modification.

---

## Core principles

1. **Never spend a model invocation on deterministic work.** Discovery,
   search, symbol lookup, formatting, compilation, tests, result
   summarization, tool scheduling, context selection, retry decisions,
   and cancellation are software's job. The model is invoked for
   reasoning, ambiguity, planning, and genuine decisions.
2. **Never provide unnecessary context.** The model receives the minimum
   useful context for its *next decision*. The durable session may be
   large while the model-visible projection stays small.
3. **Optimize successful work, not raw token count.** Primary measures:
   successful tasks per model turn, per 1M model tokens, per dollar.
   Secondary: input/output/cached tokens, requests, turns, tool calls,
   latency, retries, verification failures.
4. **Prefer one useful turn over many exploratory ones.** Maximize the
   information and deterministic execution around each invocation.
5. **Deterministic controller, reasoning model.** The loop owns context
   construction, tool scheduling, retries, cancellation, validation,
   verification, lifecycle, accounting, termination, and (later)
   delegation. The model reasons and proposes actions.
6. **Extend, do not rewrite.** Additive changes; preserve working
   behavior; abstractions only for real requirements; no speculative
   infrastructure; no rewrites for aesthetics; record necessary
   architectural changes in an ADR.
7. **Rust quality is first-class.** Loom is also a Rust learning project:
   explicit async boundaries, structured cancellation, clear error types,
   cohesive modules, minimal public APIs and dependencies, clean crate
   boundaries, no gratuitous `Arc<Mutex<_>>`, `unsafe_code = "forbid"`.
8. **Increase practical capability, honestly.** Compensate for model
   weaknesses through task shaping, context compilation, constrained
   tools, deterministic execution, verification, evidence-driven
   recovery, and adaptive workflows — then *measure* whether each support
   helps. A weaker model may want narrower tasks and stronger feedback; a
   stronger one may want higher-bandwidth turns and fewer interruptions.
   These are hypotheses to benchmark, not fixed assumptions, and Loom
   never conceals failures or claims model equivalence.

### Iteration rules

```text
existing Loom → smallest justified capability → new seam only if needed
              → tests → benchmark (eval.md) → architectural review
```

Do not jump from an idea to a large subsystem; do not rewrite the previous
iteration because a clean slate is easier; do not add abstractions that
"may be useful someday". ADRs when a real boundary changes.

---

## Iterations

The long-term sequence. Each builds on the previous one and preserves its
seams. Order is the intended dependency order, not a calendar; later
iterations may start early where a backlog item is cheap and measured.

| # | Iteration | Status | Backlog items |
|---|---|---|---|
| 1 | Core service | **Done** | — (see Shipped) |
| 2 | Core efficiency & repository intelligence | **In progress (current)** | 1, 3, 4✓, 5✓ᴬ, 6✓, 7✗, 8✓ᴬ, 10✓ᴬ, 11⏸, 12⏸, 16, 17 |
| 3 | Intelligent subagent scheduling | Not started | 19, 21 |
| 4 | Execution safety & richer tools | **Largely shipped early** | 2✓, 13⏸, 23 |
| 5 | Skills & contextual knowledge | Partial (AGENTS.md facts) | 4✓ (instructions), 20.4 |
| 6 | MCP & LSP integration | Not started | 18 |
| 7 | Plugin system | Future scope | — |
| 8 | Intelligent model routing | Not started; prerequisites in place | 9⏸, 14✓ |
| 9 | Learning & self-optimization | Not started; data path partial | 1, 20 |
| — | Product surface (cross-cutting) | Partial | 15, 22 |

Legend: ✓ shipped · ✓ᴬ phase A shipped, remainder open · ✗ skipped on
evidence · ⏸ deferred on evidence (revisit trigger recorded in the item).

### Iteration 1 — Core service (done)

Smallest headless core that completes a real coding task end to end:
session → task → context compiler → provider → streamed response →
deterministic tools → results → model → completion, with usage and events
recorded.

Shipped semantics that later iterations must preserve:

- **Model retries.** Requests are side-effect free; a retryable error
  after streaming started may still retry within the attempt budget.
  Streamed deltas are provisional until `ModelCompleted`.
- **Cancellation is not rollback.** A cancelled batch records completed →
  actual result, failed → actual error, in-flight/queued → cancelled; the
  assistant message and full result set commit atomically.
- **`StopReason`, not absence of tool calls, decides completion.**
  `EndTurn` → success (subject to the completion gate, item 8);
  `ToolCalls` → continue; `MaxTokens`, `ContextLimit`, `ContentFilter` →
  task failure (continuation/compaction recovery is future work).
- **Capabilities are per model** (`provider + model → ModelCapabilities`),
  and the model is fixed for the life of a task.

Planned GLM support differs from the original plan: Loom targets the
**metered Z.ai API**, never the GLM Coding Plan subscription endpoint
(a non-API product; custom-harness use risks an account ban — see the
warnings in `crates/loom-cli/tests/eval.rs`). Shipped adapters are
DeepSeek, OpenRouter, Anthropic, GLM, and NVIDIA (wiremock-verified against the
Z.ai spec; no funded key yet for live runs).

### Iteration 2 — Core efficiency & repository intelligence (current)

Make Loom materially better than a basic agent loop without large new
subsystems:

- **Advanced context compilation** — recoverable pruning, checkpoints,
  historical selection, reasoning-inclusion policy, better estimation,
  cache-friendly construction, references to archived artifacts.
  *Shipped:* pruning v1 + turn-age masking, frozen checkpoints, artifact
  refs, o200k estimation. *Open:* items 10 (rest), 12.
- **Repository intelligence** — answer "which parts of this repository
  matter for this task?" *Shipped:* facts scan + bootstrap facts (item 4).
  *Open:* a persistent symbol/import/reference index is **not** yet
  justified by measurement; it arrives through item 18 if at all.
- **Higher-bandwidth operations** — intent-level `inspect / search /
  change / run / verify`. *Shipped:* fused edit+verify, multi-edit batches,
  region echo, sandboxed shell.
- **Tool scheduling** — dependencies, conflicts, safe concurrency.
  *Shipped:* wave scheduler with path-conflict serialization. Bounded
  prefetch is item 11.
- **Deterministic verification** — only failures or ambiguity reach the
  model. *Shipped:* multi-ecosystem `verify`, evidence shaping, fused
  verify, completion gate.
- **Fast paths** — rename, simple compiler fix, constant change, format,
  dependency update solved with **no** model invocation. *Not started*
  and not yet a backlog item; needs a task analyzer and an eval slice of
  fast-path-shaped tasks before it can be ranked.
- **Measurement** — turns, context tokens per turn, tokens per solved
  task, exploratory calls, verification-triggered retries vs the
  Iteration 1 baseline. *Shipped* in the eval suite; N ≥ 5 comparisons
  are blocked on a funded low-variance eval model (item 1).

### Iteration 3 — Intelligent subagent scheduling

Delegation as a runtime optimization: spawn only when

```text
parallelism gain + specialization gain + context reduction
  + success-probability gain
  − child tokens − child turns − synthesis cost − conflict risk  >  0
```

The parent owns the task; a child gets a bounded capsule (goal, scope,
constraints, allowed tools, expected output, budget), not the parent's
context. Modes: research, review, verification, implementation —
*parallelize knowledge acquisition aggressively, writes conservatively*.
Lifecycle: spawn, run, observe, redirect, cancel, complete. The scheduler
lives above `Agent::run_task`, never inside the turn loop. First step is
item 19 (one read-only scout, one level deep); item 21 only after 19
shows delegation pays.

### Iteration 4 — Execution safety & richer tools

Pulled forward because the shell lever was large and measured. *Shipped:*
`edit_file`/`write_file`/`run_command`/`verify`, OS sandbox (Seatbelt /
bubblewrap) with network off by default, the command manifest as the
escalation layer, credential scrubbing, process-group kill, `CommandEffect`
audit records. *Open:* resource-aware allow/ask/deny policy (needs an
interactive frontend for "ask"), `git` as a structured tool, worktree
isolation (item 21), containerized runners (item 23).

### Iteration 5 — Skills & contextual knowledge

Knowledge loaded on demand, not injected globally. *Shipped:* always-on
project instructions (`AGENTS.md` → `CLAUDE.md`, 4 KiB head) through the
facts block. *Open:* an `.agents/skills/` index where the model sees skill
names first and loads bodies on demand, attached through the context
compiler seam — not a separate prompt mechanism. Learned lessons
(item 20.4) reuse the same seam.

### Iteration 6 — MCP & LSP integration

MCP servers attach through the existing `Tool` trait; nothing MCP-specific
leaks into the loop. LSP supplies definition, references, symbols,
diagnostics, rename, type information — justified only by fewer
exploratory model turns. Tree-sitter symbol tools (item 18) come first.

### Iteration 7 — Plugin system (future scope)

Tools, skills, agents, providers, hooks, policies, commands, and context
providers as plugins, built on proven seams only. DeepSeek's harness
architecture is research input, not a dependency.

### Iteration 8 — Intelligent model routing

Smallest sufficient reasoning for each task class (trivial → large,
research), treated as an optimization problem; verification uses no model
when deterministic checks pass. Prerequisites shipped: per-model
capabilities and pricing as data, five providers. Model profiles
(item 9) come first; routing changes the per-task model-fixed invariant
and needs an ADR.

### Iteration 9 — Learning & self-optimization

Trace everything that matters (task, classification, context decisions,
model choice, tool sequence, subagent decisions, tokens, turns, latency,
verification, outcome); evaluate strategies against fixed task suites
with the model held constant; then across model tiers to learn which
supports generalize and which need per-model configuration (G4, G5). Track
success by model and task class, solved tasks per 1M tokens and per
dollar, turns and tokens per solved task, recovery after verification
failure, and regressions from over-constraining or fragmenting tasks.
Self-improvement means better policies and routing — never autonomous
modification of Loom's own source. Path: item 20.

### Explicitly deferred

Plugin marketplace, fancy TUI, large built-in tool catalog, learned
routing, autonomous source self-modification, distributed or multi-host
orchestration — downstream of a strong core.

---

## Current state

```text
Current iteration:     2 (core efficiency), with iteration 4 tools shipped early
Language:              Rust (stable), 7 crates, unsafe forbidden
Shipped providers:     DeepSeek, OpenRouter (free + paid), Anthropic, GLM,
                      NVIDIA (NIM hosted inference)
GLM status:            metered Z.ai API, wiremock-verified (no funded key)
NVIDIA status:         NIM free-trial tier (no credit card), live-verified against
                      z-ai/glm-5.3 (2026-10-09); DeepSeek V4 NIM rows EOL'd
Eval models in use:    OpenRouter free tier (high variance); DeepSeek key drained;
                      NVIDIA NIM free tier (live-verified, ready for N≥5 sweeps)
Primary target:        successful work per model invocation
Biggest open risk:     N=1 measurements below the noise floor (see item 1)
```

---

## Shipped

Verified in the codebase as of 2026-10-09. Backlog items that shipped are
also summarized here so this section is the single source of truth.

### Foundations

- **Workspace architecture.** Seven crates with strict dependency
  direction: `loom-kernel` (agent core, I/O-free behind traits, depends
  on no workspace crate) ← `loom-providers`, `loom-store`, `loom-tools`
  ← `loom-engine` (composition root) ← `loom-core` (facade for
  embedders) ← `loom-cli`. Provider and tool specifics cannot leak into
  the kernel by construction. `unsafe_code = "forbid"` in kernel and
  tools.
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
- **Token census.** Per-turn, per-source token estimates (system, facts,
  user, assistant, tool results, reasoning) with an authoritative
  total; surfaced to the loop, the pruning trigger, and eval reporting.
  Counts come from an injected `TokenEstimator`: o200k BPE
  (`O200kEstimator`) for OpenAI-compatible providers, the same scaled
  1.25× and rounded up (`AnthropicEstimator`) for Anthropic; the kernel
  keeps only the bytes÷3 `HeuristicEstimator` as its default.
- **Repository intelligence.** One bounded, deterministic scan per task
  on the blocking pool: languages, build systems, source/test
  directories, config files, git branch and changed files (porcelain
  output unquoted, including octal escapes), plus bootstrap facts
  (item 4): a depth-4 / 200-line file tree, toolchain versions, and the
  head of `AGENTS.md` (fallback `CLAUDE.md`, 4 KiB). Degrades to fewer
  facts, never a failed task.
- **Window-relative context pruning.** Trigger at 80% of the input
  budget (64K floor); protected tail of 3 turns / 20K tokens; four
  stub strategies (duplicate reads, superseded reads, stale errors,
  age-out of all remaining unprotected reads); frozen, replayable
  checkpoints with a delta-reclaim gate so a parked session costs zero
  extra cache misses and each trigger costs exactly one. A projection
  that overruns the budget outright is rescued by a forced prune —
  decide stubs, recompile a stubbed view of the same transcript — and
  fails closed only when nothing is prunable or the pruned view still
  does not fit. Turn-age masking (item 10) stubs read-only results older
  than 8 turns even below the trigger, through the same reclaim gate.
  Stubs are one line. Output reservation is automatic (capped at 32K).

### Tools

- **Eight shipped tools** behind an object-safe trait with JSON-schema
  parameters compacted for token cost: `read_file` (windowed, 8 MiB
  cap), `search` (regex, gitignore-aware, binary-skipping,
  summary-throttled), `list_dir`, `read_artifact`, `run_command`,
  `verify`, `edit_file`, `write_file`. `edit_file` is unique exact-match
  with an atomic `edits: [...]` batch form across files, an echo of the
  edited region (±3 numbered lines) so no re-read is needed, and a
  whitespace-trim repair when exactly one trimmed match exists (recorded
  in `effects.repairs`). Repeat read-only calls whose earlier result is
  still unpruned in the projection return `unchanged since call <id>`.
- **Path confinement.** Symlink-hardened component-wise resolution on
  every tool path: in-root symlink targets allowed, escapes rejected,
  dangling links and cycles bounded with loud errors. Regression-tested
  against the high-severity escape patterns.
- **Bounded output with artifacts.** 64 KiB cap per tool output with
  char-boundary-safe cuts and explicit omission markers; oversize
  spills to a content-addressed artifact store and the model gets a
  paged read tool to recover it. Command streams keep a 64 KiB head
  *and* tail — errors live at the tail. A command timeout kills the
  entire process group, so grandchildren (`rustc` under `cargo`)
  cannot outlive the kill (macOS: the wrapper `exec`s in place and
  `killpg` covers the tree; Linux: `--unshare-pid` + `--die-with-parent`
  tear down the pid namespace — see architecture.md §6.8).
- **Multi-ecosystem verification.** `verify` detects the workspace's
  build system from root marker files (cargo > npm > python > go >
  make) or takes an explicit `ecosystem` argument; the command comes
  from a fixed per-ecosystem table — never from model input, so no
  allow-list is needed. Ecosystems without a build-agnostic `check`
  (npm) refuse the scope rather than guess a script, and the fused
  verify in `edit_file`/`write_file` skips with a note instead of
  failing the edit.
- **Evidence shaping.** Failed builds/tests parsed into compact
  structured evidence (cargo JSON diagnostics, libtest, nextest JUnit,
  go vet/test, pytest, python compileall) and linked to the recent
  change on the likely-causing path; unparseable failures degrade to
  bounded raw output, never hidden.
- **Sandboxed shell and command policy** (item 2). `run_command` runs
  any command inside an OS sandbox — Seatbelt on macOS, bubblewrap on
  Linux — with writes confined to workspace + temp + cargo home and
  network off by default. A `.loom/commands.toml` manifest is the
  escalation layer (`network`, `extra_writable`, `sandbox = false`) and
  sets per-rule limits — a wall-time cap (the whole process group is
  killed at expiry and the model sees the error), a per-stream output
  window, and extra environment scrubbing. Provider credential env names are scrubbed
  from every spawned child regardless of manifest; every executed
  command is recorded as a structured `CommandEffect` in the session
  transcript, so the session is the audit log.
- **Batch scheduling.** Wave scheduler: concurrent read-only calls,
  write/read conflicts serialized by overlapping paths, results in call
  order, cancellation-safe, poison-safe.
- **Completion gate** (item 8, phase A). On `EndTurn`, if files changed
  since the last green check, the loop refuses once with the diff
  summary and the original instruction.
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
  machines), `auth` management, `stats` (per-session turns, per-tool
  call quality, edit acceptance, repeats, repairs, spill savings — from
  transcripts), exit-code discipline (130 on cancel).
  Engine is frontend-neutral: no printing outside the CLI renderer.
- **Providers.** Five adapters. Four share one OpenAI-compatible wire
  and SSE streaming driver: DeepSeek (built-in capability table,
  reasoning effort and thinking controls), OpenRouter (capabilities
  and pricing resolved from the provider's model catalog), and GLM
  (Z.ai metered API; 5.3-family forced thinking with effort clamping,
  Preserved Thinking), and NVIDIA (NIM hosted
  inference; publisher/model ids; free-trial tier). Free and
  paid models travel the same path — pricing is data, never a branch —
  and the catalog is fetched and cached by the frontend, so the engine
  stays synchronous. Unknown models, missing catalogs, and windows
  below the 64K pruning floor all fail at setup, not mid-task.
  Provider-reported cost is carried on usage for paid spend. The third,
  Anthropic messages (item 14), has its own streaming state machine,
  places `cache_control` breakpoints from the provider-agnostic
  `stable_prefix_messages` hint, and carries a static capability table
  for the 5.5 lineup and 4.x legacy models (dated ids only).
- **Eval suite** (`docs/eval.md`). Offline proofs on every commit:
  replay equivalence (transcript and byte-identical requests),
  zero-dropped events, and corpus invariants (each seed fails exactly
  its fail-to-pass set, passes exactly its pass-to-pass set, and an
  untouched workspace fails its gate). Live measurement: 17 tasks
  (15 outcome-gated bugfix tasks mined from Loom's own fix history,
  2 lab instruments), N runs per task, per-run turns / census / billed
  and cached tokens / cost / wall time / tool-cache hits, per-task
  aggregates and a gated-pass-rate suite headline; strict and tolerant
  modes.
- **Verification bar.** 648 tests passing (13 live-API tests ignored by
  default), clippy clean, fmt clean.

---

## Backlog

Numbered items (stable ids, referenced from commits and the CHANGELOG).
Items that shipped keep their entry for the evidence trail; the
**Iterations** table above maps each to its iteration. Ordered by expected
value against the goals. Each item names the goals
it serves, the evidence behind it, and the seam it plugs into; none
requires unwinding shipped invariants. Evidence is drawn from a survey
of harnesses with working userbases (Claude Code, Codex CLI, OpenCode,
Aider, Pi, mini-SWE-agent, SWE-agent, OpenHands, ForgeCode,
Terminus-KIRA, Codebuff, Amp, Crush, gptme, Plandex, Goose, OpenDev,
Sema Code, oh-my-pi, Serena, RTK, Meta-Harness) and the published
ablations behind them. Three findings from the 2026 literature frame the
whole list. First, the *harness* moves the same model's Terminal-Bench
2.x pass@1 by 13–24 points (ForgeCode 81.8 vs Claude Code 58.0 on the
same Opus 4.6; GLM-5.2 at 72.6 vs 59.8 across four harnesses on TB 2.1),
and a controlled SWE-bench Verified grid measured harness-induced
variance at 7.8× model-induced variance with 6 of 9 model rankings
reversing across harnesses. Second, the *cost* effect is larger and more
reproducible than the pass-rate effect: harness choice moves tokens per
solved task by up to 40×, while paired pass-rate deltas often carry
confidence intervals that include zero. Third, harnesses in the same
score tier differ in cost by up to 4×. So every item is gated on
measurement (item 1) — with cost/turns as the primary discriminating
metrics and pass rate as the co-equal success constraint — and
vendor-reported numbers are treated as hypotheses until Loom's own eval
reproduces them.

### Near term — measurement and the cheapest wins

1. **Efficiency eval, phase 2** (G9, G10). The suite measures outcome
   and cost; extend it so it can *rank harness changes*:
   - a 30-task hard subset as the iteration gate (the Meta-Harness
     protocol), then external slices (SWE-bench Verified,
     Terminal-Bench) for comparability;
   - ~~headline **$ and turns per solved task**, cache-read ratio, and
     tool-failure rate alongside pass rate~~ — shipped: the suite
     headline now carries an `economics` block (total and per-solved
     cost, turns per solved mean/median, token-weighted cache-read
     ratio over successful runs, micro-eval tool-failure rate);
   - ~~per-tool, per-model **failure-mode micro-evals** (edit-apply
     success, malformed-call rate, repeated-call rate) as release
     gates~~ — shipped offline half: every run report (success or
     failure) embeds a `micro` block (per-tool fault taxonomy, edit
     acceptance, identical repeats, spills), recomputable from the
     session snapshots `eval_live` persists under
     `records/sessions/` and cross-checked by an offline test; the
     per-model comparison dimension and the 30-task gate remain;
   - `loom stats`: what each mechanism saved — pruning, tool-cache hits,
     evidence shaping, output filters — from the events already
     recorded (RTK's `gain`). Shipped transcript half (`loom stats`
     reports turns, per-tool call quality, edit acceptance, repeats,
     spill savings from any stored session); mechanism attribution
     needs persisted events, not just transcripts.
   - **Funded low-variance eval model** — the gating prerequisite.
     The free tier's variance (B9: 6–40 turns on identical config)
     sits above most levers, so items 4, 5, 6, 8, and 10 shipped on
     N=1 A/Bs that do not meet the maintenance bar (N ≥ 5 medians).
     Once funded: re-baseline and re-run those A/Bs at N ≥ 5, and
     record which claims survive. Candidate models (checked 2026-10-09):
      GLM-5.3 via the metered Z.ai API ($1.40/M input, $4.40/M output,
      $0.26/M cached input; 95.4% SWE-bench Verified any-scaffold; 1M
      context) — the same provider the current-state block already
      plans, so one funded key unblocks both; or DeepSeek V4 Pro
      ($1.32/$3.96; 96.4%). Budget scale: 15 field tasks at N=5 is ~75
      runs; at a cache-heavy 0.3-1M input tokens per run plus output, a
      full sweep lands in the tens of dollars — the blocker was never
      the price, it is having a metered key wired into the eval at all.
      A third candidate costs nothing at all: NVIDIA's NIM free-trial
      tier (no credit card) hosts z-ai/glm-5.3 at 1M context —
      live-verified 2026-10-09 with a real streaming session — so a
      free `NVIDIA_API_KEY` may satisfy this prerequisite within its
      per-model rate limits, and the N≥5 sweep then paces itself
      instead of paying. (The NIM catalog's DeepSeek V4 deployments
      hit end-of-life 2026-08/2026-09 and are no longer candidates.)
   - **Basic-harness baseline and model tiers** (G4, G5, G9). Add a
     "bare loop" configuration (read/edit/shell only; no facts, pruning,
     evidence shaping, completion gate) and run the same suite on at
     least two model tiers with the model held constant per comparison.
     This is the only way to test the G4 claim that Loom raises a weaker
     model's success rate, and the input that model profiles (item 9)
     need.
   - **Persist events** alongside transcripts (also item 20.1), so
     `loom stats` can attribute savings per mechanism.
   Seam: the live eval reports and the event stream.
2. **Sandboxed shell** (G1, G2, G3) — **shipped 2026-10-08**. Strong
   models working through a
   shell are ~53% cheaper than through fixed tool menus (Fan et al.,
   2026); Pi, mini-SWE-agent, and Terminus run mostly or only on a
   shell. (2026-10-09 nuance: the interface effect is task-shape and
    model-tier dependent — an enterprise-benchmark ablation found bash
    alone beats typed tools by 4.8–24.5 points at 19–72% fewer tokens
    with no detectable gain from adding the catalog back, while a
    repository-issue-fixing study found CodeAct-style Python beats
    bash-only by 56.3% fewer tokens at similar resolve rates; the shell
    lever is real, its size moves with the workload and the model.)
   Replaced "allow-list of bare programs" with "any command
   inside an OS sandbox": Seatbelt on macOS, bubblewrap on Linux
   (Landlock needs `unsafe`, which the workspace forbids), writes
   confined to workspace + temp + cargo home, network off by default,
   the credential scrub floor kept. The shipped command policy became
   the escalation layer (`network`, `extra_writable`, `sandbox =
   false`, plus the existing timeout/output-window/env-scrub limits).
   Bounded head+tail capture and process-group kill kept; bare names
   resolve on `PATH` before wrapping so missing-program errors stay
   legible. Seam: `run_command`, `verify`, and the policy module.
3. **Command-aware output filters** (G2, G3). RTK-style per-command
   reducers for *successful* and general output (git status/diff/log,
   ls/tree, grep, test passes) — RTK reports a 56% average reduction —
   with the full output teed to an artifact on failure so nothing is
   lost. Evidence shaping already does this for failed builds; this
   generalizes it. Seam: `loom-tools` render tables, the artifact store.
4. **Bootstrap facts** (G2, G3, G4). Extend the facts block with a
   project instruction file (`AGENTS.md`, falling back to `CLAUDE.md`;
   bounded), a depth-limited file tree, and toolchain/package-manager
   versions. Meta-Harness found exactly this snapshot by search and
   measured 2–5 early turns saved. Still frozen per task, so the
   cached prefix is unaffected. Seam: `FsFactsSource`.
   **Shipped 2026-10-08** — tree depth 4 / 200-line cap, cargo·rustc /
   node·npm / python3 / go probes, AGENTS.md→CLAUDE.md 4 KiB head.
   Measured on the free-tier suite (15 × 1): gated 11/14 → 13/14,
   B9 cap-fail → solved in 8 turns, list_dir ~14/run → 1/run.
5. **Edits that need no follow-up** (G1, G2, G4).
   - **Multi-edit**: `edits: [{old_string, new_string}]`, applied
     atomically, across one or more files.
   - **Result shows the edited region** (±N lines), so the model never
     re-reads to confirm (SWE-agent, Claude Code).
   - **Syntax guard**: a tree-sitter parse of the edited file before
     writing; an edit that breaks parsing is refused with the error
     location (SWE-agent's lint-guarded edit: 18.0 vs 15.0 resolved).
   Exact unique-match replace stays the default format: general
   whitespace-fuzzy matching was measured never to fire (the narrow
   trim-only repair in item 6 covers the one observed failure shape), and anchor formats (hashline)
   help weak models but cost strong models 8–38% more. Format becomes a
   per-model profile setting (item 9), chosen by measurement. Seam:
   `edit_file`/`write_file`.
   **Multi-edit + echo shipped 2026-10-08** (phase A): `edits:
   [{old_string, new_string, path?}]` batches atomically across files
   (all-or-nothing, in order, per-edit uniqueness), and every result
   echoes the edited region (±3 numbered lines, 8 KiB cap). A/B on the
   edit-heavy tasks (B3/B4/B6, 1 run each, free tier): all solved,
   turns 27 → 21, input tokens −21%, post-edit re-reads 1 across three
   runs (corpus baseline ≈ 4/run), batch form adopted on first contact.
   Syntax guard (tree-sitter) deferred: heavy dependency; the verify
   fusion already covers Rust parse failures.
6. **Pre-dispatch call repair and cheap re-reads** (G2, G3, G4).
   - Repair malformed tool calls before dispatch: argument aliases
     (`path`/`file_path`), stringified JSON objects, whitespace-padded
     `old_string` with exactly one trimmed match (ForgeCode). Every
     repair saves a failed round trip; repairs are recorded in effects
     so the eval can count them.
   - A tool-cache hit returns `unchanged since call <id>` instead of
     the whole content again, while the earlier result is still in the
     projection unstubbed.
   Seam: between stop-reason validation and `plan_turn`; the tool cache.
    **Shipped 2026-10-08** — the two halves the corpus justified:
    whitespace-trim repair for `edit_file` `old_string` (43 not-found
    failures in the 84-run corpus; recorded in `effects.repairs` and
    counted by `loom stats`) and the `unchanged since call <id>` stub
    for repeat read-only calls while the original result is still in
    the projection. Argument aliases and stringified-JSON repair were
    skipped: zero occurrences in the same corpus. A/B on nemotron
    (B3/B6/B9): all solved; 5 stub hits across 4 runs; 0 repairs — the
    item-5 echo already removed the padding trigger for this model, so
    the repair is a net for models that do pad.
7. **Search and read shape** (G2). `search` grouped by file with a
   files-only mode and optional context lines; a `glob` tool. Measure
   the read window: SWE-agent found a ~100-line window beat both 30
   lines and whole files, while Loom defaults to 2000. Seam: `search`,
   `read_file`, a new read-only tool.
   **Evidence-based skip (2026-10-08).** A default-100 probe (B1/B3/B6
   ×1, same model) measured 2–3.5× WORSE than the 2000-line default:
   B3 5→15 turns / 14.2K→49.5K input tokens, B1 4→14 / 42.7K→86.2K.
   Paging replays the transcript per page, and with the bootstrap tree
   (item 4) and `search` already narrowing reads, models self-bound
   (median explicit window 34 lines). The corpus also showed `search`
   results already lean (median 2.5 KB, ~2/run, grouped-by-file above
   the 100-match threshold), so files-only/context-line modes and `glob`
   have no measurable lever here. Reverted; the 2000-line default with
   explicit `offset`/`limit` stands.
8. **Completion gate and plan** (G1, G3, G4).
   - On `EndTurn`, if files changed since the last green `verify`,
     refuse once with the diff summary and the original instruction
     (Terminus-KIRA's double confirmation, SWE-agent's
     `review_on_submit`, LangChain's pre-finish checklist: +13.7 points
     from harness changes only).
   - A `todo` tool; the runtime re-appends the current list at the tail
     of the projection when it changes, so the goal never sinks into
     mid-history. Enforced plans took ForgeCode from 38% to 66%;
     planning cut cost ~30% for strong models (Fan et al.).
   Seam: the stop-reason branch of the loop; a kernel-owned tool.

   **Refusal half shipped 2026-10-08 (phase A).** The EndTurn gate
   refuses once when files changed since the last green check
   (`exit status: 0` from a command-shaped result; fused write+verify
   satisfies it in one step). A/B on the free tier: the refusal fired
   exactly once on the checkless canary (+2 turns) and never on the
   synthesis task — the gate is cheap insurance, not a turn-saver. The
   `todo` tool stays deferred.

### Mid term — context and model policy

9. **Model profiles** (G4, G5, G7). A `ModelProfile` beside
   `ModelCapabilities`, carried as data: edit format, read window,
   reasoning-effort schedule, tool argument names. Includes a
   **progressive reasoning policy** — high effort for the opening turns
   and for verification, low in between (ForgeCode). The loop sets
   effort per turn; adapters only translate it. Seam: provider
   capabilities and the request compiler.

   **Evidence-based deferral (2026-10-08).** No measurable lever exists
   today: the OpenRouter catalog lists zero reasoning-capable free
   models (and the adapter deliberately never sends OpenRouter's
   `reasoning` object — wrong for some upstream models), the funded
   DeepSeek key is drained, tool-argument-name aliases failed 0 times
   in the 838-call corpus (item 6 survey), the 2000-line read default
   won its A/B (item 7), and Loom has a single edit format. Revisit
   when a funded DeepSeek key returns, a reasoning-capable free model
   appears, or item 14's multi-provider work makes profiles structural.
   **Revisit trigger fired (2026-10-09):** item 14 shipped Anthropic
   models with adaptive reasoning that Loom does not yet drive, so the
   reasoning-effort half is now structural. It is still unmeasured — no
   funded Anthropic key is wired into the eval — so it stays gated on
   item 1's funded-model prerequisite. This is the first concrete step
   of Iteration 8 and the main lever for G5. Mechanism check
    (2026-10-09, current Anthropic docs): on the 5.5 lineup thinking is
    always on and adaptive — `thinking: {"type": "disabled"}` and manual
    `budget_tokens` both return 400 — and depth is steered by
    `output_config.effort` (low → max; default medium on Opus 5.5).
    Two mechanics shape the profile design: the resolved effort is
    rendered into the prompt, so changing it between requests busts
    cache breakpoints — a progressive reasoning schedule must use the
    per-turn system-message effort form (Fable 5.1+) that applies to the
    next assistant turn without a cache bust — and thinking counts
    toward `max_tokens`, so the output budget needs headroom at high
    effort. **Transport shipped (wiremock-verified):** the Anthropic
    adapter keeps post-conversation-start system messages in place as
    in-turn `role: "system"` turns (leading ones still fold into the
    top-level `system` param), and `Usage` now carries
    `thinking_tokens` + `cache_creation_input_tokens` for attribution —
    what a schedule needs to observe its own cost. The schedule itself
    (a kernel-side effort policy emitting those in-turn notes) remains
    open, gated on item 1.
10. **Pruning, phase 2** (G2).
    - **Turn-age masking**: stub read-only results older than K turns
      even below the window trigger — SWE-agent's last-5-observations
      ablation beat full history (18.0 vs 15.0), and an 80% trigger on a
      1M-token window never fires.
    - **Cold-cache pruning**: when the gap since the last request
      exceeds the provider's cache TTL, the prefix misses anyway, so
      re-decide for free, bypassing the delta-reclaim gate (Codebuff).
    - **Reasoning clearing**: drop reasoning older than N turns in the
      same pass (Anthropic `clear_thinking`), where providers allow it.
    - **Usage-calibrated estimates**: correct the estimator against
      the previous turn's billed `input_tokens` (Sema Code). The o200k
      estimator (shipped 2026-10-06) already replaced bytes÷3, so this
      is now a residual correction, mainly for Anthropic's scaled
      estimate.
    - Stubs stay one line: models rarely use recovery hints (Fan et al.).
    Every addition keeps the checkpoint freeze: one cache miss per
    decision. Seam: `PruningContextCompiler`, census.
    **Turn-age masking shipped 2026-10-09** (default K=8, `PruneConfig::
    age_mask_turns`; episodic via the same reclaim gate, checkpoint freeze
    kept). A/B on the longest tasks (B9/B14): dormant at current transcript
    sizes — no stub fired, both solved, no regression — so it ships as
    bounded-growth insurance, not a measured win. **Cold-cache pruning,
    reasoning clearing, and usage-calibrated estimates deferred with
    evidence**: eval requests are back-to-back (gap < cache TTL), the free
    eval model emits no reasoning traces, and estimates already track billed
    input within ±20% (median ratios: nemotron 0.97, deepseek 0.88, deepseek
    large requests 1.22 — conservative exactly where triggers matter;
    measured on the bytes÷3 heuristic, before the o200k switch, so the
    current error is smaller still).
11. **Deterministic openers** (G2, G3). Runtime-executed steps before
    the first model turn, chosen by task shape: read files the prompt
    names, `git diff` for changed files, a baseline `verify` to capture
    pre-existing failures. Their results enter the transcript as
    ordinary tool results, so replay and pruning need no special case
    (Codebuff's programmatic steps). Seam: the loop's setup phase.
     **Evidence-based deferral (2026-10-09).** Corpus survey (13 nemotron
     records, post-items-4-8 stack): task text names a file only twice per
     suite (read-the-named-file ≈ dead); run/verify calls within the first
     three calls ≈ 0.85/run (baseline-verify saves ~1 turn); facts already
     carry the git snapshot and tree, and eval fixtures are fresh workspaces
     (git-diff opener ≈ no-op). The lever is below the free-model variance
     floor — B9 spans 6–40 turns on identical config — so an A/B cannot
     demonstrate benefit, and the suite sits at 13/14 gated. Revisit when a
     funded low-variance model is wired in or the corpus grows dirty-repo
     tasks.
12. **Handoff compaction** (G2). The last resort when non-prunable
    history outgrows the window: instead of recursive in-place
    summarization (which Codex reports degrades accuracy), generate a
    goal-directed handoff — task, decisions, open problems, relevant
    files — and continue in a fresh projection (Amp's Handoff). One
    cache miss, frozen like a prune checkpoint. Prefer provider-native
    compaction where an adapter has it. Seam: a final strategy in the
    pruning decorator.
     **Evidence-based deferral (2026-10-09).** Handoff sits behind the
     pruning trigger, and the harness's largest transcript (B14, the
     longest task) grows to a 62 KB final request ≈ 21K est-tokens — below
     the 64K floor trigger measured during item 10's scoping. Pruning is
     dormant on this stack, so its last-resort fallback is strictly
     dominated. Revisit when corpora or funded long-horizon sessions
     actually cross the window. Evidence refreshed 2026-10-09:
     provider-native compaction is now concrete — Anthropic ships the
     `compact_20260112` context edit (auto-triggers at a token
     threshold, returns a typed compaction block, server drops
     everything before it), which the "prefer provider-native" clause
     should target first. The fallback shape is also measured now:
     CliffCompaction (arXiv 2609.26779) shows rule-based truncation —
     drop tool results over ~500 chars, reduce old calls to signatures,
     discard prior compaction blocks instead of summarizing summaries —
     matching Claude-Code-style summarize+microcompact quality at lower
     cost, cache-friendly, with zero auxiliary LLM calls (tool results
     are 56% and tool calls 28% of context tokens on their Terminal-Bench
     2.0 unpacking — exactly the buckets Loom's stubs already clear);
     AutoCompact shows model-invoked compaction at phase transitions
     wins even when a 256K window never fills.
13. **Background commands** (G1, G2). Long builds and tests run
    detached; the runtime injects a completion notice at the next turn
    boundary instead of blocking a turn on them (Sema Code, Crush). Seam:
    the bounded exec and turn assembly.

     **Evidence-based deferral (2026-10-09).** Corpus survey of the
     post-items-4-8 records: zero command calls are batched with
     independent tool calls in the same assistant message (the runtime
     already executes same-message calls concurrently), and every
     non-command follow-up lands on the next turn — after the result is
     visible. In the harness's edit→verify loops the command result is
     the input to the next decision, so detaching adds a poll protocol
     without removing a wait; waits cost wall time, not turns (G1/G2
     unaffected). The lever is interactive wall-time — a user queueing
     work while builds run — which needs the interactive CLI (items 15,
     19) to matter. It also injects wall-clock-dependent notices at turn
     boundaries, breaking the byte-stable census and replay equivalence
     the eval harness is built on. Revisit when interactive sessions
     exist, or a corpus shows same-message command+work batching.
14. **Non-OpenAI provider** (G7). An Anthropic-messages adapter to
    pressure the `ModelProvider` boundary. The compiler emits
    **segments marked cacheable or volatile**, so cache breakpoints are
    placed by the adapter and the kernel stays provider-blind (OpenDev).
    Capability discovery copies the OpenRouter catalog pattern.
    **Shipped 2026-10-09.** `AnthropicProvider` (+ pure
    `anthropic_wire` translation and wiremock tests) speaks the
    messages API: system as a top-level param, tool_use/tool_result
    blocks, thinking-delta reasoning capture, Anthropic error
    taxonomy, and `cache_control` breakpoints derived from the new
    provider-agnostic `ModelRequest.stable_prefix_messages` hint
    (`#[serde(default)]`; the default compiler sets it to the whole
    projection, pruning inherits the guarantee). It runs its own
    streaming state machine rather than refactoring the OpenAI wire
    driver (drift risk over ~100 duplicated lines, noted in the
    module docs). Capabilities come from a small static table because
    Anthropic's /v1/models exposes no capability fields to reduce.
    Verified offline (wiremock) — no live key. **Engine/CLI wiring
    shipped 2026-10-09** (same day, second pass): `--provider
    anthropic` resolves `ANTHROPIC_API_KEY` through the same
    account/env ladder as the other providers, and the capability
    table carries the current lineup — `claude-fable-5-1`,
    `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`
    (1M context, 128K output, adaptive reasoning) alongside the 4.x
    legacy entries; dateless aliases are refused (they float).
    `temperature` is never forwarded (4.7+ models 400 on non-default
    values), and preserved thinking is a non-issue because Loom never
    re-emits thinking blocks. Anthropic token estimation is scaled
    1.25× over o200k, rounded up (`AnthropicEstimator`), so the 1M
    window's budget math errs toward pruning early rather than
    sending an over-window request.
15. **CLI session resume.** Listing, selection, continuation, and
    transcript export (also the input for item 19).
16. **Search parity.** Adopt the `ignore` crate's matcher for chained
    gitignore files or pin the documented negation-precedence
    divergence with real-repository fixtures.
17. **Artifacts lifecycle.** Reference counting from live transcripts,
    an age/size-bounded GC, and a size-reporting command.

### Later — structure and self-improvement

18. **Symbol tools** (G1, G3). Tree-sitter `symbols_overview` and
    `replace_symbol` for Rust, TypeScript, and Python; LSP-backed
    references and rename after that (Serena). One call replaces a
    search→read→edit chain. Builds on the item 5 parser.
19. **Delegation by measured economics** (G6). A read-only scout on a
    cheap model that returns a ≤1 KB answer (Codebuff file-picker, Amp
    Librarian), one level deep only. Delegate when estimated parent
    context saved × remaining parent turns exceeds the subagent's cost —
    computed from the census and cost data the loop already records,
    and checked by the eval. Requires mid-task model selection for the
    child only; the parent's model stays fixed per task. Seam: the
    engine composes a child `Agent`; leases and cancellation already
    exist.
20. **Self-optimization** (G10). In order:
    1. trace export: one directory per task with transcript, events,
       census, harness version, and outcome — **phase 1 shipped**
       (`loom run --trace DIR`: `events.ndjson`, `transcript.json`,
       `manifest.json`) and **phase 2 shipped** (event schema v3's
       `ContextPruned`: trigger arm + per-call mechanism + estimated
       reclaim, emitted once per checkpoint freeze — mechanism-level
       attribution from any trace) and **phase 3 shipped**
       (`loom stats --trace DIR` aggregates exported traces directly:
       billed usage from manifests, per-trigger and per-reason prune
       attribution from `context_pruned` events — no server-side
       persistence needed);
    2. every tunable — prune trigger and floors, read window, reasoning
       schedule, edit format — moved from constants into one config
       struct, so the replay and live evals can sweep it (this subsumes
       adaptive pruning thresholds);
    3. an outer loop in the Meta-Harness shape that reads *raw* traces,
       harness source, and scores and proposes changes (compressed
       feedback measured worse);
    4. lessons: recurring failure→fix pairs mined from history into
       short, expiring rules injected through the facts block (gptme
       lessons, `rtk learn`). This is the long-term memory item; the
       hard part is staleness and eviction, not storage.
21. **Parallel orchestration.** Worktree-per-agent isolation with a
    merge path, above the engine. Only after item 19 shows delegation
    pays.
22. **Frontends.** A TUI or desktop client, additive once resume ships.
23. **Distributed verification.** Remote or containerized `verify`
    runners with the same evidence contract.

### Deliberately not planned

Each would add surface without evidence it serves the goals (G8):

- **Embedding/vector retrieval.** None of eleven harnesses in a 2026
  source study uses it; deterministic search plus symbols suffices.
- **Agent frameworks, multi-level subagent trees.** One level is enough
  where delegation pays at all.
- **MCP/skills marketplace and deferred tool loading.** Schema
  filtering saves tokens only with large tool menus (21–57K
  tokens/turn at ~30 tools; nothing at ~9; a controlled 2026 study
  measured eager 44-tool schema transmission at ~217K median input
  tokens per run versus ~71K with on-demand fetching, and the
  programmatic-vs-JSON token crossover sits near 26 tools — both
  bracket the same threshold). MCP and on-demand skills
  themselves are Iterations 5–6; deferred tool *loading* becomes worth
  measuring only once those push the menu past ~30 tools.
- **Hashline or patch formats as the default edit.** Per-model profile
  option only, if measurement favors it.
- **More per-provider tokenizers.** o200k covers the OpenAI-compatible
  providers and a scaled o200k covers Anthropic; usage calibration
  (item 10) closes the remaining gap if measurement ever shows one.

### Open questions

- Should the failure ledger loosen for idempotent read-only retries
  after non-deterministic errors, and should it tighten to near-repeats
  (same tool and path failing three times with different arguments)?
- Where does user-facing configuration live (sandbox escalations,
  provider defaults, model profiles): per-workspace manifest, user
  config, or both with defined precedence?
- What sandbox escape hatch, if any, is acceptable for network-needing
  commands (dependency fetches) without reintroducing an allow-list?

---

## Maintenance bar (ongoing)

- All tests green, clippy clean, `cargo fmt --check` clean.
- No new blocking calls on async workers; hashing never under locks.
- Projection stays byte-stable between events (eval asserts it — keep
  it that way).
- Any new model-visible byte passes the output budget with an explicit
  marker.
- Any change claimed to save turns or tokens ships with a live-eval
  comparison (medians, N ≥ 5) against the previous baseline.
