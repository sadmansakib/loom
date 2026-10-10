# Changelog

All notable changes to Loom. Format loosely follows Keep a Changelog; this
project versions by iteration, not semver.

## 2026-10-10 — Leaked-descendant pipe hold no longer hangs collection

A command that leaks a background process (a test suite spawning `sleep`,
then exiting itself) left the harness waiting forever: the descendant
inherited the output-pipe write-ends, so EOF never arrived after the direct
child exited. `run_bounded` — the one execution path behind `run_command`,
`verify`, `fusion`, and the fast paths — now waits on the direct child
first, gives the stream readers a 2s grace window, reaps the whole process
group if the pipes are still held (the group belongs to the command), and
after one final bounded wait returns any still-held stream truncated with
an explicit marker. Never a hang; the exit status always reaches the
model; a reaped group yields the stream whole. Two tests cover both paths
(group-reaped holder, `setsid`-escaped holder); tools suite 202.

## 2026-10-10 — Dependency-update fast paths, ecosystem routing, dynamic models

Three changes land together because they close the same audit: the
fast-path seam's second slice, format routing by ecosystem, and the
end of hardcoded model knowledge in the frontends.

**Dependency updates (fast-path slice 2).** `update/bump <crate> to
<version>` now solves with zero model invocations on Cargo projects:
the analyzer parses the intent, `edit_manifest` rewrites only
`[dependencies]`/`[dev-dependencies]`/`[build-dependencies]` entries
of the exact simple shape `name = "x.y.z"` (inline tables, dotted
keys, `.workspace` entries decline; indent and final newline
preserved), and the result must verify with `cargo metadata
--no-deps --offline` before it sticks — otherwise the snapshot
restores and the agent loop takes over, byte-identical. Ecosystem
routing: format intents name their toolchain (`cargo fmt` vs `gofmt`
phrasing), routed via `detect_ecosystem`; Go verifies with `gofmt -l`
+ `go build ./...`. npm/Python/Make formats decline — no
deterministic offline verifier for the edit class — and non-Cargo
dependency bumps decline the same way, by design. A new lab fixture
(FPATH4, ungated until the running campaign closes) measures the
dependency lever; engine and tools tests grew to cover the grammar,
manifest rewrite, and zero-request e2e solve.

**No hardcoded models.** Model capabilities and pricing are now
data, resolved at open: `Engine::open_async_with` fetches the
OpenRouter catalog for *every* provider (with the OpenRouter
credential; OpenRouter failures propagate, others fall back to
built-in tables — catalog wins when both know the model). Replay
tests derive capabilities from the recording itself (window floored
at 8× the largest serialized request; tool/parallel/reasoning flags
from the transcript) instead of any provider table. Pricing returns
`Option<f64>` — provider-reported when present, price table or free
tier when known, `unpriced` (never a panic) otherwise — and report
JSONs now carry a `cost_source` field. Replay got honest about mixed
dirs: recordings parse as `<task>-<model>-<run>` (longest task-id
prefix, so `EXT-` ids replay again), 429-era empty shells are
skipped, and failed-run recordings replay to their same failure —
fidelity is the verbatim transcript and turn counts, not success.

**Required flags.** `loom run` no longer defaults `--provider
deepseek --model deepseek-flash`: both are required, as are the
provider arguments to `auth login|switch|logout`. Loom picks no
provider for you; examples now say `--provider nvidia --model
z-ai/glm-5.3-flash`.

## 2026-10-10 — Global AGENTS.md support

The harness already read project-level `AGENTS.md`/`CLAUDE.md` through
the facts scan; the user's standing instructions had no home. `loom run`
now resolves a config directory (`$LOOM_CONFIG_DIR` → `$XDG_CONFIG_HOME/
loom` → `~/.config/loom`) and, when it holds an `AGENTS.md`, composes it
into the system prompt once at startup — appended as a final
`## Global instructions (<resolved path>)` section after the built-in
instruction or an explicit `--system-prompt`. The file is bounded at
10 KiB (char-boundary-aware truncation with a byte-count marker),
a missing file is a clean no-op, and `--no-global-instructions` opts
out for one run. Loading lives entirely in the CLI frontend: the engine
never reads the environment (ADR-008), so embedders and the eval
harness are unaffected by any machine's global file; project-level
`AGENTS.md` remains a facts-scan concern (§5.2) and needs no CLI
involvement. 13 CLI tests (+8 instructions module: missing/unreadable/
verbatim/capped/multibyte boundary/merge identity/merge section;
+2 args parsing: flag default and opt-out; +3 load-global path cases).

## 2026-10-10 — Linux gaps: sandbox preflight, error mapping, headless keyring

Two Linux failures that read as Loom bugs but are host policy now fail
at setup with actionable fixes. First, the sandbox preflight:
`loom run` executes the same bwrap smoke command CI uses (pid/net
namespace + ro-bind /usr + tmpfs /tmp) before the task starts —
`loom_core::tools::sandbox::preflight()`, a no-op on macOS — so an
Ubuntu 24.04+ host denying unprivileged user namespaces
(`kernel.apparmor_restrict_unprivileged_userns=1`) fails once at setup
instead of failing every `run_command`/`verify` mid-task with a bare
`bwrap: setting up uid map: Permission denied`. Escape hatch:
`LOOM_SKIP_SANDBOX_PREFLIGHT=1`. Any failed command whose stderr
carries a `bwrap:` setup signature also gets a remediation block
appended (AppArmor per-binary `userns` profile → `bubblewrap-suid` →
host-wide sysctl, plus container seccomp flags and the
`.loom/commands.toml` `sandbox = false` opt-out); ordinary command
failures are never annotated. `docs/linux-sandbox.md` carries the
distro matrix, the AppArmor profile stanza for the loom binary, and
both escape hatches. Second, headless credential storage: loom-engine
now links `keyring-core` with explicit stores instead of the all-in
`keyring` crate — macOS uses the Keychain directly; Linux starts on
the Secret Service (persistent, needs session D-Bus) and falls back
once, on a platform/storage failure, to the kernel keyutils store
(always present, headless-safe, in-memory), after which resolution
falls through to the environment names as designed (§18.3) and
`loom auth login` still works. Five new tests (preflight no-op probe,
AppArmor/seccomp/generic bwrap remediation classification, ordinary
failures left unannotated); workspace suite green at 745 passing.

## 2026-10-10 — Assistant-text elision and LLM handoff (roadmap item 12 complete)

Aged assistant text was the final unpruned bucket, and the last-resort
LLM handoff existed only as a roadmap line. Both shipped, in evidence
order. First, deterministic elision (CliffCompaction's truncation beat
summarization, arXiv 2609.26779): outside the protected tail, assistant
text older than `elide_assistant_text` turns (default 8) exceeding
`elide_assistant_chars` (default 400) keeps its first 200 chars plus a
` [pruned N chars]` marker. The signature is a constant — config gates
selection only — and idempotent, so rescue-baked and frozen re-applied
views cannot double-elide; elisions freeze into the same
`PruneCheckpoint` behind the same 20K-reclaim gate (additive serde
defaults, old checkpoints replay unchanged). Second, the LLM handoff
itself: when the pruned view still overruns the budget, the loop builds
a goal-directed digest (task, session brief, recent assistant messages,
recent failed tool results, touched files) and makes exactly one
provider call — no tools, 1024-token cap, plain-text end-turn responses
only (anything else fails closed) — then freezes a checkpoint whose
projection is a *view*: session brief, the handoff summary as a fresh
user item, and the transcript tail. Ordinal-keyed elisions skip the
view (it renumbers ordinals); call-id elisions still apply. One handoff
per task — a second overrun is a hard error — and the checkpoint
re-freezes byte-stably. Observable as `ContextHandoff { turn, chars }`
(event schema v6, additive). `AgentConfig::handoff_compaction` (default
on) gates it; the engine's bare arm disables it with loop shaping, so
the A/B arms stay honest. 758 tests (+9 pruning: elide_text shape and
idempotence and multibyte boundary, window-gated selection, protected
tail, frozen re-apply, handoff view construction, call-id-only frozen
elisions in view, serde defaults; +3 agent-loop: budget-overrun falls
through to handoff with billed usage and event, disabled propagates
`BudgetExceeded`, guard rejects tool-call responses).

## 2026-10-10 — Fast paths: zero-invocation solves (roadmap item 3)

The cheapest model call is the one the harness never makes. The engine
now runs a deterministic analyzer (`loom-tools/src/fastpath.rs`)
*before* the agent loop on every task: rename
(`rename X to Y`), constant change (`change/set CONST from/to LIT`),
and format (`run rustfmt…`, 15 phrase variants) intents, parsed by
anchored grammars with exact-case identifiers and keyword-reject lists —
never a fuzzy guess. A hit is only a proposal: whole-workspace guard
checks (1–500 whole-word occurrences for renames, zero collisions with
the target identifier, exactly one `const` site, from-value match,
CRLF declined), a byte snapshot of every file about to change, apply,
then verification through the same policy-gated sandbox as `run`
(`cargo fmt --check` + `cargo check`). Any failure restores the
snapshot and the instruction falls through to the agent loop — the
never-worse contract: on every decline/revert path the workspace is
byte-identical to before. A hit is booked by the kernel as a
zero-request solve: `Agent::record_fast_path` persists the user item,
emits `FastPathHit { kind, files_changed }` (event schema v7,
additive), books an assistant item carrying the note, and completes
the task with all-zero counters (turns, model requests, tokens, tool
calls). `EngineOverrides::fast_paths` (default on) gates the seam and
the bare arm disables it with loop shaping. The eval suite grew the
FPATH slice — FPATH1 (constant, tier s), FPATH2 (cross-file rename,
tier s), FPATH3 (exact-file format gate, tier s) — gated like any
other task, grammars general rather than task-shaped, so fast-path
coverage is measured, not assumed. 802 tests (+8 analyzer: grammar
accept/reject, word-bounded replace, literal round-trip, const-site
detection; +1 kernel zero-request bookkeeping; +4 engine: rename
solved with zero model requests, unmatched falls through, override
disables; +3 offline eval task proofs; +189→189 tools suite steady).

## 2026-10-10 — The eval suite learns to measure the harness (roadmap item 1)

The suite graded outcomes well and harness value not at all: every
field task was a single-fault miniature (≤251 LOC, ≤6 files, largest
file 129 lines), nothing crossed the pruning floor, and no baseline
existed to credit the levers against. Five recommendations shipped:

- **Scale-tier corpus** — NAV1 (many-reads: 45-module units crate,
  fault in one constant among hundreds), BIG1 (large-file: 614-line
  lib.rs, 52 utilities), MULTI1 (cross-file: five of twelve widget
  schema constants drifted; the fix necessarily touches five files),
  LONG1 (heavy, long-horizon: ten-chapter interpreter spec + stubbed
  evaluator — the only in-house task whose final request crosses the
  64K-pruning floor, so pruning/checkpoints finally fire in eval).
- **Bare-loop arm** — `LOOM_EVAL_BARE=1` runs the same suite on the
  basic-harness baseline (no facts, no pruning, no completion-gate or
  failure-ledger feedback, primitive five-tool registry; plain
  append-only compiler). Engine seam: `EngineOverrides::bare_loop()`
  + `Agent::with_loop_shaping(false)`; artifacts suffixed `-bare` so
  both arms coexist. The Scaffold-Effect numbers (harness moves
  tokens-per-solved up to 40× at flat pass rates) are only measurable
  against exactly this baseline.
- **Efficiency headline** — the suite economics block gains the
  SWE-Effi / Scaffold-Effect metrics: `tokens_per_solved` (billed
  input+output amortized per solved gated run), the
  `expensive_failure_ratio` (share of spend on unresolved runs —
  failures burn multiples of successes in field data), the
  `failure_categories` vector (`end_reason` counts + `gate_failed`),
  and `idle_turns` / `idle_turn_rate` (the no-tool-call assistant
  turn tax), plus per-task `no_tool_turns` on every run report.
- **Difficulty tiers** — every field task carries an `s`/`m`/`l`/`xl`
  human-time tier; the headline reports pass rate per tier, because a
  blended rate hides the tier cliff (84.7 % → 33.1 % easy → hard in
  field data). B-tasks backfilled.
- **Editor bench** — `editbench`, a deterministic token-free
  `edit_file` micro-bench (PR-Edit's finding: editor competence
  correlates r=0.98 with downstream resolve): unique-anchor apply
  across 40/240/1200/3000-line buckets, ambiguity refusal, batch
  atomicity, cross-file batches, whitespace repair, unicode
  byte-exactness, oversize cap. Report:
  `target/eval/report-editbench.json`; runs on every commit.
- **Polyglot editor bench** — the same bench grew nine
  language-shaped families: Python indentation blocks, JSON escaped
  quotes (parse-verified), YAML/TOML indent-and-table edits, JS
  template literals, shell quoting, a two-part CRLF case (a `\r\n`
  anchor applies; a bare-`\n` anchor must NOT match a CRLF file — no
  cross-ending fuzzy match), extension-less Makefiles, and one batch
  touching `.py/.js/.sh/.json/.yaml/.toml` atomically. Shipped with
  **POLY1**, an agent-level mixed-language outcome task (eight fixes
  across shell/Python/JS/YAML/TOML/JSON/Make/CRLF text; gate is
  contains-needles, `tests_green: null` — no cargo test, non-Rust
  workspaces fully supported), so polyglot editing is measured at
  both the editor and the agent level. The breadth sweep extends
  both layers to every mainstream family: twelve more editor-bench
  families (Go tabs, C `snprintf`, Java `equals`, C# coalescing,
  Ruby heredocs, PHP interpolation, TypeScript annotations, Kotlin
  elvis, Swift `guard let`, Postgres `$$` bodies, MySQL backticks,
  zsh/fish/PowerShell dialects) and three gated tasks — SHELL1
  (bash/zsh/fish/PowerShell, one real defect per dialect), DB1
  (SQLite/Postgres/MySQL/Mongo/Redis artifacts; no engines run —
  gates check the files), LANG1 (Go/Java/C/C#/Ruby/PHP/TypeScript/
  Kotlin/Swift, one idiomatic defect each).
- **Hard-gate preset** — `LOOM_EVAL_TASKS=hard-gate` expands to every
  gated task (heavies included, labs never). The preset is derived,
  not enumerated: a new gated task joins the release gate by
  existing, so nothing can be silently excluded (anti-benchmaxxing).
  Split campaigns merge without re-spend: `eval_suite_union`
  (`--ignored`, no tokens) rebuilds a suite headline from the
  per-task reports already on disk (`-union.json`), and in hard-gate
  scope a missing report is a loud failure, not a silent skip.
- **External anchor** — `cargo run -p loom-eval --bin multiswe-import`
  bridges Multi-SWE-bench JSONL into `EXT-*` heavy tasks (clone at
  `base_commit`, apply the test patch, gate on the instance's own
  F2P/P2P, protect the test files), so an external comparability
  slice is one command away; the offline proofs validate imported
  seeds like any other task. Five Rust instances imported and
  seed-validated (bytes ×2, rayon, clap, tracing); darwin-invalid
  candidates (ripgrep, fd — upstream `#[cfg]`-guards macOS out of
  their own p2p tests) dropped with reasons, never edited. Published
  anchors for comparison live in `docs/benchmarks/published-anchors.md`.

## 2026-10-10 — Provider-gated reasoning elision (roadmap item 12)

Assistant reasoning traces were the last bucket stacking verbatim
every turn — replaying them byte-exact is a provider contract once
tools are involved (ADR-006), so the pruning compiler never touched
them. Elision is now possible and provider-gated: with
`PruneConfig::elide_reasoning` set (default `None`), reasoning traces
outside the protected tail and older than the window drop from the
projection (`reasoning = None` — a reasoning slot is either the
provider's verbatim trace or absent, never paraphrased), freezing into
the same `PruneCheckpoint` behind the same 20K-reclaim gate. The
engine's provider gate turns it on only where omission is provably
safe: GLM (dropping old traces forfeits only the Preserved-Thinking
cache benefit for those turns) and Anthropic (its wire never forwards
reasoning on replay, so the elision changes nothing on the wire).
DeepSeek and the other OpenAI-vocabulary backends stay off — a
tool-bearing request whose prior assistant turns lack
`reasoning_content` fails with HTTP 400 (live-verified). Ordinals skip
`ContentFilter`-stopped turns so frozen keys always match what the
projection contains. 706 tests (+5 pruning: elide-when-enabled,
off-by-default, filtered-turn ordinal alignment, frozen re-apply,
attribution-once; +1 engine provider gate).

## 2026-10-10 — Deterministic history elision (roadmap item 12)

Tool results were the only bucket the pruning compiler ever shrank;
tool-call arguments and superseded user items stacked verbatim forever
— together the 28%-of-tokens bucket CliffCompaction (arXiv 2609.26779)
measures on terminal workloads. The pruning decorator now elides them
deterministically, zero LLM calls: outside the protected tail, arguments
older than `elide_turns` (default 8) collapse to signatures — strings
over `elide_arg_chars` (default 160 chars) become `[pruned N chars]`
while short identifying fields like paths survive for re-run recovery —
and a user item superseded by a later one ages out into a one-line stub.
The session brief (ordinal 0) and the *last* user item — the live
task's instructions under follow-up tasks — are exempt by construction,
so continuation sessions never lose their brief. Elisions freeze into
the existing `PruneCheckpoint` (verbatim argument bodies, so re-apply is
byte-stable regardless of config drift) behind the same 20K-reclaim
gate: one prefix-cache miss per freeze, zero while parked. Assistant
text and reasoning remain untouched; the LLM handoff stays the last
resort for non-prunable overflow. 700 tests (+6 pruning: arg elision,
brief exemptions, long continuation, frozen re-apply, opt-out,
attribution-once).

## 2026-10-10 — Artifacts lifecycle (roadmap item 17)

Artifact bodies were immortal: written before the turn that references
them commits (ADR-012), they could only leak — an orphaned body was
harmless but permanent, and the item-3 filters tee more of them than
ever. `ArtifactStore` now carries `list` and `delete` (metadata-only
listing, oldest first; first-write times on both backends, so a
re-stored id keeps its original age). `artifact_gc` walks every stored
transcript for the live set, then deletes only orphans past the
backstop bounds — 30 days of age, then oldest-first until the store
fits 256 MiB; a referenced body is never deleted, whatever its age or
the store's size. The engine runs the pass at open, best-effort (a
failed scan skips the run entirely — an empty live set would treat
everything as an orphan). `loom stats` reports the store's lifecycle:
bodies, bytes, live/orphan split, and reclaimable bytes, always
store-wide. 694 tests (+8: kernel GC units, SQLite list/delete).

## 2026-10-10 — Command-aware output filters (roadmap item 3)

`run_command` now compacts well-shaped successful output per command:
git log (whole commit blocks), git diff, git status, ls/tree, grep,
and cargo test's passing-test runs each reduce at their own unit
boundaries with counted omission markers — never a byte cut mid-line.
Nothing is lost: reductions apply only when an artifact store is
attached, and the raw stream is teed there on both paths (the reduced
success stream and the failure-shaped evidence it replaced), each
with a refetch note mirroring the engine's spill note. Without a
store, output is raw, pre-filter behavior. New `reduce` module (10
unit tests) plus 4 store/tee round-trip tests — 686 tests total, all
green. The spec budget rose 6,150 → 6,250 B: run_command's
description now tells the model well-shaped output may arrive reduced
with an artifact refetch.

## 2026-10-09 — Complexity pass, eval batch: grade_gate's arms split

Tier-1 close-out (lizard CCN ≥ 15): `eval::grade_gate` 17 → 6. The
fixture-test run moved to `gate_tests` (with the spawn_blocking join
and its two error reports); the two gate kinds each got their grader —
`grade_bugfix` (fail_to_pass / pass_to_pass / protected-file ledger)
and `grade_outcome` (exact-files / contains / tests-green checks).
`grade_gate` itself is now the dispatch: which gate, whether tests are
needed, which grader. 672 tests unchanged. With this, every function
in the Tier-1 audit is under the threshold — 19 functions across five
commits, behavior-preserving throughout.

## 2026-10-09 — Complexity pass, cli batch: run's consumer wiring decomposes

The CLI adapter's `run` (lizard CCN 22 → 7) is now the process skeleton
alone: `spawn_consumers` builds the §11 fan-out (renderer thread always,
`--trace` writer thread when requested), `ctrl_c_ladder` is the two-stage
stop signal (pause between turns, then abort), `join_consumers` closes the
channels and collects the threads (handing back the trace writer), and
`finish_trace` maps the task outcome to the manifest's terminal shape and
finishes the best-effort export. 672 tests unchanged.

## 2026-10-09 — Complexity pass, tools batch: edit, confinement, and evidence hotspots decompose

Tier-1 (CCN ≥ 15) reductions across loom-tools, same treatment as the
kernel and providers batches: `edit_file::apply_edits` 26→13 and
`edit_file::call` 17→12 (load/resolve/replace-span/repair-probe/assemble
helpers — the fused edit+verify flow keeps its shape);
`policy::validate` 17→13 (`bare_name` predicate); `confinement::walk`
17→10 (`step_normal` carries the symlink machinery — lstat, existing
target canonicalisation, dangling-target recursion with the
`SYMLINK_LIMIT` bound; all 19 confinement security tests green);
`evidence::go_failures` 16→13 (`go_is_status`),
`evidence::pytest_failures` 17→14 (`pytest_summary_failures` fallback +
`pytest_is_body` predicate); `verify::render` 15→5 (`parse_evidence`
per-ecosystem dispatch + `push_raw_fallback`). Every extraction is
behavior-preserving; 672 tests unchanged.

## 2026-10-09 — Complexity pass, providers batch: the wire hotspots decompose

Tier 1 of the complexity audit (lizard CCN ≥ 15), provider batch. The
SSE driver's `step` split into named helpers — `send_request` (header
timeout vs builder-vs-transport mapping), `non_success_error` (status
classification with `Retry-After`), `sse_body` (the non-SSE 200
refusal), `eof_is_done` (the narrow unterminated-`[DONE]` tolerance),
and `ingest_chunk` (decode + parse + accumulate one body chunk) —
24 → 14. The OpenAI accumulator's `ingest`/`finalize` split into
`merge_tool_fragment`, `terminal_error` (the guard ladder), and
`normalized_stop` (the §5.1 table): 18 → 11 and 18 → 8. The Anthropic
assembler's `ingest` moved its fat arms into
`ingest_message_start`/`ingest_block_delta`/`ingest_message_delta`:
19 → 11. The Messages API driver now shares the wire driver's send
helpers (auth travels as a header so both protocols' wire bytes stay
their own): `step` 18 → 15. Wire parsers stay match-heavy by contract;
the goal was ≤ 15, not single digits.

## 2026-10-09 — Complexity pass, kernel batch: the hotspots decompose

Tier 1 of the cyclomatic-complexity audit (lizard, CCN ≥ 15) starts with
the kernel's five hottest functions, each decomposed into named,
documented helpers with zero behavior change (672 tests green):

- `pruning::decide` 38 → 1: the transcript scans live in
  `TranscriptFacts::collect`/`protected_from`, and each pruning pass
  (`dedupe_reads`, `supersede_reads`, `stub_stale_errors`,
  `age_out_large_reads`, `mask_aged_reads`) is a small function over a
  shared `DecisionPass`; `decide` is now the policy order, nothing else.
- `agent::run_turns` 33 → 25: budget derivation, the facts scan, turn
  compilation, and the completion gate's refusal half
  (`refuse_completion_once`) each moved out; what remains is the
  stop-reason dispatch itself.
- `agent::run_task` 20 → 6: the epilogue is `finalize_task`.
- `facts::render` 16 → 7: per-section renderers
  (`render_listed`, `render_instructions`, `render_git`).
- `agent::tools::execute_call` 15 → 10: the tool cache's read and write
  halves are `cached_result`/`cache_result`.

Also fixed a latent build bug: `loom-kernel` uses
`tokio::task::spawn_blocking` in library code but only declared the
`sync`/`time`/`macros` features — workspace-wide feature unification
masked it, so a lone `cargo build -p loom-kernel` failed. `rt` is now
declared.

## 2026-10-09 — Facade boundary guarded: loom-cli depends on loom-core alone

The ADR-021 rule is now enforced by the suite, not by review vigilance:
`crates/loom-cli/tests/facade.rs` fails `cargo test --workspace` the
moment `loom-cli` declares a direct dependency on any workspace crate
other than `loom-core` (dependencies, dev-dependencies, and
target-specific sections all covered), and also asserts the facade
dependency is present so the guard cannot pass vacuously. The
architecture doc's §9 closes with the boundary's inventory: the CLI
keeps presentation only (args, renderer, tracing/exit codes, auth
prompts, Ctrl-C mapping), and the cross-process story stays in-process
by design — with the versioned event stream, serializable
`StatusSnapshot`, and byte-stable trace formats documented as the seam
a future server would cut along.

## 2026-10-09 — loom-eval: the eval harness becomes a crate, driven by the engine (C1)

The eval harness moved out of `loom-cli`'s test directory into an eighth
workspace crate, `loom-eval`, and stopped hand-wiring an `Agent`: every
arm now opens the real `Engine`, so harness wiring cannot drift from
production composition. The enabler is a new overrides seam on the
engine — `Engine::open_with`/`open_async_with` take an
`EngineOverrides` (a prebuilt provider for offline replay, a provider
hook for recording, an optional registry and compiler; every `None`
keeps the production choice). The live arm resolves credentials through
the engine exactly like the CLI (keychain or env), warms the OpenRouter
catalog cache, and records through a hook that wraps whatever the
engine built; the replay arm injects the recorded script as the
provider. NVIDIA's free tier prices at $0 in the cost table, and the
pinned compiler now derives its estimator from the provider
(`estimator_for`) instead of hardcoding o200k. `loom-cli` lost its
last direct dev-dependencies beyond tempfile. One behavioral note:
runs now include the engine's `AwaitingInput` marker, so per-run event
tallies count one more event than recordings made before this change.

## 2026-10-09 — StatusRecorder: pollable progress snapshots (B5)

Frontends polling "where is this task right now?" no longer need to
reimplement event folding. `kernel::event::StatusRecorder` is a sink
that folds the stream a task already emits into a `StatusSnapshot`:
current turn, in-flight tools (correlated by call id, so concurrent
same-name calls stay distinct), the running model-request attempt,
task phase (idle/running/completed/failed/cancelled/paused), and the
idle session after `AwaitingInput` — the id a resume loop sends the
next input to. Clones share one snapshot: fan a clone out next to the
rendering sink (`FanoutSink::new().with(recorder.clone())`), poll the
retained handle. Pure in-memory tally — cannot drop, transparent to
`StrictSink`; one recorder tracks one task. Snapshots serialize, so a
GUI can shuttle them across a process boundary. No event-schema
change: the stream already carried everything.

## 2026-10-09 — TaskControl: two-granularity stopping (cancel vs pause)

`run_task` and `continue_session` now take a `TaskControl` instead of a
bare cancellation token. `cancel` keeps the old semantics — abort
mid-flight, drop the stream, fill unstarted tool calls — and the
session survives it: every committed item was persisted, so aborting a
call never loses the session. `pause` is the new graceful stop:
observed only at turn boundaries, it lets the in-flight turn (model
reply and tool batch) commit in full, then ends the task as
`TaskStatus::Paused` with `end_reason: "paused"` and a `TaskPaused`
event (event schema v5) — a pause is not a cancellation, and both the
status and the event stream say so. The CLI maps one Ctrl-C to pause
("stopping after the current turn") and a second to abort; either
exits 130 with the session resumable. `TaskControl::from(token)`
wraps an existing hard-stop token, which is how the eval harness's
strict sink keeps tripping the task on event loss.

## 2026-10-09 — Session continuation + the AwaitingInput event (B3)

First-class resume for the driving API: `Engine::continue_session(id,
input, …)` loads a persisted session and runs the next task on it,
returning the advanced `Session` and the task outcome together — a
failed or cancelled task still advances the session (input committed,
partial turns persisted), so only load errors are `Err`, with a new
`EngineError::SessionNotFound` for ids that do not resolve. This is
the core seam roadmap item 15's CLI resume builds on.

Every task end now emits `AwaitingInput` (event schema v4) naming the
idle session, right after the terminal event — the signal a
stream-only frontend (GUI/TUI resume loop, roadmap item 22) keys on
instead of polling. Emitted by the engine's driving layer only;
kernel-level drivers and the eval harness see no new events, and the
one-shot CLI renders nothing for it.

## 2026-10-09 — FanoutSink: multi-consumer event fan-out

`loom_core::kernel::event::FanoutSink` composes several event sinks into
one: every event fans out to each child, each consumer numbers its own
stream from zero, and a dropping child (closed channel) costs nothing but
its own events — siblings keep receiving and the task keeps running. The
fan-out's drop count sums the children, so the task-end `events_dropped`
report still catches a loss on any consumer. Chainable:
`FanoutSink::new().with(renderer).with(tracer)`; `with_shared` takes an
`Arc<dyn EventSink>` for callers that keep a handle to one child.

The CLI now uses it: `loom run` splits its single consumer thread into
two — the stdout renderer always, the NDJSON trace writer when
`--trace` is on — so a stalled disk can delay the trace without ever
delaying the terminal, and vice versa. This is the kernel-side shape
GUI/TUI frontends fan out on (punch-list item B1).

## 2026-10-09 — Catalog policy behind the engine; `Engine::open_async`; auth env-state helper

The OpenRouter catalog policy — fetch with the credential the provider
itself would use, cache beside the state database with a 24-hour TTL,
fall back to a stale cache when the network is unreachable — moved from
the CLI into `engine::catalog` (A5). `Engine::open_async` (B2) drives it
before the synchronous open when a config needs a catalog and does not
already carry one, so every frontend gets the sequencing identically
instead of re-implementing it around `Engine::open`; the CLI's `run`
dropped its runtime-dances and calls `open_async` like anyone else.

`auth::provider_env_state` (B6) computes which environment fallbacks
are live, next to `provider_env_names` where the names are declared;
`auth list` renders it instead of scanning the environment itself.
Tests: 653 (+2 — `open_async` credential ordering, env-state marking);
the four catalog-policy tests moved crates with the module.

## 2026-10-09 — Trace format + attribution move behind the engine

The trace export (`events.ndjson` / `manifest.json` / `transcript.json`)
and the `--trace` aggregation now live in `loom-engine` as
`engine::trace` and `engine::trace_stats`. The format is a data
contract: any frontend — CLI, TUI, GUI — that drives a task writes
byte-identical traces through `TraceWriter`, so any consumer can read
them back. The aggregation is pure (typed rows in, totals, JSON blocks,
and human-readable lines out; zero terminal I/O) and frontends decide
what to print — the CLI's stats command keeps only the rendering. The
trace file names are exported as the contract constants. Test counts
unchanged (651): the eight trace tests moved crates with the modules.

## 2026-10-09 — Facade completion: stats + data-dir policy behind loom-core

ADR-021 says a frontend depends on `loom-core` and nothing else; the CLI
still held two direct dependencies and two pieces of domain policy:

- `loom stats` opened the SQLite store and walked it itself. The data
  step — listing, id-substring filtering, and folding every session's
  transcript analytics into totals — is now
  `kernel::analytics::sessions_overview(store, filter)`, returning a
  typed `SessionsOverview`; the CLI renders it. The fold now carries
  `repairs` too (the CLI's old fold silently dropped them, so store-wide
  totals always reported zero).
- `loom-core` re-exports `loom-store`, so the CLI (and any future
  frontend) opens the durable store through the facade.
- The `LOOM_DATA_DIR` → `$XDG_DATA_HOME` → `~/.local/share/loom`
  resolution moved from the CLI to `loom_core::paths` as the suggested
  default every frontend may adopt — the engine still never reads the
  environment for the database location (ADR-008).

`loom-cli` now depends on `loom-core` alone.

## 2026-10-09 — NVIDIA live verification; DeepSeek V4 NIM rows end-of-life

First live run against NIM with a real free-tier key: a two-turn
session on `z-ai/glm-5.3` (read the repo README, answer its first
heading) completed with a tool call, streamed reasoning, and honest
accounting — 6,306 in / 2,624 cached / 17 out, wall 36.5 s. The same
run exercised the trace pipeline end to end: `--trace` wrote the task
directory, `loom stats --trace` read it back.

Live probing also showed the catalog moved: all three
`deepseek-ai/deepseek-v4-*` deployments return HTTP 410 (end-of-life
2026-08-07 / 2026-09-14) and are removed from the capability table —
unknown-model is the loud, correct answer for dead ids. The GLM-5.3
rows stay (one live-verified, one catalog-pinned); the non-forced
`reasoning_effort` pass-through keeps its wire and unit coverage with
the EOL'd ids as inert stand-ins for any future non-forced deployment.

## 2026-10-09 — loom stats --trace: attribution over exported traces (item 20.1, phase 3)

`loom stats --trace DIR` aggregates what `loom run --trace DIR` wrote:
each task directory's `manifest.json` contributes the provider's own
billed counts (input, output, cached, thinking, cache-write tokens,
turns, wall time), and each `events.ndjson` contributes its
`context_pruned` reports — per-trigger (window / age_mask / rescue) and
per-reason call attribution with estimated reclaimed tokens. Reports in
both human and `--json` form; corrupt manifests are counted and
skipped, not fatal. Reclaimed tokens are flagged as estimator-side
estimates; usage fields are the provider's counts. This closes item
20.1's attribution loop without server-side event persistence.

## 2026-10-09 — NVIDIA adapter: NIM hosted inference, free-tier eval path

`--provider nvidia` runs the five verified NIM deployments —
`z-ai/glm-5.3`, `z-ai/glm-5.3-flash`, `deepseek-ai/deepseek-v4-pro`,
`deepseek-ai/deepseek-v4-pro-0813`, `deepseek-ai/deepseek-v4-flash` —
over the shared OpenAI-compatible wire at `integrate.api.nvidia.com/v1`
(Bearer `NVIDIA_API_KEY` or `NIM_API_KEY`). Model ids keep their
`publisher/model` form. Reasoning depth is the top-level
`reasoning_effort` field: GLM-5.3 deployments force thinking on and
clamp an effort of "none" to `low`; DeepSeek V4 deployments pass `none`
through as real non-think mode. No `thinking` object is sent (that is
Z.ai first-party dialect) and no usage-dialect code is needed (NIM
uses the OpenAI-standard details shapes). NIM's chat template defaults
`clear_thinking` to false — preserved thinking — which the wire layer
already satisfies by replaying `reasoning_content` verbatim.

The free-trial tier needs no credit card, so this adapter is also the
standing zero-cost candidate for the funded-eval-model prerequisite
(roadmap item 1): GLM-5.3 and DeepSeek V4 Pro at 1M context, within
per-model rate limits. Verified by wiremock (8 integration tests +
6 unit tests; workspace now 643 passing, 13 live-API ignored), not
yet by a live run.

## 2026-10-09 — Prune attribution on the event stream (item 20.1, phase 2)

Event schema v3 adds `ContextPruned`: when a pruning pass freezes a new
checkpoint, the agent loop emits one event carrying the trigger arm —
`window`, `age_mask`, or `rescue` — plus per-call mechanism
(`duplicate_read` / `superseded_read` / `stale_error` / `aged_out` /
`turn_age`) and estimated reclaimed tokens. Turns that merely re-apply a
frozen checkpoint emit nothing, so summing `reclaimed_tokens` across
`ContextPruned` events (deduplicating by `call_id`) attributes savings
per mechanism from any `--trace` export. `CompiledContext` gains the
matching `prune_report`; the human renderer prints a one-line notice.
`loom stats` integration still needs events persisted server-side or
read from trace directories.

## 2026-10-09 — GLM adapter on Z.ai's metered API; usage dialects move to their adapters

`--provider glm` targets `api.z.ai/api/paas/v4` (never the Coding Plan
endpoint). The 5.3 family (glm-5.3, -flash, -flashx) forces thinking
on — `disabled` fails the request — so a configured effort of "none"
clamps to `low` instead of disabling; glm-5.2 accepts both. Requests
carry `clear_thinking: false` (Preserved Thinking): the wire layer
already replays assistant `reasoning_content` verbatim, which is that
mode's client obligation. Capability table pins the four 1M-context
ids; unknown ids fail loud. Engine/auth/estimator wired (ZAI_API_KEY
or Z_AI_API_KEY); verified by 8 wiremock tests, not live — no funded
key.

The shared OpenAI wire now carries only OpenAI-standard usage shapes:
`prompt_tokens_details.cached_tokens` and
`completion_tokens_details.reasoning_tokens` are generic, while
DeepSeek's `prompt_cache_hit_tokens` and OpenRouter's `cost` moved to
`dialect_usage` overrides in their own adapter files, threaded through
the finalize policy like every other provider opinion. GLM, whose
usage block sticks to the standard shapes, adds no dialect code.

## 2026-10-09 — Anthropic 5.5 wire behaviors: in-turn system messages, thinking/cache-write usage

No funded key, so the 5.5 behaviors are pinned by wiremock tests, not
live runs. Two changes make the adapter ready for a reasoning-effort
schedule (roadmap item 9):

* **In-turn system messages** — only leading system messages fold into
  the top-level `system` param. A system message that appears after the
  conversation starts stays in place as an in-turn `role: "system"`
  turn: the 5.5-era per-turn steering channel (an `output_config.effort`
  note) that applies to the next assistant turn without busting cache
  breakpoints. Hoisting it into the global param would have rewritten
  the cached prefix — the old fold-all behavior was a bug for this
  mechanism. Breakpoint placement still counts in-turn system messages
  as regular turns.
* **Usage attribution** — `Usage` gains `thinking_tokens` (the
  Anthropic `usage.output_tokens_details.thinking_tokens` /
  OpenAI `completion_tokens_details.reasoning_tokens` split of output
  tokens) and `cache_creation_input_tokens` (Anthropic cache writes,
  billed above base rate). The OpenAI-compatible wire maps
  `completion_tokens_details.reasoning_tokens`; the Anthropic assembler
  captures both fields from `message_start`/`message_delta`.

New tests: in-turn system round trip and breakpoint interaction,
5.5-style thinking stream with thinking/cache-write accounting, and the
OpenAI-wire reasoning-token mapping.

## 2026-10-09 — Trace export: `loom run --trace DIR` (item 20.1, phase 1)

One directory per task with three files: `events.ndjson` (the numbered
event stream exactly as the sink ordered it), `transcript.json`
(committed session items and task records), and `manifest.json`
(harness version, event schema version, run configuration, outcome with
a stable `end_reason` token, summary counters, and the final context
census). The export is best-effort: write failures are counted in the
manifest and warned on stderr, never mapped to a run outcome. Runs
without `--trace` are unchanged. Mechanism-level attribution (which
pruning decision saved which tokens) still needs prune-event variants
in `EventKind` — a schema v3 bump, kept as a follow-up. 598 tests.

## 2026-10-09 — Evidence refresh (web research) and architecture.md drift fixes

`docs/architecture.md` corrected against the code: §1 dependency table
(kernel depends on no workspace crate; engine also links tools), §2/§12
"no model-reachable shell" restated accurately (sandboxed shell is
model-reachable and confined; the unsandboxed `sh -c` is the
user-configured credential command only), §5.2 bootstrap facts, §5.3
injected per-provider estimators (o200k / Anthropic-scaled; kernel
keeps bytes÷3 default), §5.4 turn-age masking, §4 completion gate,
§6.2 `edit_file` batch/echo/trim-repair, §6.7 `unchanged since call`
stub, §9 `loom stats`, test count 594.

`(docs/roadmap.md` evidence base refreshed from the 2026 literature:
backlog preamble now cites Terminal-Bench 2.x same-model harness
spreads (13–24 points), the controlled harness-vs-model variance grid
(7.8×), and the 40× tokens-per-solved cost spread; item 1 names funded
eval-model candidates with pricing (GLM-5.3 metered, DeepSeek V4 Pro)
and sweep budget; item 2 carries the task-shape × model-tier nuance on
shell-vs-tools; item 9 records the Opus 5.5 adaptive-thinking mechanics
(effort steering, cache-bust rules, per-turn effort form); item 12
records Anthropic's native `compact_20260112` and the CliffCompaction /
AutoCompact results.

`README.md` rewritten against the current code: seven-crate table with
the kernel dependency invariant, three providers, iteration-2 status,
sandboxed shell and shipped-feature list. All 14 dangling
`research.md` references (a file that never existed in the repo) are
repointed or replaced with inline facts: the two harness warnings now
cite `docs/roadmap.md`, and the DeepSeek wire-format notes in
`loom-providers` cite their own live verification tests.

## 2026-10-09 — Roadmap: product vision merged with the evidence backlog

`docs/roadmap.md` now carries the restated goals (G1–G10, renumbered:
practical capability per model tier and workflow adaptation are new
G4/G5), core principles, and the nine-iteration sequence, with a table
mapping every backlog item to its iteration. Status corrected against the
code: crate dependency direction, sandboxed shell replacing the
cargo-only default, three providers, o200k/Anthropic estimators,
bootstrap facts, edit batches/echo/trim repair, completion gate,
turn-age masking, `loom stats`, 594 tests. Item 9's revisit trigger
(item 14) is marked fired; item 1 gains the funded eval model, N ≥ 5
re-runs of the N=1 claims, and a bare-loop × model-tier baseline. GLM is
planned via the metered Z.ai API only, never the Coding Plan.

## 2026-10-09 — Anthropic: 5.5-family models and full engine/CLI wiring

`--provider anthropic` now works end to end. The adapter shipped earlier
as a wiremock-verified component but the engine had no `anthropic` arm
and `ANTHROPIC_API_KEY` was never consulted — three changes close that
and bring the model table current:

* **Capability table** — `claude-fable-5-1`, `claude-opus-5-5`,
  `claude-sonnet-5-5`, `claude-haiku-5-5` at 1M context / 128K output /
  adaptive reasoning, alongside the retained 4.x entries. Dateless
  aliases (`claude-opus-5`) are refused on purpose: they float to new
  snapshots with different windows, and the fail-loud contract is the
  point of the table.
* **Engine + auth wiring** — `build()` gained the `anthropic` arm (no
  reasoning requirement: the adapter sends no reasoning parameter, so a
  non-thinking model is configuration, not error), and the credential
  ladder resolves `ANTHROPIC_API_KEY` like the other providers'
  variables.
* **Per-provider token estimation** — Anthropic models bill text
  differently (the 4.7+ tokenizer counts ~30 % more for the same text)
  and publish no local vocabulary, so `AnthropicEstimator` scales the
  o200k count 1.25×, rounded up. Under-estimating a 1M window turns
  into a mid-task provider rejection; over-estimating just prunes a
  little sooner.
* **`temperature` is never sent** — Claude 4.7 and later answer a
  non-default `temperature`/`top_p`/`top_k` with a 400. No Loom surface
  sets one; the wire now drops it instead of faithfully forwarding a
  field the API rejects. Preserved thinking (thinking blocks bound to
  the producing model/conversation) was also audited: Loom never
  re-emits thinking blocks, so the 5.5 contract changes are satisfied
  by construction — documented in `anthropic_wire`'s module docs.

## 2026-10-09 — Linux sandbox: complete the kill cascade, tolerate absent dirs

Two Linux-only defects in the `bubblewrap` wrapper, both invisible on a
macOS dev box because the two wrappers behave differently — and there
was no CI to run the suite anywhere else:

* `--new-session` makes bwrap `setsid()` the COMMAND, moving it out of
  the process group loom placed the wrapper in. `GroupKillOnDrop`'s
  `killpg` could therefore only ever reach bwrap itself, so a policy
  timeout or cancellation leaked the whole command tree — `cargo` and
  every `rustc` under it, which is exactly what architecture.md §2
  promises a timeout prevents. `--unshare-pid` is the missing half of
  the cascade: bwrap's PID 1 carries `PR_SET_PDEATHSIG`, so killing the
  wrapper tears the namespace down and SIGKILLs everything inside it.
  `--die-with-parent` was already passed; alone it only helps when
  bwrap's *parent* dies, never on the timeout path.
* `--bind` aborts the whole sandbox when its source is absent, and the
  cargo home was bound with it on a directory that need not exist. On a
  Linux host with no `~/.cargo` and no `CARGO_HOME` (containers, CI,
  rustup installed elsewhere) *every* `run_command` and `verify`
  invocation failed at sandbox setup. The workspace keeps the strict
  bind — a missing workspace is a misconfiguration and should fail
  loudly — while cargo home, granted writable dirs, and `TMPDIR` moved
  to `--bind-try`, which skips a missing source and still binds a
  present one read-write.

The bwrap argv is now produced by a platform-independent
`bubblewrap_argv` and unit-tested on every platform: it was previously
compiled only on Linux, so on the machine it was written nothing ever
inspected the argv that carries both flags.

First CI in the repo: `fmt` + `clippy -D warnings`, and
`cargo test --workspace` on both Ubuntu and macOS. The Ubuntu job
installs `bubblewrap` and relaxes
`kernel.apparmor_restrict_unprivileged_userns`; without that the
wrapper cannot build its namespaces on a hosted runner at all, and the
Linux path stays untested — which is how both defects shipped.

The MSRV floor was a lie: `rust-version = "1.88"` while `uuid` 1.27
(the v7 id source, pinned in `Cargo.lock`) declares `rust-version
1.89`, so a 1.88 toolchain failed at dependency *resolution* rather
than at compilation. It cannot be pinned down — the manifest requires
`^1.27` — so the floor is now 1.89 and the CI job pins that exact
toolchain. Found by attempting the build in a Docker container, which
is the only way this repo gets compiled anywhere but the author's
machine.

Both Linux fixes were then reproduced in a container (Debian 12,
bubblewrap 0.8.0, aarch64): the exact argv loom emits is accepted, a
plain `--bind` on an absent source aborts while `--bind-try` skips it,
and the full `loom-tools` suite is green — including the
dropping-the-future cascade test and the fused cargo-check paths, which
macOS alone never proves. Note that a container needs `--privileged`
(or at least `CAP_SYS_ADMIN` + `CAP_NET_ADMIN`) for bwrap to bring up
loopback in its own network namespace; without it every sandboxed
command dies with `loopback: Failed RTM_NEWADDR`. GitHub's Ubuntu
runners are full VMs and do not need it.

Riding along: `resolve_in_path` now requires the execute bit, so a
non-executable file on `PATH` no longer shadows a later working entry
and then fails at spawn as a permissions error instead of the legible
"not found on PATH". The OpenAI-compatible request body sends `"n": 1`,
matching the `choices[0]`-only read of the response.

## 2026-10-09 — Anthropic messages adapter (roadmap item 14)

`loom-providers` gained an Anthropic adapter that speaks the messages
API rather than the OpenAI wire: system text as a top-level `system`
param, `tool_use`/`tool_result` content blocks (failed results carry
`is_error`), `thinking_delta` reasoning capture, Anthropic's error
taxonomy (429 → rate-limited, 529 → overloaded, 413 → context
overflow), and stop-reason mapping incl. the same calls-beat-reason
normalization the OpenAI wire uses. It streams through its own state
machine — deliberately not the shared OpenAI driver, since refactoring
that delicate path could not be re-verified live without a funded key.

The kernel stays provider-blind: `ModelRequest` grew a
`stable_prefix_messages` hint (`#[serde(default)]`, so old eval
records still deserialize) — the count of leading messages the
compiler guarantees byte-identical across requests. The default
compiler sets it to the whole projection (append-only transcript,
frozen facts); the adapter places an Anthropic
`cache_control: ephemeral` breakpoint at the end of that region.

Capabilities resolve from a small static table
(claude-opus-4-1 200K/32K, claude-sonnet-4-5 200K/64K,
claude-haiku-4-5 200K/32K; unknown ids return None, fail loud) —
Anthropic's models endpoint exposes no capability fields, so there is
nothing to reduce catalog-style. Engine/CLI provider selection stays
deferred until the multi-provider CLI work (roadmap items 15+).

Verified offline with wiremock: request wire shape, auth headers
(`x-api-key` + `anthropic-version`), cache-breakpoint placement,
tool-call assembly from partial `input_json_delta` chunks, truncated
streams reporting `length` without calls, and error classification.

Riding along: `CompiledContext.stubbed_calls` — the pruning compiler
now reports which projected call ids it rewrote to stubs this compile.
The tool loop excludes those from the cache-stub shortcut (roadmap
item 6), so a repeat read whose original body was pruned gets its full
re-bounded content back instead of a misleading
`unchanged since call <id>` pointing at a body the model can no longer
see.

## 2026-10-09 — Pruning phase 2: turn-age masking (roadmap item 10)

Read-only results older than 8 turns (configurable via `PruneConfig::
age_mask_turns`) are stubbed even below the context-window trigger, with
the same reclaim gate (batches stay episodic — one cache miss per
decision) and the checkpoint freeze intact: the projection stays
byte-stable between batches. Age masking alone never clears history —
the whole-history pass still requires the window trigger or a budget
rescue.

Measured honestly: on the current stack the longest eval tasks no
longer build large transcripts, so the mask stayed dormant in A/B
(B9/B14 both solved, no stubs fired, no regression). It ships as
bounded-growth insurance for longer horizons, after SWE-agent's
last-5-observations ablation (18.0 vs 15.0). The other roadmap pieces
are deferred with evidence: cold-cache pruning (eval requests are
back-to-back, gap < cache TTL), reasoning clearing (the free eval model
emits no reasoning traces), and usage-calibrated estimates (bytes÷3
already tracks billed input within ±20%; median ratios nemotron 0.97,
deepseek 0.88, large deepseek requests 1.22 — conservative exactly
where triggers matter).

## 2026-10-08 — Completion gate (roadmap item 8, phase A)

When the model tries to finish while files changed since the last green
check (`exit status: 0` from a command-bearing tool), the agent refuses
the EndTurn once: it lists the changed paths, asks for the relevant
checks, and quotes the original task (Terminus-KIRA double-confirm
shape). A second finish attempt is always accepted — the gate nags, it
does not trap. Fused write+verify satisfies the gate in one step, and
checks only count when shaped like commands. Free-tier A/B: the refusal
fired exactly once on the checkless canary (B2, +2 turns) and never on
the synthesis task (B15), which now converges edit→test→green in 15
turns — attributed to items 4–6, not the gate; the gate ships as cheap
insurance (542 tests green, replay made one-canned-turn drift-tolerant).
The roadmap's `todo`-tool half is deferred to a later phase.

## 2026-10-08 — Call repair and cheap re-reads (roadmap item 6)

Two robustness halves, chosen by corpus: a whitespace-trim fallback for
`edit_file` `old_string` (exact match 0 → apply the one trimmed match;
the 84-run corpus had 43 not-found failures of exactly this shape) and
a stub for repeat read-only calls — when a tool-cache hit's producing
result is still in the projection, the repeat returns `unchanged since
call <id>` instead of re-serving bounded content. Repairs are recorded
in `effects.repairs` and counted by `loom stats`. Argument aliases and
stringified-JSON repair were skipped: zero occurrences in the corpus.
A/B on the free tier (B3/B6/B9, nemotron ×1 + B9 ×2 re-runs): all
solved, 5 stub hits across 4 runs, 0 repairs (the item-5 echo already
removed the padding trigger for this model — the repair stays as a net
for models that do pad). B9's turn spread (6/12/40) is task variance,
not regression: the 40-turn run fired no stubs and still solved.

## 2026-10-08 — Multi-edit batches and edited-region echo (roadmap item 5, phase A)

`edit_file` now accepts `edits: [{old_string, new_string, path?}]`: an
atomic batch applied in order, across one or more files — if any edit
fails to match, nothing is written. Batch error messages carry
`edit {i} of {n}:` prefixes; single-edit messages are unchanged. Every
successful result echoes the edited region (±3 numbered lines, 8 KiB
cap) so the model never re-reads to confirm an edit; echo spans track
final-state positions across earlier edits in the same batch.

Measured on the edit-heavy tasks (B3/B4/B6, one run each, free tier):
all solved; turns 27 → 21; input tokens −21%; post-edit re-reads 1
across three runs (corpus baseline ≈ 4 per run); the batch form was
adopted by the model on first contact. The tree-sitter syntax guard is
deferred (heavy dependency; the verify fusion already refuses broken
Rust).

## 2026-10-06 — Process-group kill for tool commands

`run_command`'s timeout killed only the direct child; a grandchild
spawned by a shell kept running and could hold the output pipes open
(documented debt since the tool review). Fixed with `process-wrap`:

- `exec::run_bounded` spawns commands as process-group leaders
  (`ProcessGroup::leader()`), so one signal reaps the whole tree.
- The stock `KillOnDrop` wrapper only sets tokio's flag — SIGKILL to the
  direct pid, which orphans grandchildren — so a small owned guard kills
  the group (`killpg`) when the future is dropped (timeout, Ctrl-C,
  abort) and disarms itself after a successful wait, before the pgid can
  be recycled.
- Test: a marker process tree survives until the running future is
  aborted, then dies (stale marker processes are pre-killed so earlier
  failed runs cannot mask the probe).

## 2026-10-06 — Real token estimation (o200k) for census and pruning

Context accounting moves from bytes/3 (±30 % error) to real BPE
token counts (~±8 %), without putting a tokenizer in the kernel:

- `loom-kernel` defines a pure `TokenEstimator` trait plus
  `HeuristicEstimator` (the old bytes/3 behavior, the default);
  `DefaultContextCompiler` and `PruningContextCompiler` take an injected
  estimator, and the census and all prune-trigger/reclaim math are now
  in tokens.
- `loom-engine` ships `O200kEstimator` over `tiktoken-rs`
  (`o200k_base`), wired in `Engine::open`; the eval and benchmark
  harnesses use the same estimator for parity.

## 2026-10-07 — Corpus growth: B14 and B15 from shipped fixes

Round 4 of mining Loom's fix history for bugfix tasks; the suite now
spans 15 tasks (B1–B15).

- **B14 — verified result cache** (b534c65): the read-only tool cache
  served hits on the `(tool, args)` key alone, ignoring whether the
  files behind the recorded answer had changed. F2P: a content change,
  a touch without an edit, an absent file appearing, a read file
  disappearing — each a miss. P2P: unchanged repeat hit, different
  args miss, overlapping write invalidates, directory write
  invalidates beneath it, run_command clears everything, disjoint
  write keeps the entry.
- **B15 — failure evidence shaping** (9a50d23): failed `run_command`
  output went to the model raw instead of shaped into evidence. F2P: a
  failed libtest run shapes into test failures plus recent changes,
  multiple failures keep document order, a panic without a message
  still shapes its location. P2P: a successful run keeps its streams,
  an unparseable failure falls back to raw output, empty streams are
  omitted.
- Both proven SWE-bench style offline (seeds fail exactly their F2P
  set, pass their P2P set; real fixes flip everything green — 10/10
  and 6/6) and live: 10/10 gated runs at pass rate 1.0
  (deepseek-flash).

## 2026-10-07 — Verification beyond cargo

`verify` was cargo-only; it now serves the build systems the facts
scanner already detects. The tool reads the workspace's root marker
files (cargo > npm > python > go > make) or takes an explicit
`ecosystem` argument, and the command comes from a fixed
per-ecosystem table — `cargo check/test --message-format=json`,
`go vet/test ./...`, `python3 -m compileall/pytest`, `npm test`,
`make check/test` — never from model input, so no allow-list is
needed. Ecosystems without a build-agnostic check (npm) refuse the
scope rather than guess a script. New evidence parsers compress go
vet/build diagnostics, go test `--- FAIL:` sections, pytest `FAILURES`
sections (with short-summary fallback), and `compileall` syntax
errors into the same structured evidence as cargo; npm/make keep the
bounded raw fallback. The fused verify in `edit_file`/`write_file`
detects the ecosystem too (cargo when no marker) and skips with a
note instead of failing the edit when a scope cannot run. The
serialized-spec total budget rose 4,820 → 5,150 B for the new
parameter's schema; the per-tool budget (840 B) held.

## 2026-10-07 — B1–B17 baseline measured

First full tolerant-mode baseline of the 17-task suite on
deepseek-flash, 5 runs each: 61/75 gated runs (81.3%). Eleven runs
were quota-corrupted — the DeepSeek account drained mid-suite, so B8
and B9 (all runs) never measured and one B7 run aborted; excluding
quota-only failures the signal is 61/64 (95.3%). B7 (heavy task) is
the one genuine failure mode: the model burns the whole 8,192-token
reserved output on reasoning and stops at `max_tokens`. Suite cost by
harness accounting: $0.23. Live runs paused until the account is
topped up; the replay corpus keeps the 70 green recordings.

## 2026-10-07 — Micro-evals, headline economics, and `loom stats`

The efficiency-eval offline half (roadmap item 1): the suite can now
rank harness changes by call quality and economics, not just outcome.

- `loom-kernel` grows `analytics::session_stats`: per-tool fault
  taxonomy (success / invalid-input / execution / timeout / repeated /
  unknown / not-exposed / cancelled), edit-family acceptance,
  identical-call repeats (name + canonical arguments, including
  non-adjacent ones the ledger cannot see), and oversize spills with
  the bytes each kept out of context. Pure over the transcript; the
  same numbers from a live run, a stored session, or a recording.
- Every eval run report — success or failure — embeds a `micro` block
  from that function; `eval_live` also persists each run's session JSON
  under `target/eval/records/sessions/`, and a new offline test
  recomputes every snapshot, cross-checks it against the live block
  (exact equality), and writes `report-micro-<model>.json`.
- The suite headline gains an `economics` block: total and per-solved
  cost, turns per solved (mean/median), token-weighted cache-read
  ratio over successful runs, and the tool-failure rate — each field
  stating the population it is computed over.
- `loom stats [--session PREFIX] [--json]`: the same analytics over
  the durable store (`LOOM_DB`), human table or JSON. Billed usage,
  cache ratios, and prune savings are never estimated from transcripts
  — those live in live-run reports only.
- `aggregates_from` gains per-task micro rollups (tool-error,
  malformed, and edit-apply rates, repeats, spills).

Roadmap item 1 updated to reflect the shipped offline half.

## 2026-10-07 — Run_command under a workspace command policy (02fcaf3)

Backfill: the command execution policy shipped as 02fcaf3 without a
CHANGELOG entry. `.loom/commands.toml` extends the cargo-only default
(`[[command]]` rules: `timeout_secs` ≤ 3600, `max_output_bytes` ≤ 1 MiB,
`env_scrub`); a timeout kills the whole process group; a credential
scrub floor (`*_API_KEY`) always applies; the engine loads the manifest
at open and the spec advertises the allowed programs. Rejections and
timeouts surface as audited tool errors.

## 2026-10-08 — Bootstrap facts: tree, toolchains, project instructions (roadmap item 4)

The frozen Repository-facts block now carries three more surfaces so the
model stops paying early turns to discover them: a depth-limited file
tree (4 levels, 200-line cap with a `(+N more entries)` marker),
toolchain versions probed from detected build systems (cargo/rustc,
node/npm, python3, go), and the project instruction file (AGENTS.md,
falling back to CLAUDE.md, head-bounded to 4 KiB with an explicit
`first N of M bytes` notice). Everything stays in one frozen block per
task, so prompt prefix caching is untouched.

Measured (nemotron-3-super-120b-a12b:free, 15 tasks × 1 run, same
recipe as the pre-change baseline): gated pass rate 11/14 → **13/14**
(78.6% → 92.9%); B9 went from a 50-turn-cap failure to solved in 8
turns; list_dir calls collapsed ~14/run → 1/run and read_file ~25/run →
~7/run. Only B15 (hard synthesis) still hits the turn cap — a model
limit, not a harness one. Cost $0 provider-reported.

## 2026-10-08 — Sandboxed shell (roadmap item 2)

`run_command` now runs any bare program name inside an OS sandbox
instead of an allow-list: Seatbelt (`sandbox-exec`) on macOS,
bubblewrap (`bwrap`) on Linux — both pure process wrappers, so the
workspace's `unsafe_code = "forbid"` stands (this rules out Landlock).
Writes are denied by default and granted only to the canonical
workspace, the system temp dir, and the cargo home; the network is
off; the wrapper leads the process group, so bounded head+tail capture
and group kill are unchanged, and every child inherits the confinement
(a model-run `bash -c` pipeline cannot exceed its parent's grants).
Bare names are resolved on `PATH` before wrapping so a missing program
still fails with the legible "cannot run" error.

The `.loom/commands.toml` manifest is now the escalation layer:
`network = true`, `extra_writable = ["/abs/path"]` (absolute paths
validated), `sandbox = false` for trusted sandbox-incompatible tools,
plus the existing `timeout_secs`, `max_output_bytes`, and `env_scrub`
limits. The credential scrub floor still applies to every child
unconditionally. `verify` honors the same policy when it escalates its
fixed per-ecosystem commands. The default system prompt states the
sandbox contract (eval falls back to `default_system_prompt()`, so
nothing drifts).

Verified empirically: `cargo check -p loom-tools --offline` green
inside the generated Seatbelt profile (4.7 s, target/ writes allowed),
and a network probe inside the profile fails DNS resolution. New
regression test: `touch` to `$HOME` is denied by the sandbox. Tool
spec budget raised 5,150 → 5,350 B for the `run_command` description's
sandbox contract (still under the pre-compaction 5,339 B drift mark).

## 2026-10-08 — Dynamic output reservation

The per-turn output reservation now derives from the model's
capabilities instead of a fixed 8,192: `OutputReservation::Auto` (the
new default) takes the model's declared output maximum, floored at the
old default and capped at 32,768, clamped to half the context window so
input always keeps room. `Agent::with_reserved_output_tokens` pins an
explicit value (still window-clamped). Motivation, measured: the free-
tier eval lost B13 to `finish_reason=length` — nemotron declares a
235,929-token output ceiling that the fixed reservation cut to 8,192.
DeepSeek-shaped models (declared 8,192) are unchanged.

## 2026-10-07 — Live eval on OpenRouter (free tier)

The live eval arm can now run the suite against OpenRouter:
`LOOM_EVAL_PROVIDER=openrouter` with `OPENROUTER_API_KEY` and a full
model id (`nvidia/nemotron-3-super-120b-a12b:free`), fetching the model
catalog before the runs so capabilities resolve exactly like the CLI.
Report and record filenames slug the model id (`/` and `:` become `-`).
Provider-reported charges now win over the DeepSeek price table
(`cost_source: "provider-reported"`; the table stays the fallback and
free-tier ids price at $0 either way). Spot-run: B2 solved by
`nemotron-3-super-120b-a12b:free` in 3 turns, cost $0. The key was also
rotated in the account store (old `...beb6` removed, `...ef7a` active).

## 2026-10-07 — Budget-exceeded rescue in the pruning compiler

`PruningContextCompiler` could not rescue an inner
`ContextError::BudgetExceeded`: the inner compiler errored before the
wrapper's trigger ever fired, so an over-budget projection failed the
task outright. The wrapper now catches the overrun, decides stubs
immediately (the reclaim gate does not apply — the budget is already
blown), recompiles a private stubbed view of the same transcript, and
freezes a checkpoint like any other trigger, so the rescue costs exactly
one cache miss and later turns stay byte-stable. It fails closed with
the original error when nothing is prunable or the pruned view still
does not fit.

Also shipped in the eval harness: `LOOM_EVAL_TOLERATE_FAILURES=1`
opts a live run into measurement mode — per-task asserts are skipped,
every report and the suite headline still land, and the headline is
labeled `"mode": "tolerant"` (strict asserting stays the default) — and
`docs/eval.md` documents the suite's protocol end to end. The roadmap's
near-term list dropped two stale entries: process-group teardown
(shipped as 2bd4928) and this rescue.

## 2026-10-07 — Corpus growth: B16 and B17 from shipped fixes

Round 5 of mining Loom's fix history for bugfix tasks; the suite now
spans 17 tasks (B1–B17).

- **B16 — window-relative pruning trigger** (6c225d9): the trigger
  always returned the flat floor instead of scaling with `trigger_percent`
  of the model's input budget, so pruning fired as if every model had the
  smallest configured window. F2P: trigger scales with a large window, an
  extreme window saturates instead of overflowing, `should_prune` uses the
  real trigger rather than the floor. P2P: a small window still gets the
  floor, a zero-percent trigger is the floor, the comparison is strict
  (equality never prunes), a tiny total never triggers regardless of
  window.
- **B17 — atomic account activation** (5a2c1d7): `set_active` demoted
  every account before checking the target id existed, so activating an
  absent id left the provider with no active account. F2P: an absent id
  leaves the active account untouched. P2P: an absent id on an already
  inactive store leaves everyone inactive, switching to a real account
  demotes the previous one, switching to the already-active account is a
  no-op `Ok`, an empty store rejects any id, switching among three
  accounts leaves exactly one active.
- Both proven SWE-bench style offline (seeds fail exactly their F2P set
  and pass their P2P set; the real fixes flip everything green — 7/7 and
  6/6) and live: 10/10 gated runs at pass rate 1.0 (deepseek-flash), first
  try.

## 2026-10-07 — Corpus growth: B12 and B13 from shipped fixes

Round 3 of mining Loom's fix history for bugfix tasks; the suite now
spans 13 tasks (B1–B13).

- **B12 — effect-based wave scheduling** (59b9989): the scheduler
  serialized every batch containing a mutation instead of deriving waves
  from declared read/write effects. F2P: disjoint writes run
  concurrently, a write and a disjoint read run concurrently, an
  unrelated call rejoins the first wave. P2P: same-path ordering,
  read-only single wave, ancestor-aware overlap, wildcard access, model
  order within waves.
- **B13 — tool-spec compaction** (2c6dfee): serialized schemas carried
  schemars indirection and annotations into every request. F2P:
  `$ref` inlining with the definitions table dropped, `default` and
  `format: "uint"` stripped, integral minimums lose the float tail,
  singleton `oneOf` branches collapse to a plain enum. P2P: root
  `$schema`/`title` drop, descriptions/required/types survive,
  meaningful formats, non-integral bounds, multi-value `oneOf`, plain
  enums.
- Both proven SWE-bench style offline (seeds fail exactly their F2P
  set, pass their P2P set; real fixes flip everything green — 9/9 and
  10/10) and live: 10/10 gated runs at pass rate 1.0 (deepseek-flash).
- Task prompts now carry the "tests are the complete and only
  specification; expect no hidden tests" guard for all new tasks (the
  B10 lesson).

## 2026-10-07 — Corpus growth: B10 and B11 from shipped fixes

Second expansion round: two more real-bug tasks from Loom's fix history.

- **B10 — forced pagination** (dff6bd7): the read line-window
  truncated files that fit the byte budget, forcing multi-turn
  pagination of ordinary files. F2P:
  `default_read_returns_small_files_whole`,
  `oversized_default_read_fills_the_budget_not_the_line_window`; P2P:
  `explicit_limit_is_honoured`, `offset_is_honoured`,
  `empty_file_returns_empty_whole`.
- **B11 — pruning checkpoint churn** (43290a5): a session parked above
  the trigger re-decided every turn, rewriting the checkpoint and
  busting the prefix cache. F2P:
  `stationary_context_carries_the_checkpoint`,
  `repeated_decisions_are_stable`; P2P: `below_the_trigger_keeps_everything`,
  `first_crossing_adopts_a_checkpoint`, `reclaim_subtracts_stub_costs`.
- Same proofs as B8/B9: seeds fail exactly their F2P set, pass their
  P2P set, real fixes flip every declared name green; offline suite
  picks the directories up with no harness edits. Suite now 11 tasks.

## 2026-10-07 — Corpus growth: B8 and B9 from shipped fixes

First corpus-expansion round on the task-directory harness: two more
real-bug tasks mined from Loom's own fix history, each seeding the
parent-commit fault and gating on the fix commit's behavior.

- **B8 — quoted git paths** (af0f2fb): git's C-escaped porcelain output
  (tabs, quotes, non-ASCII) entered facts mangled because only the
  surrounding quotes were stripped. F2P: `quoted_paths_are_decoded`,
  `quoted_rename_decodes_the_destination`; P2P: `plain_paths_pass_through`,
  `rename_takes_the_destination`.
- **B9 — stream head+tail cap** (6a3a3b1): the output cap kept only the
  head, losing compiler errors and panics beyond the first window. F2P:
  `oversized_stream_keeps_head_tail_and_marker`,
  `the_tail_line_of_a_long_listing_survives`; P2P:
  `small_stream_passes_through_whole`.
- Both proven the SWE-bench way offline: seeds fail exactly their F2P
  set, pass their P2P set, and the real fixes flip every declared name
  green. Live spot-run (deepseek-flash): 10/10 gated runs, pass rate 1.0.
- Suite now 9 tasks (B1–B9); `bugfix_seeds_fail_their_gates` picks the
  new directories up automatically — no harness edits.

## 2026-10-07 — Task-directory harness with FAIL_TO_PASS gates

Phase 3 of the suite rigor work: tasks leave the source file and become
data, and the bugfix gates adopt the SWE-bench vocabulary.

- **Tasks as directories** under `crates/loom-cli/tests/eval-tasks/<ID>/`:
  a `task.json` (prompt, tools, workspace kind, gate, `heavy`, optional
  notes), an optional `system-prompt.txt`, and an optional `workspace/`
  tree copied verbatim into a fresh run root (`target/eval/<ID>/`) per
  run. Adding a task is now a directory, not a harness edit; the loader
  sorts, parses, and rejects duplicate ids and bugfix gates without
  FAIL_TO_PASS tests.
- **FAIL_TO_PASS / PASS_TO_PASS by test name**: the bugfix gate parses
  libtest's per-test outcomes instead of counting. FAIL_TO_PASS must be
  failing in the seed and passing after the run; PASS_TO_PASS must stay
  green; protected files are still byte-compared against a pre-run
  snapshot. Outcome gates (B2–B4) are unchanged in spirit — exact bytes,
  substrings, green suite — now declared in the same `task.json`.
- **Suite pass-rate headline**: every live run writes
  `report-suite-<model>.json` — per-task {kind, runs, solved, rate} plus
  the overall gated pass rate — so regressions have one number to move.
- Seed invariants updated: the offline test now proves each bugfix seed
  fails its own declared FAIL_TO_PASS set (and passes its PASS_TO_PASS
  set), and that B6's one-line real fix flips every declared name green.
  Existing records stay replay-valid — task ids and seed bytes are
  unchanged.

## 2026-10-07 — Outcome gates for the fixture tasks (B2–B4)

Rigor follow-up to the real-bug gates: every field task is now graded by
the workspace state it leaves behind, so "the model said done" no longer
passes a run. Lab tasks (B1/B5) stay un-gated on purpose — they are
regression instruments with pinned prompts.

- New `OutcomeSpec` per task: exact file bytes where the prompt pins them
  (B2: `greeting.txt` must hold exactly `hello eval`, one trailing
  newline tolerated), distinctive substrings where it does not (B3:
  `fn sub` in `src/calc.rs`; B4: `summaries.md` must address every source
  document by keyword), and a green `cargo test` where the task is code
  (B3: the fixture suite must still pass with ≥ 1 test).
- Gates run live-only (replay re-executes recorded edits for determinism,
  not grading); the report gains a `gate` object with per-check results,
  and `bug_fixed_rate` in the aggregates now covers B2–B4 too. Requests
  are untouched, so existing records stay replay-valid.
- New offline invariant `outcome_gates_reject_an_untouched_fixture`:
  a fresh fixture fails all three gates, and B3's seeded suite is proven
  green at the expected count so the check discriminates on the code
  change, not a broken baseline.

## 2026-10-07 — Real-bug eval tasks with outcome gates (B6–B7)

The suite's field tasks now include faults that actually shipped in this
repository, graded by outcome instead of transcript success — the shape
SWE-bench uses (FAIL_TO_PASS with protected tests).

- **B6 — the "capability lie"** (shipped `26bcf0b`, fixed `62ea5a6`):
  `parallel_tool_calls` claimed for every tool-capable catalog model.
  Seeded as a self-contained crate with the real regression tests in a
  protected file. Live (N=5, deepseek-flash): **5/5 solved**, 5.8 turns,
  13,592 input tokens mean, 85.7 % cache hit, ≈$0.0007 per run.
- **B7 — the orphaned grandchild** (fixed `2bd4928`): dropped command
  runs killed only the direct child. Genuinely hard: in measurement the
  model understood the fault and attempted real fixes but exhausted its
  turns in compile-test cycles (33–47 exchanges, both runs unsolved).
  Kept as an opt-in `heavy` difficulty probe, not a default-suite task.
- **Gate machinery**: after a gated run the harness executes
  `cargo test` in the fixture (wall-bounded), counts passing tests from
  the libtest summaries (a disabled target prints `ok. 0 passed`, so
  count is what catches manifest gaming), and compares the protected
  files byte-for-byte against a pre-run snapshot. Reports carry a
  `gate` object per run and a `bug_fixed_rate` aggregate; the live eval
  fails if any run's gate fails. An offline invariant test proves every
  seed fails its own gate as shipped and that B6's one-line real fix
  flips it green with the expected count.
- Replay: B6's records replay offline like any other task (recorded
  edits re-execute for determinism); grading itself is live-only.

## 2026-10-06 — OpenRouter provider (free and paid models)

Second adapter, one shared wire. Loom now talks to OpenRouter alongside
DeepSeek; free and paid models travel the same path — pricing is data
(zero vs non-zero), never a branch. Commits `727783d`–`82e01a9`.

- **Cost reporting** (`727783d`): `Usage` gains `cost_usd: Option<f64>`,
  set when a provider reports actual spend (OpenRouter does, DeepSeek
  does not); `Add` merges it (Some+Some = sum). `ModelEvent::Completed`
  stays the sole usage authority; `EVENT_SCHEMA_VERSION` bumps to 2.
- **Shared wire driver** (`26bcf0b`): the OpenAI-compatible plumbing is
  extracted behind a `WireEndpoint` trait — `http` (timeouts, bounded
  error bodies, `Retry-After`), `openai_wire` (request shape, chunk
  accumulation, mid-stream error objects), `wire_stream` (SSE state
  machine). An adapter now owns only configuration and error
  classification, so streaming behaviour cannot drift between providers.
- **OpenRouter adapter** (`26bcf0b`): capabilities, pricing, and
  `parallel_tool_calls` resolve from the `GET /models` catalog
  (`supported_parameters`; unlisted = serialize calls, the safe
  direction). HTTP 402 classifies as permanent `QuotaExhausted` (exhausted
  credits block even free models), 429 as retryable `RateLimited`.
  Catalog reads are bounded at 16 MiB and UTF-8-validated under the idle
  timeout.
- **Provider factory** (`10d26ac`): `EngineConfig.provider` selects the
  adapter; the engine hands catalog-driven adapters a
  `ProviderCatalog` enum (data, not a provider-named field). It enforces
  the 64 K pruning floor (`ModelWindowTooSmall`) and fails loudly with
  `UnknownProvider`/`CatalogMissing` rather than falling back.
- **Frontend-owned catalog** (`387a075`): the CLI fetches and caches the
  catalog beside `loom.db` (`openrouter-models.json`, 24 h TTL); a fresh
  cache skips the network, and a fetch failure falls back to a stale
  cache so a transient outage cannot block a run.
- **Review pass** (`62ea5a6`): fixed a wrong `parallel_tool_calls`
  claim (every tool-capable model got parallel), bounded the catalog
  download, neutralized `EngineConfig`'s catalog field, and collapsed
  duplicate provider-name constants and clocks.
- **Auth consts** (`82e01a9`): `loom-engine` names providers via the
  shared constants (`DEFAULT_PROVIDER = DEEPSEEK_PROVIDER_NAME`) — one
  definition site per name.
- Docs (`fba8c7a`): architecture §9/§10 and the roadmap rewritten around
  the two adapters, the factory, and the frontend-owned catalog.
- `cargo test --workspace` 466 passed / 0 failed / 13 ignored (live
  free-model tests `#[ignore]`d behind `OPENROUTER_API_KEY`, mirroring
  `DEEPSEEK_API_KEY`); clippy and fmt clean.

## 2026-10-06 — Action fusion and a measured context mandate

Two token-efficiency changes, both measured on the eval harness (N=5,
deepseek-flash) before being kept.

- **Fused verification** (`011fd5a`): `write_file` and `edit_file` accept
  an optional `verify` argument (`"check"`/`"test"`) that runs cargo in
  the same call, so a change is proved in the turn that made it instead
  of a separate verify round-trip. The fused output carries the same
  structured failure evidence as the `verify` tool; effects record both
  the file write and the command. Tool-spec guards relaxed to 4,820 B
  total (still ~10 % under the pre-compaction size).
- **Context mandate, trimmed by measurement**: the default system prompt
  teaches exactly two rules — batch independent read-only calls in one
  turn (they run in parallel; never mix a mutation into the batch), and
  set `verify` on writes that touch compilable code. The first draft
  (~885 B) was measured at +31 % input on B2 and cut to ~310 B, which
  held B2 flat and improved B3 by 26 % (16.0 K → 11.9 K input, 6.6 → 5.4
  turns). The eval harness now sources B2–B4 prompts from the engine
  default instead of a frozen copy, so measured and shipped prompts
  cannot drift apart again.
- Verdict kept honest: fusion adoption on deepseek-flash is low (1 of 25
  mutating calls; B3 still prefers one raw `run_command` cargo call),
  and B2's +33 % vs its pre-fusion baseline is the fixed cost of the
  mandate plus two larger specs (~0.9 KB/turn) on a three-turn task.
  B1's unchanged path moved −11 % across the same cycles, putting the
  N=5 noise floor around ±10 %.

## 2026-10-06 — Context pruning: episodic delta-reclaim gate (cache safety)

Follow-up hardening to step 2L. Pruning must be genuinely episodic — one
cache miss per checkpoint, never per turn (ADR-011). `PruningContextCompiler`
previously re-ran `decide()` and adopted a fresh checkpoint on *every* turn
the estimate stayed above the trigger, even when a frozen checkpoint already
existed, rewriting earlier projected messages and busting the prefix cache
each turn.

- `crates/loom-kernel/src/context/pruning.rs`: a new checkpoint is adopted
  only when its reclaim exceeds, by at least `min_reclaim_tokens`, what the
  frozen stubs already reclaim (new `frozen_reclaim(session, stubs)` helper).
  A checkpoint that already covers the transcript is carried verbatim, so a
  session parked above the trigger stays byte-stable.
- New test `frozen_checkpoint_is_not_recomputed_while_parked_above_trigger`
  (fails before the change, passes after).
- `cargo test --workspace` 420 passed / 0 failed / 10 ignored; clippy and fmt
  clean.

## 2026-10-06 — Context pruning: window-relative trigger (step 2L retune)

The 2L long-context benchmark (task B5: read→edit over 8 corpus files)
showed the ADR-011 flat 64 K trigger bounds context but costs ~25 % more
under prefix caching: each checkpoint re-bills the live window at
cache-miss price. The trigger is now **window-relative** — `max(80 % of
the model's input window, 64 K)` — so automatic pruning stays dormant on a
large window and only bounds context as a task approaches the wall (commit
`6c225d9`).

- `PruneConfig` (`crates/loom-kernel/src/context/pruning.rs`):
  `trigger_tokens` → `trigger_percent` (80) + `floor_tokens` (64 K), with a
  `trigger_tokens(max_input_tokens)` helper; the strategy tests pin the old
  flat threshold via a `firing()` config, and a new test covers the
  threshold math.
- Eval harness: `LOOM_EVAL_PRUNE_TRIGGER_TOKENS` forces an absolute trigger
  for measurement.
- B5 N=5 per arm (deepseek-flash, off-peak): no-prune vs flat 64 K — final
  context 127.6 K → 35.7 K (−72 %), input −47.6 %, turns equal (18.6),
  0 failed calls, but cost +24.6 %; the window-relative default arm is
  identical to no-prune (pruning never fires at a 1 M window), and the
  forced-64 K arm reproduces the flat result. Record
  `docs/benchmarks/2L-2026-10-06.md`.
- `cargo test --workspace` 419 passed / 0 failed / 10 ignored; clippy and
  fmt clean.

## 2026-10-06 — Context pruning (ADR-011, step 2L)

`PruningContextCompiler` bounds a long task's projection without mutating
the transcript; measured on a new long-context task (B5) it cuts the final
context ~72 % but **costs more** under prefix caching. Record
`docs/benchmarks/2L-2026-10-06.md`.

- New `crates/loom-kernel/src/context/pruning.rs` wraps
  `DefaultContextCompiler` and applies four deterministic zero-model-call
  strategies in fixed order — dedupe identical read-only calls, supersede
  reads of later-modified paths, drop stale errors, age out unprotected
  read-only results — each replaced by a stub that keeps `call_id`.
  Protected: the tail (≥ 20 K tokens and ≥ last 3 turns), user items,
  assistant text, reasoning, the first user item. It fires only past
  64 K estimated input and ≥ 20 K reclaimable; decisions are frozen in an
  episodic `PruneCheckpoint` returned in `CompiledContext` and passed back
  via `ContextInput`, so the projection is byte-stable between checkpoints.
- Wired on by default in `loom-engine`; `LOOM_EVAL_PRUNE` toggles it in the
  eval harness. New heavy task **B5** (read → edit over 8 corpus files,
  `ToolSet::ReadEdit`, `LONG_CONTEXT_SYSTEM_PROMPT`); offline replay now
  mirrors the live DeepSeek capability table (the stale 128 K window it
  assumed could not replay B5's ~127 K transcript).
- Benchmark (N=5 per arm, deepseek-flash, off-peak): final context
  127.6 K → **35.7 K (−72 %)**, input −47.6 %, turns 18.6 = 18.6, 0 failed
  calls — but cost **+24.6 %** and `cache_miss` +38.7 %, because each
  checkpoint rewrites the prefix and re-bills the live window at cache-miss
  price. Automatic pruning is a window-safety mechanism, not a cost win;
  default disposition pending.
- `cargo test --workspace` 418 passed / 0 failed / 10 ignored; clippy and
  fmt clean.

## 2026-10-06 — Tool-spec compaction (step 2K)

The serialized tool specs are the dominant fixed cost of a short task's
context; they are now compacted without changing what any tool accepts
(commit `2c6dfee`).

- `schema::parameters` renders the parser's own struct schema minus the
  bytes the model does not need: `$schema`/`title` dropped,
  `definitions`/`$ref` inlined (single-element `allOf` collapsed),
  `default` and integer `format:"uint"` removed, an integral `minimum`
  normalised from `1.0` to `1`, and a `oneOf` of single-value string
  enums collapsed to one `enum` list. Descriptions and field docs were
  tightened across all eight tools.
- Schema↔parser agreement is unchanged (per-struct `agreement::check`
  still passes). Two new guards pin the result: `tests/measure_specs.rs`
  (≤ 720 B per spec, ≤ 4,400 B total) and a noise-free assertion in
  `tests/spec_schema.rs`.
- Serialized specs 5,339 → **4,222 B** (−20.9 %); the B2 seven-tool set
  4,535 → 3,568 B, so B2's final-turn `tool_specs` census drops 1,511 →
  **1,189 est tokens** (−21 %). B2 N=5: turns 3.4 → 3.0, input −22 %,
  cost −24 %, 0 failed calls; B1 is unchanged within sample variance.
- `cargo test --workspace` 410 passed / 0 failed / 10 ignored; clippy and
  fmt clean. Record `docs/benchmarks/2K-2026-10-06.md`.

## 2026-10-06 — Frontend-neutral engine boundary shipped (ADR-021, step 2I)

Three commits land the boundary designed below:

- **`loom-kernel`** — the former `loom-core`, renamed; the neutral loop/domain
  crate stays free of provider/tool/HTTP/SQLite dependencies (ADR-001).
- **`loom-engine`** (new) — the composition root and control plane every
  frontend shares: `EngineConfig` + credential resolution (`auth`, `paths`),
  one concrete provider, the built-in tool registry, session lifecycle +
  persistence, and the driving API (`Engine::{open,new_session,run_task,
  save_session}`, `EngineError`). No stdout/stderr and no terminal I/O.
- **`loom-core`** is now the **umbrella facade** — the single crate a frontend
  imports — re-exporting the kernel, the engine, the providers, and the tools,
  plus the driving API at its root. `loom-cli` depends on `loom-core` alone
  among workspace crates and is reduced to args, tracing, signals, rendering,
  and exit codes.

No behavior change. `cargo test --workspace` 288 passed / 0 failed; `cargo
clippy --workspace --all-targets` clean.

## 2026-10-06 — Frontend-neutral capability surface design (ADR-021)

Design decision. Loom must not be bound to the CLI: TUI, GUI, and server
frontends share one engine, reached in-process, instead of duplicating
credential, provider, tool, and persistence wiring.

- **ADR-021 accepted (revised the same day)** — **`loom-core` becomes the
  umbrella facade**: the single crate every frontend imports, re-exporting a
  renamed neutral kernel (`loom-kernel`, today's `loom-core`), a new
  `loom-engine` composition root (config + credentials, provider/tool
  composition, workspace defaults, session lifecycle/persistence, driving API
  + command channel), `loom-providers`, and `loom-tools`.
- **Bindings are in-process (zero latency):** a frontend links `loom-core` and
  calls the engine directly; events arrive through the in-process `EventSink`.
  The versioned serializable `EventEnvelope` is kept only for an optional
  out-of-process `loom-server`.
- `loom-kernel` preserves ADR-001 (no provider/tool/HTTP/SQLite). The earlier
  draft — a *separate* `loom-engine` boundary with frontends importing several
  crates — is recorded as a rejected alternative.
- Planned as plan-iteration-2 step **2I** (validation row V26); it is the
  **first** I2 step, so the harness (2A) composes provider/tools through the
  engine and the SQLite store (2C) is wired once inside `loom-engine`.
- architecture.md §1/§7/§12 updated with pointers.

## 2026-10-06 — Tool review follow-ups

Post-review hardening of the mutating tools and the failure policy
(commit `8da1431`); no intended behaviour change for the passing B2 path.

- `run_command` now captures stdout/stderr with a per-stream cap (64 KiB) and
  discards the remainder instead of buffering the whole child output.
- `edit_file` refuses files larger than 8 MiB before reading them.
- The failure `Hint:` is clamped to keep tool content within
  `MAX_TOOL_OUTPUT_BYTES`.
- `ToolStarted` / `ToolCompleted` are emitted only for calls that run; a
  refused repeat emits no tool events.
- Documented: a timeout kills only the direct child, not its process group
  (deferred to the sandbox/policy iteration); `ledger_key` depends on
  `preserve_order` staying off.
- Verification: loom-core lib 96 / agent_loop 16 / loom-tools 72 tests pass;
  clippy clean; B2 live 5/5 success, 0 tool-call failures.

## 2026-10-06 — Loop-owned tool-failure policy

**Zero tool-call failures required** for the DeepSeek benchmarks. The first B2
run recorded one failure that was not a real fault: listing a not-yet-existing
directory is a legitimate existence probe, and nothing stopped a failed call
from being repeated blindly.

- **ADR-020 accepted** — two layers. (1) `read_file` / `list_dir` / `search`
  return a *successful* informational result when the target does not exist;
  real faults (invalid input, permissions, invalid UTF-8, non-regular file,
  timeout) stay errors. (2) the agent loop keeps a per-task `FailureLedger`
  keyed by `(tool name, canonical arguments)`: an identical call that already
  failed is refused as the new `ToolFailure::RepeatedCall` without executing,
  and any **success** clears the ledger. Real failures get a deterministic
  next-step `Hint:`.
- Engine (`agent/tools.rs`) unchanged; no tool call is automatically retried.
- Guard tests added in `crates/loom-core/tests/agent_loop.rs`; `cargo clippy
  --workspace --all-targets` clean.

## 2026-10-06 — Iteration 1 completed (mutating tools)

**B2 passed end to end, 5/5 runs** — the second and last Iteration 1 success
criterion. `docs/benchmarks/B2-2026-10-06.md`.

- **ADR-019 accepted** — `write_file`, `edit_file`, `run_command` added to
  `loom-tools`; registered in the CLI; the system prompt now permits writes
  and commands. No engine change (mutating calls already serialize the batch).
  `ToolOutput`/effects deliberately unchanged — deferred to iteration 2 step
  2B/2G.
- `run_command` runs bare allow-listed programs only (no shell); a non-zero
  exit is a successful, model-visible result.
- The eval harness gains a generated fixture crate and `LOOM_EVAL_TASKS`
  selection; B2–B4 are now runnable (B1 keeps its 2026-10-06 baseline).
- `crates/loom-tools` unit tests: 70 passing. `cargo clippy --workspace
  --all-targets` clean.

## 2026-10-06 — Iteration 1 freeze lifted

**B1 passed end to end.** Six live runs (5× deepseek-flash, 1×
deepseek-v4-pro), all successful, zero retries, zero dropped events. Full
per-run records and aggregates: `docs/benchmarks/B1-2026-10-06.md`.

- **ADR-004 addendum** — implementation contract changes recorded under the
  §17 freeze rule (commit `cb6119d`): `TaskError::Internal` split from
  `Protocol` (internal defects are never retried); `events_dropped` is
  sampled after the terminal event (final-flush race fixed).
- **ADR-006 addendum** — `ContextInput` gains required `model: &'a str`
  (commit `8f1447c`): per-model budget policy requires the model id in the
  compiler's scope.
- **ADR-012 accepted** — Iteration 2 scope merged into a single definition
  (`docs/iteration-2.md`; architecture §18 now defers to it). Long-term
  memory and `session_search` move to Iteration 3. Three seams settled
  ahead of write tools: tool effects recorded at execution time,
  `ArtifactRef` out-of-context result storage, and **persist-then-commit**
  turn ordering (durable before in-memory, superseding the former §18.2
  wording).
- `docs/plan-iteration-2.md` completed: ordered steps 2A–2H; the evaluation
  harness (B1–B4, N ≥ 5 runs, record/replay provider, context census) is
  step 2A and gates all optimization work.
- `docs/iteration-2.md` rewritten as the merged single definition, updated
  with B1 evidence (input tokens 50–90× output; prefix-cache economics;
  run variance).

Iteration 1 contracts in `docs/architecture.md` §1–17 remain the baseline
that Iteration 2 extends.
