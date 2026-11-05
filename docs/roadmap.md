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
3. **Backlog** — numbered, evidence-gated items (1–31), each mapped to an
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
| 2 | Core efficiency & repository intelligence | **Done 2026-10-23** (funded-measurement debt in item 1) | 1, 3✓, 4✓, 5✓, 6✓, 7✗, 8✓, 10✓ᴬ, 11⏸, 12✓, 16✓, 17✓ |
| 3 | Intelligent subagent scheduling | Not started | 19, 21, 24 |
| 4 | Execution safety & richer tools | **Largely shipped early** | 2✓, 13⏸, 23 |
| 5 | Skills & contextual knowledge | **Largely shipped 2026-10-23** (skills index + `load_skill`) | 4✓ (instructions), skills✓, 20.4 |
| 6 | MCP & LSP integration | Started 2026-10-23 (item 18 symbol tools shipped) | 18✓ |
| 7 | Plugin system | Future scope | — |
| 8 | Intelligent model routing | Not started; prerequisites in place | 9⏸, 14✓ |
| 9 | Learning & self-optimization | Not started; data path partial | 1, 20 (20.2 sweepable set shipped) |
| — | Product surface (cross-cutting) | Partial | 15, 22, 25 |

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
warnings in the loom-eval harness). Shipped adapters are
DeepSeek, OpenRouter, Anthropic, GLM, NVIDIA, and Groq. The GLM adapter
remains wiremock-verified against the Z.ai spec with no direct Z.ai
key configured; GLM-5.3 is nonetheless reachable and live today
through OpenRouter on a metered paid key (2026-10-18), which is the
path the eval uses.

### Iteration 2 — Core efficiency & repository intelligence (current)

Make Loom materially better than a basic agent loop without large new
subsystems:

- **Advanced context compilation** — recoverable pruning, checkpoints,
  historical selection, reasoning-inclusion policy, better estimation,
  cache-friendly construction, references to archived artifacts.
  *Shipped:* pruning v1 + turn-age masking, frozen checkpoints, artifact
  refs, o200k estimation, assistant-text elision + LLM handoff (item 12).
  *Open:* the deferred tail of item 10 (cold-cache pruning, reasoning
  clearing, usage-calibrated estimates — parked with evidence until a
  funded model or cache-gap workload reopens it).
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
  dependency update solved with **no** model invocation. *Shipped
  (2026-10-10):* the analyzer covers rename / constant change / format
  with anchored grammars (never a fuzzy guess), whole-workspace guard
  checks, snapshot-and-revert on any failure, and verification through
  the same policy-gated sandbox as `run` — a miss leaves the workspace
  byte-identical and falls through to the agent loop, so the path is
  never worse than its absence. The engine books a solve as a zero-
  request turn (`FastPathHit`, event schema v7), and the eval suite
  grew the FPATH slice (tier s: constant, rename, format) to keep the
  lever measured. *Shipped (2026-10-10, second slice):* dependency
  updates (Cargo manifest class: `update/bump <crate> to <version>`,
  strict table-scoped rewrite of simple `name = "x.y.z"` entries,
  verified offline with `cargo metadata --no-deps --offline`) and
  ecosystem routing for format (`gofmt -l` + `go build` verify on Go
  manifests; the format grammar now says which toolchain it means).
  npm/Python/Make formats decline — no deterministic offline
  formatter/verifier for the edit class — and non-Cargo dependency
  bumps decline the same way. *Closed (2026-10-24):* non-Cargo
  dependency updates stay declined on evidence — no candidate
  ecosystem has an offline manifest-only verifier (npm
  `--package-lock-only --offline` still resolves against the
  registry/cache; `go list -m` demands `go mod download`; pip's
  resolvers always consult an index; probed 2026-10-24, pinned by
  `npm_dependency_update_declines_to_the_agent_loop`). Revisit only if
  an ecosystem grows a parse-only verifier or networked verification
  gets a policy story.
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
shows delegation pays. Item 24 (best-of-N sampling) is the test-time
sibling: parallel rollouts of the *same* task, no delegation semantics.

### Iteration 4 — Execution safety & richer tools

Pulled forward because the shell lever was large and measured. *Shipped:*
`edit_file`/`write_file`/`run_command`/`verify`, OS sandbox (Seatbelt /
bubblewrap) with network off by default, the command manifest as the
escalation layer, credential scrubbing, process-group kill, `CommandEffect`
audit records, structured `git` tools (`git_status`/`git_diff`/`git_log`
— read-only porcelain with deterministic flags, opt-in via `--git`,
2026-10-23), and the verdict layer on top of the manifest
(`[[deny]]` program blocklist, `[policy] unknown = "allow"|"ask"|"deny"`
for unruled programs; `ask` fails closed without an interactive
frontend — the resolver seam ships, the frontend is item 22; defaults
byte-identical, 2026-10-23), and containerized/remote `verify` runners
via a declared `.loom/verify.toml` runner command — same evidence
contract, fail-closed on invalid manifests, unconstrained by necessity
with an explicit `verify.runner` notice (2026-10-23). *Open:* the
delegated-economics measurement over item 21's rollout reports.

### Iteration 5 — Skills & contextual knowledge

Knowledge loaded on demand, not injected globally. *Shipped:* always-on
project instructions (`AGENTS.md` → `CLAUDE.md`, 4 KiB head) through the
facts block; the `.agents/skills/` index (2026-10-23) — the facts block
lists skill names plus one-line descriptions (first heading of each
`*.md`, flat scan, ≤64 entries), and the opt-in `load_skill` tool
(`--skills`) serves bodies on demand at a 16 KiB head, through the same
context-compiler seam, never a second prompt mechanism. *Open:* learned
lessons (item 20.4) reuse the same seam. The 2026 skill-memory line sets
the shape for what a learned skill may be (Memp, arXiv 2508.06433;
MUSE-Autoskill, arXiv 2605.27366): bounded procedural instructions or
executable script skills — never freeform notes — lifecycle-managed
(create/evaluate/refine/retire, so the library cannot monotonically
grow), retrieval-gated (SWE-ContextBench, arXiv 2602.08316: bad
retrieval gives negative transfer), and compilable once by a strong
model then served to cheaper tiers (Memp's strong→weak transfer, G4).

### Iteration 6 — MCP & LSP integration

MCP servers attach through the existing `Tool` trait; nothing MCP-specific
leaks into the loop. The MCP design is researched and pinned in
[`docs/mcp-design.md`](mcp-design.md): stdio-only transport, manifest
allowlist with hash-pinned tool definitions (rug-pull defense), ingestion
hygiene for untrusted descriptions, and every MCP tool deferred behind a
search/load meta-tool pair with idle offboarding (the ~30-tool threshold
noted under "Measured out" is crossed by construction the moment a real
server attaches — one GitHub server is 35 tools). **Shipped 2026-10-23**:
D1 (stdio client, manifest allowlist, hygiene, pinning), D2 (deferred
surface: facts index, `mcp_search_tools`/`mcp_load_tools`, idle
eviction), and D3 (measurement probe: 35-tool mock GitHub server, both
arms completed, **12.4× upfront tool-token cut** at ≥10× bar —
`docs/daily-driver.md`), opt-in via `--mcp`. **Shipped 2026-10-25**:
remote servers via declared proxy commands — a `.loom/mcp.toml` entry
whose `command` is a local relay (`ssh -W`, `tailscale`, a TCP shunt)
runs under the same sandbox+hygiene contract, with `network = true`
required for the relay and `LOOM_WORKSPACE` exported so proxies can
resolve workspace paths (tests: `proxy_command_reaches_remote_server`,
`proxy_command_without_network_grant_fails_closed`). LSP supplies definition, references, symbols,
diagnostics, rename, type information — justified only by fewer
exploratory model turns. The LSP design is researched and pinned in
[`docs/lsp-design.md`](lsp-design.md): generic async-lsp client plus a
built-in server table (the nvim-lspconfig/mason move — breadth as data,
auto-discovery, no auto-install), manifest allowlist and sandbox reuse,
lazy document sync fed by engine edit effects, position conversion done
explicitly, ~8 fixed tools with Serena-style result shortening, rename
deferred to E2 behind WorkspaceEdit application, codeAction/executeCommand
excluded on the MCP-puppet rationale. Tree-sitter symbol tools (item 18)
came first and shipped 2026-10-23 — opt-in via `--symbols`. **Shipped
2026-10-23 (E1)**: the generic client (async-lsp, sandboxed stdio,
applyEdit declined), the `KNOWN_SERVERS` table with `.loom/lsp.toml`
overrides, document sync (lazy didOpen + edit-effect-fed didChange), and
six semantic read tools (definition/references/symbols/workspace-symbols/
hover/diagnostics) with utf-16-safe positions and pull-else-push
diagnostics — opt-in via `--lsp`; tree-sitter `--symbols` stays as the
zero-dependency floor. **Shipped 2026-10-23 (E2)**: rename + format
with WorkspaceEdit application through loom's write path (bottom-up
edits, overlap rejection, version stale-guard, ResourceOps honored),
per-server `settings` serving, `applyEdit` still declined. **Shipped
2026-10-24 (E3)**: `lsp_call_hierarchy` (prepare + incoming/outgoing,
capability-gated) plus the $0 turn-economy probe — rename-heavy fixture,
real rust-analyzer, three labeled runs; all arms success at equal
exploratory turns, with the honest caveat that the nano-tier model
elects semantic tools stochastically (one run never called them; the
rerun's single `lsp_rename` renamed six documents in one call, 3 fixup
edits vs 20). Iteration 6 complete: un-gated engineering exhausted.

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
Current iteration:     2 closed 2026-10-23 (efficiency engineering complete;
                      funded-measurement debt recorded in item 1); iteration
                      3 (subagent scheduling) not started
Language:              Rust (stable), 8 crates, unsafe forbidden
Shipped providers:     DeepSeek, OpenRouter (free + paid), Anthropic, GLM,
                      NVIDIA (NIM hosted inference), Groq (hosted inference)
GLM status:            reachable via OpenRouter (z-ai/glm-5.3, metered, live-
                      verified 2026-10-18); the direct Z.ai API is still
                      wiremock-only (no Z.ai key on this machine)
NVIDIA status:         NIM adapter shipped and wiremock-verified, but the
                      z-ai/glm-5.3 completion endpoint stopped answering
                      (HTTP 000 at 45s and 120s, 2026-10-18) while the
                      catalog still lists 81 models; DeepSeek V4 NIM rows EOL'd
Groq status:           adapter shipped, wiremock-verified, and live-probed
                      2026-10-18 on a free-plan key (openai/gpt-oss-120b,
                      openai/gpt-oss-20b, qwen/qwen3.8-27b); unbilled, and the
                      binding free-plan ceiling is 8K tokens/min
Eval models in use:    DeepSeek `deepseek-flash` on a funded adapter (1M
                      window, cache-aware usage parsing) — the v3-100 sweep
                      completed 2026-10-23: 90 resolved / 99 completed,
                      $2.1 total, 96.9% cache-read; account balance $1.12
Primary target:        successful work per model invocation
Biggest open risk:     funded measurement debt (item 1): the N ≥ 5 re-runs
                      for items 4/5/6/8/10 cost ≈$3+, the first bare-vs-full
                      contrast (10 tasks × 2 arms × N=2) ≈$0.84, and the
                      balance is $1.12 — engineering seams are done and
                      validated; only credit is missing
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
  prefix cache and the wire-level replay tests both lean on.
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
- **Verification bar.** 694 tests passing (13 live-API tests ignored by
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
A 2026-11 sweep of 74 harness papers
([`docs/lit-review-2026-11-harness-papers.md`](lit-review-2026-11-harness-papers.md))
adds four more framing findings and backlog items 26–31 below.

### Near term — measurement and the cheapest wins

1. **Efficiency eval, phase 2** (G9, G10). The suite measures outcome
   and cost; extend it so it can *rank harness changes*:
   - a 30-task hard subset as the iteration gate (the Meta-Harness
     protocol), then external slices (SWE-bench Verified,
     Terminal-Bench) for comparability — half shipped 2026-10-10: the
     corpus gained the **scale tier** (NAV1 many-reads, BIG1
     large-file, MULTI1 cross-file, LONG1 heavy long-horizon — the only
     in-house task that crosses the pruning floor) and every field task
     now carries an s/m/l/xl difficulty tier with a per-tier pass rate
     in the suite headline; ~~`multiswe-import` (loom-eval bin) bridges
     Multi-SWE-bench JSONL into `EXT-*` tasks (clone at base commit,
     apply the test patch, gate on the instance's own F2P/P2P), so an
     imported Lite-80-style slice is a command away~~ — removed
     2026-10-20 together with the local suite; measurement is now
     SWE-bench Verified in container isolation (see “Measurement”).
     Shipped
     2026-10-10, the second half: the **external slice is imported and
     validated** — 5 instances across 4 real repos (tokio-rs/bytes ×2,
     rayon-rs/rayon, clap-rs/clap at 803 P2P, tokio-rs/tracing),
     shallow-fetched at the exact base commit (one commit, zero refs —
     no post-fix history to mine), issue title+body as the prompt, gold
     patch outside the workspace, seeds proven by the offline
     `bugfix_seeds_fail_their_gates` like any in-house task; two
     candidate repos (ripgrep, fd) dropped with machine-checkable
     reasons — upstream cfg-gates their own P2P tests off macOS, so
     they are ungradeable on darwin without editing benchmark files,
     which the no-benchmaxxing rule forbids. And the **formal hard
     gate shipped**: `LOOM_EVAL_TASKS=hard-gate` — a derived preset
     (every gated task, heavies included, lab benches never; grows
     only by addition, so a new task cannot be silently left out) —
     24 tasks today: B-corpus + scale tier + B7/LONG1 + 5 EXT;
   - ~~headline **$ and turns per solved task**, cache-read ratio, and
     tool-failure rate alongside pass rate~~ — shipped: the suite
     headline now carries an `economics` block (total and per-solved
     cost, turns per solved mean/median, token-weighted cache-read
     ratio over successful runs, micro-eval tool-failure rate);
     extended 2026-10-10 with the SWE-Effi / Scaffold-Effect metrics —
     tokens per solved amortized over all billed runs, the
     expensive-failure ratio (share of spend on unresolved gated runs;
     field data says failures burn multiples of successes), the
     failure-category vector (`end_reason` counts plus `gate_failed`),
     and idle turns / idle-turn rate (the no-tool-call assistant turn
     tax);
     **Preamble tokens per turn** as a headline economics metric
     (system prompt + tool schemas billed on every step): the harness-cost
     study (arXiv 2610.04433) found fixed per-step preamble × step count
     dominates harness cost differences (16.5K Claude Code vs 830
     mini-SWE-agent), while pass-rate deltas were mostly noise — this is
     the discriminating metric for any future tool-menu or
     system-prompt growth, and it is already computable from the
     per-source census. Track it in the economics block going forward;
   - ~~per-tool, per-model **failure-mode micro-evals** (edit-apply
     success, malformed-call rate, repeated-call rate) as release
     gates~~ — shipped offline half: every run report (success or
     failure) embeds a `micro` block (per-tool fault taxonomy, edit
     acceptance, identical repeats, spills), recomputable from the
     session snapshots the local eval persisted under
     `records/sessions/` and cross-checked by an offline test~~ — the
     micro-eval machinery was removed with the local suite
     (2026-10-20); the same signals can be re-derived from SWE-bench
     run artifacts when they earn their keep; the
     30-task gate shipped 2026-10-10 (`LOOM_EVAL_TASKS=hard-gate`,
     derived — see item 1); the per-model comparison dimension remains;
   - `loom stats`: what each mechanism saved — pruning, tool-cache hits,
     evidence shaping, output filters — from the events already
     recorded (RTK's `gain`). **Shipped 2026-10-17**: the transcript
     half had been reporting turns, per-tool call quality, edit
     acceptance, repeats, and spill savings from any stored session,
     but mechanism attribution was waiting on persisted events rather
     than transcripts. Those events now exist (store schema v2), and
     `mechanisms_overview(&store, &store, …)` folds prune reports and
     fast-path hits out of the event stream while stub hits, omission
     markers, and repairs still come from the transcript — same id set,
     so both sections describe the same sessions.
   - **Paired same-task contrast reporting**: report arm comparisons
     as per-task paired deltas — win/loss/tie on the gate plus
     per-task turns/tokens/cost deltas on the same seed and workspace
     snapshot — next to the suite medians, not instead of them. Arms
     already run identical tasks, so this costs zero new runs; it
     exists because the noise floor is the local enemy (B9: 6–40 turns
     on identical config) and pairing cancels the task variance that
     aggregate medians hide. Harness² (arXiv 2610.14224) probes
     candidate harnesses on the same task for exactly this reason —
     behavioral contrast between executions is its only selection
     signal; here it strengthens claims at whatever N the budget
     affords. Paired deltas become the default presentation of any A/B
     once the funded model lands. **Shipped 2026-10-23 for SWE-bench
     artifacts**: `swebench-contrast` (loom-eval bin) reads two official
     scorer reports plus their eval dirs and renders per-instance
     outcome glyphs, win/loss/tie, and Δturns/Δtokens/Δrequests/Δcost
     (run-1 artifacts; pricing flags optional). Validated on the
     v3-100 vs post-fix overlap (6 shared instances): 3→5 resolved,
     0 wins / 2 losses / 4 ties, matching the scorer verdicts exactly.
   - **Funded low-variance eval model** — the gating prerequisite.
     The free tier's variance (B9: 6–40 turns on identical config)
     sits above most levers, so items 4, 5, 6, 8, and 10 shipped on
     N=1 A/Bs that do not meet the maintenance bar (N ≥ 5 medians).
     Once funded: re-baseline and re-run those A/Bs at N ≥ 5, and
     record which claims survive. **Status 2026-10-18: the credential
     is wired and the model is live; the account balance is the
     blocker.** OpenRouter serves `z-ai/glm-5.3` on a metered *paid*
     key (`is_free_tier: false`), resolved through the saved keychain
     account, and a real streaming eval turn returns HTTP 200. What
     is exhausted is credit: `$15` granted, `$14.963` spent, so a
     live run dies with HTTP 402 *"requires more credits, or fewer
     max_tokens"* (`report-B3-z-ai-glm-5.3.json`). The measured
     prices are ~$0.06/M prompt and $4.40/M output, so at the
     suite's cache-heavy shape (~16.4K tokens per solved run, 84%
     cache read) one run costs **~$0.03** and the 150-run full-vs-bare
     sweep (15 tasks × N=5 × 2 arms) costs **~$4.40**. A ~$5 top-up
     clears it. The account also carries a **$1/day guardrail**
     (`workspace_default_member` + `workspace_default_key`, daily,
     `spend_usd: 0` at reset), so even funded the sweep paces itself
     across ~5 days unless the guardrail is raised — `LOOM_EVAL_RPM`
     meters requests, and the daily budget is the coarser ceiling.
     **Status 2026-10-23: the funded model landed, on DeepSeek instead.**
     The dedicated `deepseek-flash` adapter (1M window, cache-hit token
     parsing) ran the full v3-100 SWE-bench Verified sweep: 90 resolved /
     99 completed for $2.1 at 96.9% cache-read (~$0.021 per instance), and
     the follow-up validation sweeps confirmed the malformed-salvage and
     test-consultation-nudge fixes (f682374). What remains open is only
     the re-baseline debt at today's balance of $1.12: the N ≥ 5 re-runs
     for items 4/5/6/8/10 cost ≈$3+, and the first bare-vs-full contrast
     (10 tasks × 2 arms × N=2) ≈$0.84 — skipped on budget and recorded
     here rather than run thin (2026-10-23 decision).
     Candidate alternatives, re-checked 2026-10-18: the direct Z.ai
     API is still wiremock-only (no Z.ai key on this machine);
     DeepSeek is drained (`is_available: false`); **NVIDIA NIM is no
     longer a zero-cost candidate** — the adapter is shipped and
     wiremock-verified, and `/v1/models` still lists 81 models
     including `z-ai/glm-5.3`, but `POST /v1/chat/completions` returns
     nothing (HTTP 000 at both 45s and 120s). That outage is the
     cause of all 11 `model_error` records in the on-disk corpus: the
     infra failures were a dead endpoint, not sampling noise, and the
     new `is_infra_run` classification is what makes them legible.
   - ~~**Basic-harness baseline and model tiers**~~ (G4, G5, G9) —
     shipped 2026-10-10, the A/B half: `LOOM_EVAL_BARE=1` runs the
     same suite on the bare loop (read/list/write/edit/shell only; no
     facts, pruning, evidence shaping, or completion gate; plain
     append-only compiler), artifacts suffixed `-bare` so both arms
     coexist. Engine seam: `EngineOverrides::bare_loop()` +
     `Agent::with_loop_shaping(false)`. The funded A/B runs moved
     2026-10-10 to NVIDIA NIM (z-ai/glm-5.3, free tier, keychain
     credential) after the DeepSeek key drained mid-run (HTTP 402).
     **The A/B was never actually executed**: there is no `-bare`
     report in `target/eval/`, so the G4 claim that Loom raises a
     weaker model's success rate is still unmeasured. NIM went dark
     afterward (above), so the arms now wait on OpenRouter credit
     rather than on engineering — the arm is shipped, pinned by
     `bare_registry_excludes_the_kernel_todo_tool`, and
     `suite_headline` already reads `arm: "bare"`. **2026-10-23: the seam is
     wired end-to-end for SWE-bench** — `swebench-run --arm bare` (or
     `LOOM_EVAL_ARM`) forwards `LOOM_BARE=1` through the frozen container
     script into the CLI, which applies `EngineOverrides::bare_loop()`;
     the script itself stays byte-identical (asserted by test), and
     `swebench-contrast` renders the paired arms once both exist. The
     G4/G5 number still waits on credit, not engineering. **2026-10-23
     (later): first G4/G5 contrast EXECUTED at $0** — vehicle: the
     user's nano gateway (custom OpenAI-compatible provider,
     `kilo/nvidia/nemotron-3.5-lightning:free-1M-txt`), 5 instances x
     1 run per arm (django-11239, sphinx-7748, scikit-learn-14141,
     sympy-13974, astropy-12907), official scorer both arms,
     `swebench-contrast` over the paired artifacts. Result: **full 1/5,
     bare 0/5** — the single resolution (scikit-learn-14141) belongs to
     the harness arm, which spent +71 turns on it (completion-gate
     persistence iterating a wandering weak model until the patch
     scored); bare exited earlier with a failing patch. Directionally
     supports G4/G5, but N=1 x 5 is far below the N>=5 medians bar:
     recorded as a probe, not maintenance evidence. Vehicle history:
     NIM rate-stormed structurally (40 RPM free cap), the OpenRouter
     key proved invalid, the nano gateway held at -j2 with 91.5-92.3%
     cache. The number this
     must produce is the G4/G5 headline in `docs/benchmarks/`: the
     claim that Loom raises a weaker model's success rate, which the
     input model profiles (item 9) also need. ~~Also shipped alongside:
     the deterministic **editor bench** (`editbench`, token-free, PR-Edit
     style) pinning `edit_file` competence across file sizes,
     ambiguity refusal, batch atomicity, and the oversize cap on every
     commit~~ — removed 2026-10-20 with the local suite; edit
     competence now has to show up in the SWE-bench numbers.
   - **Persist events** alongside transcripts (also item 20.1), so
     `loom stats` can attribute savings per mechanism. ~~Shipped
     2026-10-17.~~ Store schema v2 writes the observed event stream
     alongside the transcript, and item 1's `loom stats` section above
     is the consumer.
   - **Frozen-subset suite audit** (2026-11): UTBoost (arXiv 2506.09289)
     amplified SWE-bench's own suites and found 345
     erroneously-passing patches — 24.4% of Verified leaderboard entries
     affected — so the subset's official P2P/F2P verdicts are hypotheses
     too. One-off deterministic re-grade of the frozen 100 with
     amplified suites; weak-suite instances flagged in the provenance
     block; any headline that flips gets re-gated. Mapped in
     [`docs/lit-review-2026-11-harness-papers.md`](lit-review-2026-11-harness-papers.md).
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
   **Shipped 2026-10-10** — `run_command` only (verify already owned
   green runs): git status/diff/log, ls/tree, grep, and cargo-test
   pass lists reduce at the command's own unit boundaries (commit
   blocks, lines, passing-test runs) with counted omission markers —
   never byte cuts mid-line. Reductions apply only when an artifact
   store is attached; the raw stream is teed there on both paths
   (success-reduced and failure-shaped evidence) with a refetch note,
   so nothing is lost. No store → raw output, pre-filter behavior.
   14 tests (reducer units + store/tee round-trips).
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
   **Syntax guard shipped 2026-10-16 (phase B)** — which overtook the
   deferral rather than resolving it. `crates/loom-tools/src/syntax.rs`
   parses the file with rust-tree-sitter *before* the write, so a
   malformed edit is refused at the boundary instead of after a failed
   build; `LOOM_SYNTAX_GUARD=0` opts out. The dependency that was once
   "too heavy" turned out cheaper than the round trip it replaces: a
   locally-parsing edit costs no turn, while a `cargo check` that fails
   afterwards costs the whole check-plus-read cycle. The verify fusion
   still covers what a parser cannot see (type errors, behaviour), so
   both halves stand.
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
   **`todo` tool shipped 2026-10-17 (phase B)**, which overtook the
   deferral above. The tool is kernel-owned (not a provider tool spec),
   which is the point: the goal is loop state, not a capability the
   model may ignore or the bare arm may drop. State lives at
   `.loom/todo` (`TODO_STATE_PATH`) and the runtime re-appends the
   current list under a stable `TODO_BLOCK_HEADER` ("## todo (kernel)")
   when it changes, so the goal never sinks into mid-history. Event
   schema v9 and per-mechanism stats landed with it; the bare arm
   excludes the tool on purpose (a basic harness does not get one), and
   that exclusion is pinned.

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
   **Routing slice zero (planned):** task-*start* model selection — a
   trivial classifier (task text → difficulty tier) that picks the model
   before `Agent` construction. Needs no Iteration-8 ADR because the
   model-fixed-per-task invariant is untouched: the invariant governs
   mid-task swaps, and selection happens before the first request.
   Evidence: LangChain reports 64% median cost cut with no quality loss
   from start-of-thread routing (hypothesis until Loom's eval
   reproduces). Blocked on item 1's funded-model prerequisite like the
   rest of this item.
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
    - **Phase-aware retention** (2026-11): stub exploration reads earlier
      once the loop deterministically detects the writing phase (recent
      turns carry `edit_file`/`write_file` calls) — lifecycle-aware
      eviction per TokenPilot; selfopt-gated like every other stub class.
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
     demonstrate benefit, and the suite sits at 13/14 gated. The first
     revisit trigger is now half-met: a low-variance model is reachable
     (OpenRouter `z-ai/glm-5.3`, live) but the account has no credit for
     an N≥5 sweep (item 1), so the A/B still cannot be paid for. Revisit
     when eval budget is restored or the corpus grows dirty-repo tasks —
     the second trigger is the stronger one, since the deferral's real
     cause was task shape, not variance.
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
     **Deterministic half shipped (2026-10-10).** The pruning decorator
     now elides CliffCompaction-style without any LLM calls: aged
     tool-call arguments collapse to signatures (long strings become
     `[pruned N chars]`, identifying short fields survive), and user
     items superseded by a later user item stub out — with the session
     brief (ordinal 0) and the *last* user item (the live task's
     instructions under follow-up tasks) exempt by construction.
     Decisions freeze into the existing `PruneCheckpoint` behind the
     same 20K-reclaim gate and re-apply verbatim (byte-stable, one cache
     miss per freeze). What remains for handoff: assistant text and
     reasoning — still never reduced, so the last-resort handoff stays
     for sessions whose non-prunable tail alone outgrows the window.
     **Reasoning half shipped (2026-10-10, provider-gated).** Aged
     assistant reasoning traces now drop from the projection too
     (`reasoning = None`, same 8-turn window and freeze) — but only
     where omitting replayed reasoning is provably safe: GLM (loses
     only the Preserved-Thinking cache benefit) and Anthropic (never
     forwards reasoning on replay) get it via the engine's provider
     gate; DeepSeek stays off because a tool-bearing request whose
     prior turns lack `reasoning_content` fails with HTTP 400
     (live-verified). Assistant text was then the last unpruned bucket.
     **Assistant-text elision + LLM handoff shipped (2026-10-10) — item 12
     complete.** Aged assistant *text* now collapses deterministically
     (CliffCompaction-style, ahead of any LLM): outside the protected
     tail, text older than 8 turns exceeding 400 chars keeps its first
     200 chars plus a ` [pruned N chars]` marker — idempotent, frozen
     into the same checkpoint, re-applied verbatim (byte-stable). When
     the pruned view *still* overruns the budget, the loop performs the
     last-resort handoff: it builds a goal-directed digest (task, brief,
     recent assistant messages, recent failed tool results, touched
     files) and makes exactly one provider call — no tools, 1024-token
     cap, plain-text end-turn responses only — for a handoff summary;
     the frozen checkpoint projects a view transcript (brief + handoff
     as a fresh user item + tail), skipping ordinal-keyed elisions in
     the view since call-id elisions are position-independent. One
     handoff per task; a second overrun fails closed. Emits
     `ContextHandoff` (event schema v6); gated by
     `AgentConfig::handoff_compaction` and disabled in the engine's
     bare arm. Remaining nicety, not scope: provider-native
     context editing when an adapter gains it — Anthropic
     `compact_20260112` (auto-triggers at a token threshold) and
     `clear_tool_uses_20250919` (server-side tool-result clearing, the
     same shape as Loom's stubs; caveat: invalidates prompt-cache
     prefixes, so it belongs behind the same 20K-reclaim gate), plus
     `context_management.compact_threshold` and the stateless
     `/responses/compact` endpoint on the OpenAI-compatible side (free
     reach for four of five adapters) — the in-house path above is the
     default everywhere today.
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
    transcript export (also the input for item 19). **Shipped 2026-10-16**:
    `Engine::continue_session` + `AwaitingInput` event (2026-10-09), then the
    CLI surface — `loom sessions` lists persisted sessions (id, transcript
    length, last-task status, straight from `SqliteStore`, no provider/credentials
    needed) and `loom run --session ID TASK…` reappends to that session,
    reusing every `run` flag. A live smoke resumed a completed session into a
    new task end to end. Transcript *export* remains (folded into item 20's
    `--trace`).
16. **Search parity.** Adopt the `ignore` crate's matcher for chained
    gitignore files or pin the documented negation-precedence
    divergence with real-repository fixtures.
    **Shipped 2026-10-16** — the item offered a fork (adopt the
    matcher, or pin the divergence) and took the second branch for the
    one case where the two disagree, while adopting the matcher for
    everything else. `search.rs` compiles one `Gitignore` per ancestor
    `.gitignore` between the workspace root and the scope, deepest-first
    so the nearest file wins exactly as git resolves it, and applies them
    through the walk's `filter_entry`. Six tests pin the behaviour on
    repo-shaped fixtures: `whole_tree_walk_matches_git_on_nested_negation`,
    `chained_ignores_subdir_overrides_parent`,
    `nested_path_styled_rule_anchors_at_its_own_directory`,
    `repo_shaped_tree_scoped_and_whole_agree`,
    `scoped_search_nearest_ignore_file_wins`, and the one deliberate
    divergence — `scoped_whitelist_below_scope_is_pruned`. A whitelist
    inside the scope cannot re-include what an ancestor excludes; that
    is *stricter* than git, and it is pinned rather than left implicit
    because a quieter strictness is a property, not a bug.
17. **Artifacts lifecycle.** Reference counting from live transcripts,
    an age/size-bounded GC, and a size-reporting command.
    **Shipped 2026-10-10** — `ArtifactStore` grew `list`/`delete`
    (first-write times on both backends); `artifact_gc` walks every
    stored transcript for the live set, then deletes only orphans past
    the backstop bounds (30 days age, 256 MiB total, oldest first — a
    referenced body is never deleted). The engine runs the pass at
    open, best-effort; `loom stats` reports bodies, live/orphan split,
    and reclaimable bytes, store-wide (references from any session keep
    a body alive). 8 tests across the kernel and store crates.

### Later — structure and self-improvement

18. **Symbol tools** (G1, G3). Tree-sitter `symbols_overview` and
    `replace_symbol` for Rust, TypeScript, and Python; LSP-backed
    references and rename after that (Serena). One call replaces a
    search→read→edit chain. Builds on the item 5 parser.
    **Shipped 2026-10-23 (iteration 6, first step).** Both tools live
    in `loom-tools::symbols`, opt-in via `EngineConfig::symbols` /
    `loom run --symbols` (default off, so the frozen SWE-bench
    protocol and every existing frontend are unchanged; the bare-loop
    arm never carries them). `symbols_overview {path, depth}` renders
    a bounded (8 KB, then per-kind counts) nesting-aware listing —
    Rust fns/structs/enums/traits/impls/mods/consts/macros (impls
    named by their type, so methods address as `Config/new`), Python
    functions/classes with nested defs (replacement spans widened to
    include decorators), TypeScript/Tsx functions/classes/interfaces/
    enums/type aliases/arrow-function declarators. `replace_symbol
    {path, name_path, body}` splices whole definition bodies with
    `/`-separated suffix addressing (a leading `/` anchors at top
    level), refuses ambiguous or missing matches with the candidates
    listed, re-runs the item 5 parse guard before the single atomic
    write, and records the write in its effects. Measurement —
    whether the tools cut exploratory turns — is item 1's funded
    debt, not a gate on the seam.
19. **Delegation by measured economics** (G6). A read-only scout on a
    cheap model that returns a ≤1 KB answer (Codebuff file-picker, Amp
    Librarian), one level deep only. Delegate when estimated parent
    context saved × remaining parent turns exceeds the subagent's cost —
    computed from the census and cost data the loop already records,
    and checked by the eval. Requires mid-task model selection for the
    child only; the parent's model stays fixed per task. Seam: the
    engine composes a child `Agent`; leases and cancellation already
    exist.
    **Shipped 2026-10-23 (iteration 3, first step).** The seam and hard
    bounds are in: `ScoutConfig` on `EngineConfig` (opt-in, `None` by
    default so the frozen SWE-bench protocol is untouched) composes a
    child `Agent` on its own provider/model with a read-only registry
    (read/list/search/artifact only — no writes, no shell, no recursion:
    the child has no scout tool), `loop_shaping` off, an 8-turn ceiling,
    a wall-clock timeout (default 120 s), and a 1 KB truncated answer.
    Cancellation rides the parent's tool context. CLI: `loom run
    --scout PROVIDER --scout-model MODEL`. The delegation-economics
    trigger stays parked with item 1's funded-measurement debt — the
    runtime ships the seam, the eval measures when credit allows.
    **Extended 2026-10-25**: multi-model collaboration on the same one
    level — deterministic cascade escalation, cross-check, ≤3-question
    fan-out, provenance footers, per
    [`docs/subagents-design.md`](subagents-design.md); the economics
    trigger stays parked.
    **Prior (2026-11, arXiv 2503.13657):** MAST's taxonomy puts 36.9% of
    multi-agent failures on inter-agent misalignment and 21.3% on
    superficial task verification — structural costs of delegation
    itself, not implementation accidents. The item-19 trigger must clear
    the coordination-failure tax, not just the token arithmetic.
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
       struct, so sweeps over the SWE-bench protocol can vary it (this
       subsumes
       adaptive pruning thresholds). **Sweepable set shipped
       2026-10-23**: `ToolBudgets` (all per-tool output budgets,
       including the edit-echo shape) travels in `EngineConfig`, and
       the prune trigger/floor/reclaim/tail layer over the provider
       base via pure env seams (`LOOM_*` vars) the frozen container
       script never sets — defaults byte-identical. The
       reasoning-schedule slot stays gated with item 9;
    3. an outer loop in the Meta-Harness shape that reads *raw* traces,
       harness source, and scores and proposes changes (compressed
       feedback measured worse). **Shipped 2026-10-25 (offline seam)**:
       `selfopt` (loom-eval) runs the Propose–Probe–Compose loop over
       existing sweep artifacts — rule-based single-knob proposals plus
       a one-shot model proposer that reads per-instance medians,
       turn/event traces, and ledger lessons; probes are docker re-runs
       with `LOOM_*` env overlays through `ContainerSpec::extra_env`
       (frozen script untouched); adoption gate = median input-token
       improvement ≥5%, event-count non-regression ≥−10%, failures
       non-increasing; independently-winning overlays are merged and
       the merge re-probed; lessons enter the ledger only with
       contrast evidence (Harness² rule). Design:
       [`docs/selfopt-design.md`](selfopt-design.md);
    4. lessons: recurring failure→fix pairs mined from history into
       short, expiring rules injected through the facts block (gptme
       lessons, `rtk learn`). This is the long-term memory item; the
       hard part is staleness and eviction, not storage. Implementation
       reference: the provider-native memory-tool shape (Anthropic
       `memory_20260818` context edit; Claude Code's agent-written
       `MEMORY.md`) is the same pattern at the file level — plain files
       under model control, injected through a seam Loom already has.

    **Harness² input (2026-10-20, arXiv 2610.14224).** Google's
    recursive-harnessing framework is the same thesis with the editing
    inverted — the model edits its own harness per task, offline here
    instead. Three things transfer. Phases 2–3 get their search shape
    from its Propose–Probe–Compose: propose invariant (task-agnostic)
    and domain (task-class) edits separately, probe candidates on the
    same task, then **compose** — combine independently-winning edits
    and re-probe the combination, because edits interact (prune
    trigger × read window × tool menu); per-lever winners are not a
    shippable config. Its evidence rule becomes phase 4's adoption
    gate: a lesson enters the facts seam only with before/after
    contrast evidence from paired runs, carries an expiry, and is
    re-validated or evicted — staleness answered structurally, not by
    hope. And its split gives the landing zone: invariant lessons
    become defaults in the config struct, domain lessons become
    task-class profiles. The online half (model-edited harness at
    task time, k× the token cost) is deliberately not taken: it breaks
    the deterministic-controller principle and the never-worse
    contract. Recursion stays offline in the eval harness, where the
    maintenance bar rules.
    5. **selfopt output shape and instance selection** (2026-11): emit
       the Pareto-ranked overlay set as cost tiers (`--tier lite|heavy`),
       not just the merged winner — HarnessPro (arXiv 2610.26770) beats
       RHO (+2.0 pp pass, −26.1% cost) shipping exactly that product;
       weight probes toward prior-failure instances (RHO's hard-task
       coreset, arXiv 2606.05922) for more signal per probe dollar; and
       if the ledger shows premature convergence (proposals rejected on
       the event-count gate that later variants would pass), split
       candidate promotion from deployment selection (ParetoSaddler,
       arXiv 2611.02079) — the arithmetic gate stays for deployment.
21. **Parallel orchestration.** **Shipped 2026-10-25** —
    `engine::worktrees` best-of-N rollouts, CLI `--best-of N` (2..=8,
    semaphore-bounded, opt-in): N detached git worktrees under
    `.loom/worktrees/`, N full child engines (same task, frozen
    compiler, worktree-local DB, per-child provider from a
    resolved-once credential), deterministic winner selection —
    completion (`end_turn`/`fast_path`), then fewest failure markers
    (loop nudges, failed tool calls, task failures; the `micro`
    taxonomy Harness² uses for its all-rollouts-fail fallback), then
    turns, then model requests — winner patch applied via `git apply`,
    worktrees kept on apply failure for forensics, fail-closed outside
    a git repository. Item 24's vary-the-harness dimensioning and the
    delegated-economics measurement remain open on top of it.
22. **Frontends.** A TUI or desktop client. The facade contract is now
    ready: `loom-core` roots a stable surface (driving API + `TaskControl`/
    `EventSink`/`EventKind`/`Session`/ids) with a documented semver split
    from the rolling crate aliases (ADR-021), and `examples/frontend.rs`
    shows the full drive loop — open, subscribe, steer, summarize. Resume +
    steering are both shipped, so a frontend is purely additive.
23. **Distributed verification.** Remote or containerized `verify`
    runners with the same evidence contract. **Shipped 2026-10-23** —
    `.loom/verify.toml` declares the runner argv (docker, ssh, …) with
    `{workspace}`/`{program}`/`{args}` placeholders; both the standalone
    `verify` tool and the fused edit/write verification path route
    through it, the evidence pipeline is unchanged, and a broken
    manifest fails closed rather than silently verifying locally.
24. **Best-of-N task-time sampling** (G1, G9). Fork the prepared
    workspace snapshot into N independent child agents (same model,
    varied approach hints), run them concurrently in the existing OS
    sandbox, and select the first rollout whose deterministic outcome
    gate passes — the gate, not the model, judges. Evidence: RTV +
    summary-conditioned refinement lifted Opus 4.5 from 70.9%→77.6%
    SWE-bench Verified and 46.9%→59.1% Terminal-Bench v2.0 while
    halving steps (arXiv 2604.16529); S*-style hybrid
    parallel+iterative allocation beat majority vote by 10–16 pp
    (arXiv 2502.14382). Caveats that shape the design: weak selectors
    can make fixed Best-of-3 *worse* than Best-of-1 (arXiv 2608.03961)
    — using the deterministic gate as the selector avoids that failure
    mode entirely; grading must use tests the agent cannot edit
    (Loom's gate already does); N multiplies tokens, so start at N=2-3
    on hard-gate failures only (adaptive budgeting, not fixed N), and
    measure tokens-per-*solved*-task, not per run. Prerequisites all
    shipped: byte-stable projection, sandbox fork, deterministic gates,
    usage accounting. Sibling of item 21 (parallel rollouts, no
    delegation semantics); blocked on item 1's funded model like
    everything that multiplies spend. Seam: the engine composing N
    child `Agent`s above `Agent::run_task`, same as item 19.

    **Harness² input (2026-10-20, arXiv 2610.14224).** Two additions
    from its contrastive selection. All-rollouts-fail fallback: pick
    the rollout whose trace carries the fewest failure-mode markers —
    identical repeats, edit rejections, idle turns, already counted by
    the `micro` taxonomy — a deterministic selector for the case the
    gate cannot decide, dodging the weak-selector regression above.
    And vary the harness, not just approach hints: facts verbosity,
    todo granularity, reasoning-effort schedule are Harness²-style
    domain edits that diversify rollouts without changing the model;
    the N traces are then free contrast evidence for item 20's lesson
    mining.
25. **Queued user steering** (G1, G2). **Shipped 2026-10-16** — the
    `TaskControl::steer` seam queues user messages on the control (shared via
    `Arc` across clones); the turn loop drains them at each turn boundary and
    appends them persist-then-commit before that turn's compile, so census,
    replay, and the eval harness stay byte-stable and unaffected. New event
    schema **v10** adds `SteeringApplied { turn, messages }` (one per
    non-empty boundary drain, after `TurnStarted`). Distinct from item 15's
    pause: no stop, an injected instruction the model sees next turn. The CLI
    does not yet expose an interactive feed — it remains a library seam for
    future frontends (item 22).

### Late 2026 — arXiv-survey items (26–31)

From the 2026-11 literature sweep (74 papers, mapped in
[`docs/lit-review-2026-11-harness-papers.md`](lit-review-2026-11-harness-papers.md));
same rules as every item — evidence-gated, measured on the frozen subset
before anything is default-on. Four findings frame them. Turn count
explains 84% of log trajectory cost and 98.6% of input tokens hit the
prefix cache — turns and invocations, not observation bytes, are the
cost unit (TurnCost, arXiv 2610.15093). Selection, not coverage, is the
test-time-scaling bottleneck (arXiv 2407.21787, 2502.01839, 2501.14723).
LLM-summarized compaction damages 5–64% of continuations and retains
≤16% of recurring details vs 79.1% for mechanical extraction — with
DeepSeek-Flash the worst of 14 summarizer backbones, so the stub floor
is permanent (Compact-Bench, arXiv 2610.31710). And orchestration buys
cheap models cost cuts, not quality lifts — quality returns need the
model tier under them (arXiv 2607.06906).

26. **Dynamic turn budget** (G1, G2, G9). Replace the static
    `max_turns` with a staged grant: initial budget (per-model P75 once
    item 9 profiles exist), extended only on deterministic progress
    evidence (files changed since the last check, a new green verify, a
    new evidence class). Evidence: fixed P75 limits cut cost 24–68% at
    ~flat solve; dynamic grants add 12–24% more (arXiv 2510.16786);
    per-model thresholds are mandatory (Gemini collapsed under extreme
    limits). Seam: the loop's turn gate + `EngineConfig`. Probe before
    default-on — the paper's evidence is associational.

    **Implementation shipped 2026-11, default-off.** `TurnBudget {
    initial_turns, extension_turns, max_extensions }` on `Agent` and
    `EngineConfig`; `max_turns` stays the hard ceiling. Evidence read
    from `CompletionGate` state the loop already keeps — a file
    mutation since the grant (its mutation epoch; successful commands
    do not count), or mutations still pending against the full suite
    (truncated inside the verify loop). Event schema v12 adds
    `TurnBudgetExtended`; env seams `LOOM_INITIAL_TURNS` /
    `LOOM_EXTENSION_TURNS` / `LOOM_MAX_TURN_EXTENSIONS` are registered
    selfopt knobs. One design correction from the first test run: the
    zero-progress counter counts any successful non-read-only call, so
    `echo` would have counted as progress — the mutation epoch is the
    honest signal. Default remains inert until the paired probe
    (frozen subset, free gateway, N=5 medians) reproduces the
    2510.16786 direction; the probe is the item's remaining open tail.
27. **Guarded decision reuse** (G1, G2, G3). Cache the model's resolved
    next action and skip the following reasoning call when tool
    feedback confirms the stored decision is still valid — principle 1
    extended from tool scheduling to decision scheduling. Evidence:
    HarnessMM (arXiv 2610.32813) — −29.5% tokens at flat success
    (Codex) and −87.9% with success up (DeepSeek) on HumanEval/MBPP;
    toy single-file benches, so SWE-bench-class transfer is exactly
    what the probe must answer. Guard primitives: continue an in-flight
    batch-edit plan while the parse guard holds; skip re-deliberation
    when only file ids/mtimes changed; repeat-verify after
    non-mutating turns. Seam: a controller decision cache over the
    existing effects/side-channel state.
28. **Shared-prefix best-of-N fork** (G1, G2, G9). Items 21/24 run N
    children from turn 0; fork them from a shared exploratory prefix
    (facts compile + first K turns) instead — sessions persist and the
    projection is byte-stable, so the shared turns are paid once and
    hit the provider prefix cache in every child. Evidence: SWE-Replay
    (arXiv 2601.22129): −17.4% cost, +3.8% resolve vs naive scaling.
    Risk: correlated failures from lost diversity — re-measure on the
    frozen subset. Seam: `engine::worktrees` + kernel session fork.
29. **Execution-based selection filter** (G1, G9). Before item 21's
    ranking, run the existing fixed-table `verify` in each candidate
    worktree and drop candidates that fail the existing suite — no
    model call, fails closed, kills the "winner breaks the suite"
    class. Evidence: the selector, not coverage, caps best-of-N value
    (arXiv 2501.14723, 2407.21787), and weak selectors can make
    Best-of-3 worse than Best-of-1 (arXiv 2608.03961 — already item
    24's caveat). Count ranking ties in the selection report;
    model-judged tie-breaks are revisited only if ties measurably
    dominate.
30. **Per-task tool-menu policy** (G2, G3, G4, G5). The facts scan
    already detects languages and build systems deterministically —
    use them to narrow the tool menu before turn 1 (no cargo tooling in
    an npm workspace), land the model-profile menu from item 9 on top
    (bash-capable models do better and cheaper with leaner menus,
    arXiv 2609.20804), and add table-driven stall-breakers: the failure
    ledger already detects near-repeats, so N failed edits on one file
    deterministically inject that file's `symbols_overview` next turn —
    context, not a model call (TokenEconomizer, arXiv 2610.22732:
    −18–31% tokens, Pass@1 up in 4/5 settings). Prune only on certain

    facts; a wrongly pruned tool is a task loss.
    *Implementation shipped 2026-11:* (a) the symbol pair is pruned at
    engine open when the facts scan finds none of Rust/Python/TypeScript
    — the opt-in flag no longer registers dead schema bytes; (b) the
    table-driven stall-breaker: two consecutive failed edit-family calls
    on one file (`EDIT_STALL_TURNS`; the failure ledger already refuses
    the identical retry, so a second *distinct* failure is a shape
    problem) injects that file's `symbols_overview` outline as a
    `LoopNudge` — deterministic context, zero model calls, once per
    stall, only when the symbol tools are registered so the frozen
    baseline surface is unchanged. The model-profile menu from item 9
    stays gated on its funded eval key. Open tail: live-eval deltas
    (tokens, turns-to-recovery) on the frozen subset.
31. **Symbol-cued search output** (G1, G2, G4). Attach a one-line
    enclosing-symbol + sibling map (tree-sitter from item 18) to each
    `search` match group: "correct file, wrong function" causes 44–58%
    of function-level misses, and organizing the evidence — not adding
    tools — narrows the gap (RepoNav, arXiv 2609.08355). Lives inside
    item 7's 2026-10-08 skip boundary: leaner results at the current
    2.5 KB median, one line per group, output-budget marker mandatory;
    measured on NAV1.
    *Implementation shipped 2026-11:* deepest enclosing symbol only (the
    sibling map was cut — more bytes, no evidence); cue is `[in
    Class/method]`, capped at 64 bytes, absent on lines outside any
    declaration (imports, whitespace) rather than guessed. Cues are
    computed on the search walk's blocking worker (one capped re-read +
    tree-sitter parse per hit file), default-on with a
    `LOOM_SEARCH_SYMBOL_CUES=0` opt-out mirroring the syntax guard.
    NAV1 no longer exists (2026-10-20 suite replacement); the open tail
    is the frozen-subset sweep measuring whether cues cut
    refine/re-search turns.


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
- **Image/multimodal input.** No leading-harness evidence that images
  move coding-agent outcomes on text-shaped benchmarks, and every
  model-visible byte would bypass the text census and elision machinery.
  Revisit trigger: an eval task class that provably needs image input
  (UI screenshots, architecture diagrams) on a multimodal funded model.
- **LLM-summarized or model-managed context.** Compact-Bench (arXiv
  2610.31710): summaries retain ≤16% of recurring details vs 79.1%
  mechanical, damaging 5–64% of continuations — DeepSeek-Flash is the
  worst of 14 summarizer backbones; Source-Judged Compaction (arXiv
  2610.20927): 83% of compaction failures trace to summary entries with
  no source in environment output; the Complexity Trap (arXiv
  2508.21433): masking ≈ summarization. The stub floor is permanent.
- **Code-space harness self-modification.** ADAS (arXiv 2408.08435),
  Darwin Gödel Machine (arXiv 2505.22954): the empirical-validation
  loop is selfopt's; the arbitrary-code edit surface is what G10's
  "without uncontrolled self-modification" forbids. Bounded overlays
  and (if ever needed) declarative policy rules are the line.
- **Activation-level harness compilation.** Latent Harness (arXiv
  2611.01415), agentic soft tokens (arXiv 2610.41291): need
  activation access to the served model — breaks G7 for every hosted
  provider. Research-plugin note only if local open-weight serving ever
  lands.
- **Model-judged rollout selection.** Rubrics (arXiv 2601.04171) and
  RETRACE (arXiv 2608.08950) spend invocations on judgment; prompt-based
  reranking adds little over a deterministic gate (item 24's evidence)
  and weak selectors make Best-of-3 worse than Best-of-1 (arXiv
  2608.03961). Revisit only on measured ranking ties (item 29).

### Open questions

- ~~Should the failure ledger loosen for idempotent read-only retries
  after non-deterministic errors, and should it tighten to near-repeats
  (same tool and path failing three times with different arguments)?~~
  **Resolved 2026-10-25**: loosen only for read-only tools after
  transient errors (rate-limit / network class, the same signal the
  eval retry loops key on); the near-repeat tightening is declined —
  varied-argument retries are legitimate exploration, and a gate
  there trades recovered tasks for speculative token savings without
  evidence. Revisit only if a live eval shows failure-ledger stops
  blocking a recovery that later succeeds.
- ~~Where does user-facing configuration live (sandbox escalations,
  provider defaults, model profiles): per-workspace manifest, user
  config, or both with defined precedence?~~ **Resolved 2026-10-25**:
  per-workspace manifests stay the only file-level config (audit
  surface travels with the repo); credentials and provider selection
  stay environment-level. No user config file until two-plus
  workspaces in daily use actually demand it — then workspace >
  user precedence, the Claude Code scope ordering, arrives as an ADR.
- ~~What sandbox escape hatch, if any, is acceptable for
  network-needing commands (dependency fetches) without reintroducing
  an allow-list?~~ **Resolved 2026-10-25**: the existing per-command
  escalation IS the hatch — `.loom/commands.toml` rules carry an
  explicit `network = true` (loudest possible grant, user-declared,
  reviewable in one file). No new mechanism; MCP servers get the same
  field, and the remote-proxy work confirmed the shape carries
  (relay needs `network = true`, fails closed without it).

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
