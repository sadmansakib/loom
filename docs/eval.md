# The eval suite

Loom grades itself the way SOTA agent harnesses do: outcome gates on real
bug-fix tasks, deterministic offline proofs, and N-run live measurement.
The core corpus is mined from Loom's own fix history — every task is a
fixed bug, miniatured into a self-contained crate — and is complemented
by a **scale tier** (NAV1/BIG1/MULTI1/LONG1) that exercises the axes the
miniatures cannot: many-reads navigation, large-file editing, cross-file
consistency, and long horizons that cross the pruning floor.

## Layout

```
crates/loom-eval/tests/eval-tasks/<ID>/
  task.json          # id, prompt, tools, heavy, tier, gate, notes
  system-prompt.txt  # optional pinned prompt (lab tasks only)
  workspace/         # the miniature crate the agent works on
```

Two task kinds:

- **field** (bugfix) tasks carry a `gate` and are graded by outcome;
- **lab** tasks (B1, B5) keep a pinned `system-prompt.txt` as narrow
  regression instruments and carry no gate.

Every field task carries a **tier** — the human-time difficulty
stratification Multi-SWE-bench validated (`s` ≤15 min, `m` 15–60 min,
`l` 1–4 h, `xl` ≥4 h). The suite headline reports the pass rate per
tier, because a blended rate hides the tier cliff (field data: 84.7 %
under 15 min falls to 33.1 % at 1–4 h).

### The scale tier

The B-series miniatures are honest bug fixes, but every one is a
single-fault crate of at most a few hundred lines — no amount of them
measures navigation, large-file editing, or cross-file work. Four tasks
close those gaps:

- **NAV1** (tier m) — many-reads: a 45-module units crate; the fault
  sits in one constant among hundreds of near-identical siblings, so
  finding it takes search + reads, not luck.
- **BIG1** (tier m) — large-file: a single 614-line `lib.rs` with 52
  string utilities; the fix is one line inside it.
- **MULTI1** (tier l) — multi-file: twelve widget files whose schema
  constants must agree; five drifted, so the fix necessarily touches
  five files (Multi-SWE-bench: resolve rate falls off a cliff past one
  file).
- **LONG1** (tier l, heavy) — long-horizon: a complete interpreter spec
  across ten chapters plus a stubbed evaluator; the final request is
  large enough to cross the pruning floor, the only in-house task that
  does. Runs only when named in `LOOM_EVAL_TASKS`.
- **POLY1** (tier m) — polyglot: eight fixes across shell, Python,
  JavaScript, YAML, TOML, JSON, Make and a CRLF text file in one
  workspace. Outcome-gated (`contains` needles, `tests_green: null` —
  no `cargo test`, the gate never assumed Rust); the language-shaped
  failures mirror the editor bench's polyglot families.
- **SHELL1** (tier m) — modern shells: one real defect per dialect —
  bash strict mode (`pipefail`), zsh no-match glob handling
  (`setopt null_glob`), fish syntax (`set -gx`, not `export`),
  PowerShell property interpolation (`$($row.Name)`).
- **DB1** (tier m) — databases and dialects: SQLite (missing WHERE),
  Postgres (`ON DELETE CASCADE` FK), MySQL (backtick-quoted reserved
  word), Mongo (`"unique": true` index), Redis (`allkeys-lru` policy),
  plus an implicit-to-explicit `INNER JOIN`. No engines run — the gate
  checks the artifacts, so no dialect needs a server.
- **LANG1** (tier l) — language sweep: nine languages, one idiomatic
  defect each — Go (unchecked error), Java (`equals` vs `==`), C
  (`snprintf` sizing), C# (`?? string.Empty`), Ruby (string
  interpolation), PHP (string interpolation), TypeScript (typed
  parameter), Kotlin (elvis vs `!!`), Swift (`guard let` vs force
  unwrap).
- **FPATH1–FPATH3** (tier s) — fast-path-shaped: a constant change
  (`MAX_RETRIES` from 3 to 5), a cross-file rename
  (`parse_opts` → `parse_options`, definition plus a second-file call
  site), and a whole-workspace format (`cargo fmt` against an
  exact-file gate). These ride the same outcome gates as everything
  else; the harness's fast-path seam may solve them with zero model
  invocations (`FastPathHit`, `model_requests = 0` in the run record),
  which is the measurable point — the grammars are general, not
  task-shaped, so solving them via the analyzer or the full loop are
  both honest outcomes.
- **FPATH4** (tier s) — dependency update: bump an unused
  `tiny-dep = "1.0.0"` to `2.0.0` in `Cargo.toml`, verified offline by
  `cargo metadata --no-deps --offline`. Gated like its siblings: the
  `contains` check pins the bumped manifest line and `tests_green`
  rebuilds, so a manifest that parses-and-builds is part of passing.

## Gates (SWE-bench shape)

A bugfix gate names tests, not diffs:

- `fail_to_pass` — tests that fail on the seed and must pass after;
- `pass_to_pass` — tests that pass on the seed and must still pass;
- `protected` — paths the agent may not touch (the tests themselves).

Matching is by bare libtest function name, exactly as `cargo test` prints
it for integration tests. Grading re-runs the workspace suite and compares
outcomes; rewriting tests, disabling the suite, or leaving F2P red all
fail the gate.

## Offline proofs (run on every commit)

`cargo test -p loom-eval` enforces the corpus invariants
without spending tokens:

- `bugfix_seeds_fail_their_gates` — every seed fails **exactly** its F2P
  set and passes **exactly** its P2P set (no accidental extra signal);
- `outcome_gates_reject_an_untouched_workspace` — the gate rejects the
  seed as-shipped, so a no-op agent can never pass;
- `eval_replay_recordings` / `eval_replay_synthetic` — recorded exchanges
  replay deterministically through the engine.

## Live protocol

```
LOOM_EVAL_TASKS=B1,B16,B17        # subset; unset = all non-heavy tasks
LOOM_EVAL_RUNS=5                  # runs per task (default 5)
LOOM_EVAL_TOLERATE_FAILURES=1     # measurement mode; default strict
LOOM_EVAL_BARE=1                  # bare-loop arm; default full
LOOM_EVAL_CONCURRENCY=16          # tasks in parallel (default 1 = sequential)
LOOM_EVAL_RPM=36                  # shared requests/min meter across all runs
LOOM_EVAL_RUN_TIMEOUT_SECS=1800 # per-run watchdog; stalled run fails instead of hanging
LOOM_EVAL_INFRA_RETRIES=1     # re-measurements per infrastructure-lost run; 0 disables
cargo test -p loom-eval eval::eval_live -- --ignored --nocapture
```

The provider is DeepSeek by default. `LOOM_EVAL_PROVIDER=openrouter`
switches the live arm to OpenRouter: credentials resolve through the
engine exactly like the CLI (keychain account or `OPENROUTER_API_KEY`),
the model catalog is fetched before the runs (capabilities come from
it, like the CLI), and `LOOM_EVAL_MODEL` takes a
full OpenRouter id — `nvidia/nemotron-3-super-120b-a12b:free`,
`google/gemma-4-31b-it:free`. Report and record filenames slug the model
id (`/` and `:` become `-`). Provider-reported charges (the `cost_usd`
the account actually pays, `cost_source: "provider-reported"`) win over
the DeepSeek price table, which stays the fallback; free-tier ids price
at $0 either way. The free tier is rate-limited (requests/day, not
dollars) — check the dashboard before a full-suite run.
`LOOM_EVAL_PROVIDER=nvidia` targets NVIDIA NIM's free-trial tier the
same way (keychain account or `NVIDIA_API_KEY`; prices at $0 by
construction), which is roadmap item 1's zero-cost candidate.

Scheduling: sweeps are run-index-major (every task completes run N
before any starts run N+1), so an interrupted run leaves a complete
N-run sweep, not N half-finished tasks. Tasks run concurrently up to
`LOOM_EVAL_CONCURRENCY`; runs within a task stay sequential (each task
wipes and re-copies its own run root). `LOOM_EVAL_RPM` meters the
total request rate across all concurrent runs — size it under the
provider cap (e.g. 36 against NVIDIA NIM's 40 rpm); the kernel's
RateLimited retry remains the backstop.

- **Strict mode** (default): asserts every run green and every gate
  solved — the CI contract.
- **Tolerant mode**: survives failed runs, still writes every per-task
  report and the suite headline, and labels the report
  `"mode": "tolerant"`. Use it for baselines: a measurement run must not
  abort at the first failure it exists to observe.

### The bare-loop arm (roadmap item 1)

`LOOM_EVAL_BARE=1` runs the same tasks on the basic-harness baseline:
the engine attaches no repository facts, compiles plain append-only
context (no pruning regardless of `LOOM_EVAL_PRUNE`), disables the
completion-gate and failure-ledger feedback, and the registry shrinks
to the five primitive tools (read/list/write/edit/shell — a Coding task
loses `search`, `verify`, and `artifact_read`). The Scaffold-Effect
finding is the point: harness choice moves tokens-per-solved by up to
40× while pass rates stay put, and only an A/B against a bare loop
measures what the levers actually buy. Bare artifacts are suffixed
`-bare` (reports, records, sessions, suite headline) so both arms
coexist under `target/eval/`; the report carries `"arm": "bare"`.

Reports land in `target/eval/`: `report-<ID>-<model>[-bare].json` per
task (per-run metrics + aggregates) and
`report-suite-<model>[-bare].json` (the headline: gated pass rate over
every gated run of the session, pass rate **per tier**, the failure
category vector, and the `economics` block — total and per-solved cost,
tokens per solved amortized over all billed runs, the
expensive-failure ratio (share of spend on unresolved gated runs),
turns per solved mean/median, idle turns and idle-turn rate, the
token-weighted cache-read ratio over successful runs, and the
transcript-derived tool-failure rate).

The pass-rate denominator counts **verdict-bearing** runs: graded, and
not lost to infrastructure. The grader evaluates the workspace however
a run ended, so a run killed by the wire or by the per-run watchdog can
still carry a gate — those runs are excluded, because grading a crashed
run awards solves to crashes and charges crashes to the agent. The
headline reports `attempted_runs` alongside `gated_runs`, and each field
task reports `attempted` and `infra` beside its `rate`, so a denominator
thinned by flaky infrastructure stays visible instead of silently
flattering the rate. Infra runs are the `model_error`,
`watchdog_timeout`, and legacy unlabeled (`unknown`) reasons; they are
tallied apart from agent negatives and may be re-run, and the agent's
own negatives (`max_turns`, `gate_failed`, …) are never re-run.

### The infrastructure retry pass

After the sweep, every run that lost to infrastructure gets
`LOOM_EVAL_INFRA_RETRIES` extra attempts (default 1, `0` disables), and the
retry's report replaces the original in place. Excluding those runs keeps
`pass_rate` honest; it does not shrink the sample. On the 2026-10-17
corpus 23 of 115 attempts (20%) were infrastructure losses, which is enough
to leave every headline reporting a sample size nobody trusts.

The retry is the **same** [`RunPlan`](eval.rs) the first sweep used — same
arm, same throttle, same watchdog — so a repaired run is wired like the run
it replaces. It reuses the original run number, so the retry overwrites the
same record and session artifact, and the replacement carries a
`retried_from` field naming the reason it replaces.

Only infrastructure is retried. A `max_turns` or `gate_failed` run is the
measurement, not a broken wire: re-rolling it until it passed would grade
the harness's luck instead of the agent's capability. A retry that comes
back infrastructure too means the endpoint is down — the run stays reported
as infrastructure (the honest measurement of a broken provider rather than
a hole in the sample) and the pass prints a per-run warning. The retry is
also bounded: a dead provider costs one wasted request per slot, not N.

## Micro-evals (call quality from the transcript)

Every run report — success or failure — embeds a `micro` block computed
by `loom_core::kernel::analytics::session_stats` from the run's
transcript: per-tool fault taxonomy (success / invalid-input /
execution / timeout / repeated / unknown / not-exposed / cancelled),
edit-family acceptance, identical-call repeats (name + canonical
arguments), and oversize spills (with the bytes each kept out of
context). `eval_live` also persists each run's full session JSON to
`target/eval/records/sessions/<ID>-<model>-<run>.session.json`; the
offline test `eval_micro_walks_recorded_sessions` recomputes every
snapshot, cross-checks it against the live run's embedded `micro`
block (exact equality), and writes `report-micro-<model>.json` — the
token-free half of the release-gate numbers. `loom stats --json`
computes the same shape from any stored session.

What the transcript does *not* carry — billed usage, cache hit/miss,
prune decisions — is never estimated from it; those numbers exist only
in live-run reports.

## The editor bench (token-free)

`cargo test -p loom-eval editbench` runs the deterministic
`edit_file` micro-bench (the PR-Edit idea: editor competence is
measurable offline, and downstream resolve rate follows it). Families:
apply-by-anchor across file-size buckets (40 / 240 / 1200 / 3000 lines
— past the largest live-corpus file), ambiguity refusal (a repeated
anchor must be rejected, file untouched), not-found refusal,
whitespace-repair, batch atomicity (a bad edit voids the whole batch),
cross-file batches, unicode byte-exactness, and the oversize cap.
The polyglot extension adds nine language-shaped families: Python
indentation blocks, JSON escaped quotes (parse-verified), YAML and
TOML indent/table edits, JS template literals, shell quoting, a
two-part CRLF case (a `\r\n` anchor applies; a bare-`\n` anchor must
*not* match a CRLF file — no cross-ending fuzzy match),
extension-less Makefiles, and one atomic batch touching six languages.
The breadth sweep goes further: Go's mandatory tabs, C `snprintf`
sizing, Java `equals`, C# null-coalescing, Ruby squiggly heredocs and
interpolation, PHP `$var` interpolation, TypeScript annotations,
Kotlin elvis, Swift `guard let`, Postgres `$$` bodies and MySQL
backtick identifiers, zsh `setopt` + fish `set -gx`, and PowerShell
subexpressions — every mainstream language family and shell dialect
the agent may be handed.
Report: `target/eval/report-editbench.json`. No tokens, no network —
it runs on every commit.

## External tasks (comparability anchor)

Field tasks prove Loom against its own history; an external suite
proves it against the world. `cargo run -p loom-eval --bin
multiswe-import -- --jsonl <multi-swe-bench.jsonl> [--instance ID,..]
[--max N] [--language rust] [--repo-allowlist org/repo,..] [--out <dir>]`
imports Multi-SWE-bench instances as `EXT-<instance_id>` heavy tasks.
The importer is a network tool and never runs in CI; once imported, the
standard offline proofs validate the seeds like any other task (F2P must
fail at base, every P2P must pass). A small imported slice (the Lite-80
lesson: ~80 instances preserve rankings at ~23 % of cost) is the
roadmap's comparability half.

**Dataset.** HuggingFace `ByteDance-Seed/Multi-SWE-bench`, raw JSONL per
language (`resolve/main/rust/<Org>__<repo>_dataset.jsonl`). The importer
speaks both record shapes: the HF shape (`org`/`repo`, `base.sha`,
`fix_patch`, `f2p_tests`/`p2p_tests` name-keyed dicts, `title`/`body`
issue text) and the classic SWE-bench shape (`repo`, `base_commit`,
`patch`, `FAIL_TO_PASS`/`PASS_TO_PASS` arrays). The task prompt embeds
the issue title + body (clipped at 1500 bytes on a character boundary) —
the agent sees what a human filer saw, minus the gold patch, which lands
outside the workspace as `solution.patch` (provenance only). Records
with an empty F2P set are refused: nothing fails at seed means the task
is not gradeable as a bugfix.

**Provenance hygiene.** Imports do not `git clone` (a full clone carries
post-fix history an agent could `git log --all` mine). Each workspace is
`git init` + a single `--depth=1` fetch of `base.sha` + detached
checkout: exactly one commit, zero refs, no future. The empty
`[workspace]` table is appended to the root `Cargo.toml` when missing so
cargo does not walk up into Loom's tree — a build fix, not a test or
grading change.

**Platform validity.** Instances whose P2P lists name tests the upstream
itself cfg-gates out on macOS cannot be graded on darwin and are dropped
at import-validation time — dropped with a machine-checkable reason
(the `#[cfg(not(target_os = "macos"))]` line in the upstream test), never
edited around: `BurntSushi__ripgrep` (regression::r131, guarded since
2016) and `sharkdp__fd` (test_exec_invalid_utf8) are unsatisfiable on
darwin wholesale. Pure libraries carry no such guards.

**Imported slice (2026-10-10, darwin, all seeds validated):**

| task | repo | F2P | P2P |
|---|---|---|---|
| EXT-tokio-rs__bytes-732 | tokio-rs/bytes | test_get_int | 9 |
| EXT-tokio-rs__bytes-721 | tokio-rs/bytes | mut_shared_is_unique | 107 |
| EXT-rayon-rs__rayon-986 | rayon-rs/rayon | drain_vec_empty_range_yielded | 227 |
| EXT-clap-rs__clap-5873 | clap-rs/clap | ignore_errors::did_you_mean | 803 |
| EXT-tokio-rs__tracing-2001 | tokio-rs/tracing | no_collector_disables_global | 73 |

Five instances across four real projects, 1–803 P2P each; grade pool
verbatim from the dataset. Hefty clap and rayon P2P sets exercise
no-regression discipline at real-repo scale.

## The hard gate (release preset)

`LOOM_EVAL_TASKS=hard-gate` runs every task that carries a gate —
heavy tasks included, lab benches (B1/B5) never. The preset is derived
from the corpus, not listed, so the gate grows only by addition: a new
gated task cannot be silently left out, which is the anti-benchmaxxing
rule the whole suite runs under (no task selection, no test avoidance,
no target edits). Today that is 28 tasks: the B-series bugfix corpus,
the scale tier (NAV1, BIG1, MULTI1), the polyglot tasks (POLY1, the
shell sweep SHELL1, the database sweep DB1, the language sweep LANG1),
the long-context tier (B7, LONG1), and the imported Multi-SWE-bench
slice (5 EXT tasks). The default
suite (no `LOOM_EVAL_TASKS`) stays at the non-heavy tasks to bound time
and cost; the hard gate is what a release must pass, both arms:

```text
LOOM_EVAL_TASKS=hard-gate LOOM_EVAL_TOLERATE_FAILURES=1 \
  cargo test -p loom-eval eval::eval_live -- --ignored --nocapture
LOOM_EVAL_TASKS=hard-gate LOOM_EVAL_BARE=1 LOOM_EVAL_TOLERATE_FAILURES=1 \
  cargo test -p loom-eval eval::eval_live -- --ignored --nocapture
```

Reports land as `target/eval/report-suite-<model>.json` and
`report-suite-<model>-bare.json` — the A/B pair.

Split campaigns merge without re-spend: a live invocation overwrites
its suite headline with only the tasks it ran, so when breadth tasks
land mid-campaign the addendum's headline would clobber the main
run's. The `eval_suite_union` test (`--ignored`, no spend) rebuilds
the headline from the per-task report files — each embeds its full
`runs` array — by reusing `suite_headline` verbatim, writing
`report-suite-<model><arm>-union.json`. Default scope is the hard
gate with every report required; a missing file is a release-gate
violation, not a silent skip.

## Units and their honesty rules

- **census** — exact serialized bytes of the model request; deterministic
  and provider-agnostic; the primary token-efficiency metric.
- **est_tokens** — bytes/3 budget heuristic (what the engine itself
  reasons with).
- **tokens / cost_usd** — provider-billed, per-model evidence only. Cost
  is computed from per-model price tables with the peak/off-peak window
  labeled (`price_window`), because the provider does not report cost.

Noise floor: ±10% on repeated identical runs; B3 has shown up to 2.5×
per-run spread. Compare suites by medians over N ≥ 5 runs, never single
runs.

## Authoring a task from a fix commit

1. Mine `git log` for a fix with a pure, testable behavior change.
2. Miniature it into a no-dep (edition 2021) crate: `workspace/src/lib.rs`
   carries the **seed** = pre-fix behavior with a single crisp fault
   locus; `workspace/tests/regression.rs` carries the contract.
3. Write F2P tests that fail on the seed and P2P tests that pass on it —
   one contract, no vestigial knobs. The prompt must kill hidden-test
   anxiety: state that the tests under `tests/` are the complete and only
   specification and that no hidden tests exist.
4. Prove the seed fails exactly F2P and passes exactly P2P.
5. Copy to a temp dir, apply the **real** fix, prove all-green, delete
   the copy, strip `workspace/target`.
6. Offline suite, then a live spot-run of the new tasks (N=5), then
   CHANGELOG, then commit.

## Records hygiene

Every live run writes `target/eval/records/<ID>-<model>-<run>.jsonl`.
A record of an **errored** run (transcript aborted) is replay-invalid and
will fail `eval_replay_recordings` offline — delete it before the next
offline suite run.

## Latest baseline

See `target/eval/report-suite-<model>.json` for the current headline;
baselines are recorded in `CHANGELOG.md` when taken.
