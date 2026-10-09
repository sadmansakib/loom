# Changelog

All notable changes to Loom. Format loosely follows Keep a Changelog; this
project versions by iteration, not semver.

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
