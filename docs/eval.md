# The eval suite

Loom grades itself the way SOTA agent harnesses do: outcome gates on real
bug-fix tasks, deterministic offline proofs, and N-run live measurement.
The corpus is mined from Loom's own fix history — every task is a fixed
bug, miniatured into a self-contained crate.

## Layout

```
crates/loom-cli/tests/eval-tasks/<ID>/
  task.json          # id, prompt, tools, heavy, gate, notes
  system-prompt.txt  # optional pinned prompt (lab tasks only)
  workspace/         # the miniature crate the agent works on
```

Two task kinds:

- **field** (bugfix) tasks carry a `gate` and are graded by outcome;
- **lab** tasks (B1, B5) keep a pinned `system-prompt.txt` as narrow
  regression instruments and carry no gate.

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

`cargo test -p loom-cli --test eval` enforces the corpus invariants
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
DEEPSEEK_API_KEY=... cargo test -p loom-cli --test eval -- --ignored --nocapture
```

The provider is DeepSeek by default. `LOOM_EVAL_PROVIDER=openrouter`
switches the live arm to OpenRouter: the key comes from
`OPENROUTER_API_KEY`, the model catalog is fetched before the runs
(capabilities come from it, like the CLI), and `LOOM_EVAL_MODEL` takes a
full OpenRouter id — `nvidia/nemotron-3-super-120b-a12b:free`,
`google/gemma-4-31b-it:free`. Report and record filenames slug the model
id (`/` and `:` become `-`). Provider-reported charges (the `cost_usd`
the account actually pays, `cost_source: "provider-reported"`) win over
the DeepSeek price table, which stays the fallback; free-tier ids price
at $0 either way. The free tier is rate-limited (requests/day, not
dollars) — check the dashboard before a full-suite run.

- **Strict mode** (default): asserts every run green and every gate
  solved — the CI contract.
- **Tolerant mode**: survives failed runs, still writes every per-task
  report and the suite headline, and labels the report
  `"mode": "tolerant"`. Use it for baselines: a measurement run must not
  abort at the first failure it exists to observe.

Reports land in `target/eval/`: `report-<ID>-<model>.json` per task
(per-run metrics + aggregates) and `report-suite-<model>.json` (the
headline: gated pass rate over every gated run of the session, plus the
`economics` block — total and per-solved cost, turns per solved
mean/median, token-weighted cache-read ratio over successful runs, and
the transcript-derived tool-failure rate).

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
