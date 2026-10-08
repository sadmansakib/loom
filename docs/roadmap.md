# Loom Roadmap

Status of the Loom coding-agent runtime. Two lists: what is **shipped** and
verified in the codebase today, and what is **not yet shipped** — planned
work ordered roughly by priority, with the design seam each piece will plug
into. The companion `architecture.md` describes how the shipped system
works; this document tracks direction.

---

## Goals

Every planned item below is justified against these. An item that
serves none of them does not ship.

1. **G1** Maximize successful work per model invocation.
2. **G2** Minimize unnecessary model turns.
3. **G3** Minimize irrelevant context and token consumption.
4. **G4** Move deterministic work from the model into the runtime.
5. **G5** Make individual model turns high-bandwidth and actionable.
6. **G6** Delegate only when subagent economics justify it.
7. **G7** Keep model/provider/tool concerns cleanly separated.
8. **G8** Build an extensible, incremental Rust core without
   speculative complexity.
9. **G9** Measure execution efficiency empirically.
10. **G10** Eventually let Loom optimize how it solves tasks from its
    own execution data.

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
  extra cache misses and each trigger costs exactly one. A projection
  that overruns the budget outright is rescued by a forced prune —
  decide stubs, recompile a stubbed view of the same transcript — and
  fails closed only when nothing is prunable or the pruned view still
  does not fit.

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
  *and* tail — errors live at the tail. A command timeout kills the
  entire process group, so grandchildren (`rustc` under `cargo`)
  cannot outlive the kill.
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
- **Command execution policy.** `run_command` runs under a workspace
  policy: the shipped default allows exactly `cargo`, and a
  `.loom/commands.toml` manifest extends it with per-rule limits — a
  wall-time cap (the whole process group is killed at the expiry and
  the model sees the error), a per-stream output window, and extra
  environment scrubbing. Provider credential env names are scrubbed
  from every spawned child regardless of manifest; every executed
  command is recorded as a structured `CommandEffect` in the session
  transcript, so the session is the audit log.
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
- **Verification bar.** ~515 tests (13 live-API tests ignored by
  default), clippy clean, fmt clean.

---

## Not yet shipped

Ordered by expected value against the goals. Each item names the goals
it serves, the evidence behind it, and the seam it plugs into; none
requires unwinding shipped invariants. Evidence is drawn from a survey
of harnesses with working userbases (Claude Code, Codex CLI, OpenCode,
Aider, Pi, mini-SWE-agent, SWE-agent, OpenHands, ForgeCode,
Terminus-KIRA, Codebuff, Amp, Crush, gptme, Plandex, Goose, OpenDev,
Sema Code, oh-my-pi, Serena, RTK, Meta-Harness) and the published
ablations behind them. Two findings frame the whole list: on
Terminal-Bench the *harness* moves the same model by 16–25 points, and
harnesses in the same score tier differ in cost by up to 4×. So every
item is gated on measurement (item 1), and vendor-reported numbers are
treated as hypotheses until Loom's own eval reproduces them.

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
   Seam: the live eval reports and the event stream.
2. **Sandboxed shell** (G2, G3, G5) — **shipped 2026-10-08**. Strong
   models working through a
   shell are ~53% cheaper than through fixed tool menus (Fan et al.,
   2026); Pi, mini-SWE-agent, and Terminus run mostly or only on a
   shell. Replaced "allow-list of bare programs" with "any command
   inside an OS sandbox": Seatbelt on macOS, bubblewrap on Linux
   (Landlock needs `unsafe`, which the workspace forbids), writes
   confined to workspace + temp + cargo home, network off by default,
   the credential scrub floor kept. The shipped command policy became
   the escalation layer (`network`, `extra_writable`, `sandbox =
   false`, plus the existing timeout/output-window/env-scrub limits).
   Bounded head+tail capture and process-group kill kept; bare names
   resolve on `PATH` before wrapping so missing-program errors stay
   legible. Seam: `run_command`, `verify`, and the policy module.
3. **Command-aware output filters** (G3, G4). RTK-style per-command
   reducers for *successful* and general output (git status/diff/log,
   ls/tree, grep, test passes) — RTK reports a 56% average reduction —
   with the full output teed to an artifact on failure so nothing is
   lost. Evidence shaping already does this for failed builds; this
   generalizes it. Seam: `loom-tools` render tables, the artifact store.
4. **Bootstrap facts** (G2, G4). Extend the facts block with a
   project instruction file (`AGENTS.md`, falling back to `CLAUDE.md`;
   bounded), a depth-limited file tree, and toolchain/package-manager
   versions. Meta-Harness found exactly this snapshot by search and
   measured 2–5 early turns saved. Still frozen per task, so the
   cached prefix is unaffected. Seam: `FsFactsSource`.
   **Shipped 2026-10-08** — tree depth 4 / 200-line cap, cargo·rustc /
   node·npm / python3 / go probes, AGENTS.md→CLAUDE.md 4 KiB head.
   Measured on the free-tier suite (15 × 1): gated 11/14 → 13/14,
   B9 cap-fail → solved in 8 turns, list_dir ~14/run → 1/run.
5. **Edits that need no follow-up** (G1, G2, G5).
   - **Multi-edit**: `edits: [{old_string, new_string}]`, applied
     atomically, across one or more files.
   - **Result shows the edited region** (±N lines), so the model never
     re-reads to confirm (SWE-agent, Claude Code).
   - **Syntax guard**: a tree-sitter parse of the edited file before
     writing; an edit that breaks parsing is refused with the error
     location (SWE-agent's lint-guarded edit: 18.0 vs 15.0 resolved).
   Exact unique-match replace stays the default format: whitespace-fuzzy
   fallbacks were measured never to fire, and anchor formats (hashline)
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
6. **Pre-dispatch call repair and cheap re-reads** (G2, G4).
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
7. **Search and read shape** (G2, G3). `search` grouped by file with a
   files-only mode and optional context lines; a `glob` tool. Measure
   the read window: SWE-agent found a ~100-line window beat both 30
   lines and whole files, while Loom defaults to 2000. Seam: `search`,
   `read_file`, a new read-only tool.
8. **Completion gate and plan** (G1, G2).
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

### Mid term — context and model policy

9. **Model profiles** (G3, G7). A `ModelProfile` beside
   `ModelCapabilities`, carried as data: edit format, read window,
   reasoning-effort schedule, tool argument names. Includes a
   **progressive reasoning policy** — high effort for the opening turns
   and for verification, low in between (ForgeCode). The loop sets
   effort per turn; adapters only translate it. Seam: provider
   capabilities and the request compiler.
10. **Pruning, phase 2** (G3).
    - **Turn-age masking**: stub read-only results older than K turns
      even below the window trigger — SWE-agent's last-5-observations
      ablation beat full history (18.0 vs 15.0), and an 80% trigger on a
      1M-token window never fires.
    - **Cold-cache pruning**: when the gap since the last request
      exceeds the provider's cache TTL, the prefix misses anyway, so
      re-decide for free, bypassing the delta-reclaim gate (Codebuff).
    - **Reasoning clearing**: drop reasoning older than N turns in the
      same pass (Anthropic `clear_thinking`), where providers allow it.
    - **Usage-calibrated estimates**: correct the bytes÷3 estimate
      against the previous turn's billed `input_tokens` (Sema Code).
      This settles the tokenizer question without a tokenizer.
    - Stubs stay one line: models rarely use recovery hints (Fan et al.).
    Every addition keeps the checkpoint freeze: one cache miss per
    decision. Seam: `PruningContextCompiler`, census.
11. **Deterministic openers** (G2, G4). Runtime-executed steps before
    the first model turn, chosen by task shape: read files the prompt
    names, `git diff` for changed files, a baseline `verify` to capture
    pre-existing failures. Their results enter the transcript as
    ordinary tool results, so replay and pruning need no special case
    (Codebuff's programmatic steps). Seam: the loop's setup phase.
12. **Handoff compaction** (G3). The last resort when non-prunable
    history outgrows the window: instead of recursive in-place
    summarization (which Codex reports degrades accuracy), generate a
    goal-directed handoff — task, decisions, open problems, relevant
    files — and continue in a fresh projection (Amp's Handoff). One
    cache miss, frozen like a prune checkpoint. Prefer provider-native
    compaction where an adapter has it. Seam: a final strategy in the
    pruning decorator.
13. **Background commands** (G2, G5). Long builds and tests run
    detached; the runtime injects a completion notice at the next turn
    boundary instead of blocking a turn on them (Sema Code, Crush). Seam:
    the bounded exec and turn assembly.
14. **Non-OpenAI provider** (G7). An Anthropic-messages adapter to
    pressure the `ModelProvider` boundary. The compiler emits
    **segments marked cacheable or volatile**, so cache breakpoints are
    placed by the adapter and the kernel stays provider-blind (OpenDev).
    Capability discovery copies the OpenRouter catalog pattern.
15. **CLI session resume.** Listing, selection, continuation, and
    transcript export (also the input for item 19).
16. **Search parity.** Adopt the `ignore` crate's matcher for chained
    gitignore files or pin the documented negation-precedence
    divergence with real-repository fixtures.
17. **Artifacts lifecycle.** Reference counting from live transcripts,
    an age/size-bounded GC, and a size-reporting command.

### Later — structure and self-improvement

18. **Symbol tools** (G4, G5). Tree-sitter `symbols_overview` and
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
       census, harness version, and outcome;
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
  tokens/turn at ~30 tools; nothing at ~9). Revisit if third-party
  tools arrive.
- **Hashline or patch formats as the default edit.** Per-model profile
  option only, if measurement favors it.
- **A real per-provider tokenizer.** Usage calibration (item 10) gives
  the accuracy where it matters.

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
