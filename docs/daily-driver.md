# Daily-driver probes

SWE-bench Verified measures the frozen benchmark protocol. It says nothing
about the daily-driver claim: *a developer pointing loom at their own messy
repo and getting useful work done*. This document defines the cheap,
repeatable probe that covers that gap, and records every run under the same
honesty rules as `eval.md`.

## Protocol

### Task set

A probe repository is a small, self-contained crate with **seeded defects
and features** — never a task lifted from a benchmark. Three task classes,
one instance each to start:

1. **Fix** — a function with a real bug plus a failing test that pins the
   intended behavior. Success = `cargo test` passes and the diff touches
   the guilty code.
2. **Implement** — a stubbed function with a doc comment spec plus a
   failing test. Success = test passes, implementation matches the spec.
3. **Refactor** — a signature change that ripples across modules (rename +
   parameter change), with tests already updated to the new shape.
   Success = workspace compiles, tests pass, no stale references remain.

The probe repo lives outside the loom checkout, is recreated fresh from
[`docs/benchmarks/daily-driver-fixture.py`](benchmarks/daily-driver-fixture.py)
before every run, and is thrown away after. loom never grades itself:
success is judged by `cargo test` + human diff review, never by the agent's
own claims.

### Vehicles

- **$0 default**: the nano gateway (`--provider custom --base-url …`,
  `CUSTOM_API_KEY`), a weak free model. Expected to wander; the probe
  records what the harness does about it (gate refusals, nudges, turn
  ceilings). This is the harness-behavior vehicle, not a capability claim.
- **Funded option**: `deepseek --model deepseek-flash` off-peak
  (22:30–06:30 BST) for a capability-grade signal.

### Metrics

Per task: completed/stalled, terminal reason, model requests, turns,
whether tests were consulted (schema-11 `tool_completed` content heads),
loop-nudge count (schema-11 `loop_nudge` events), final `cargo test`
outcome, diff size. Recorded in the run log below, verbatim, wins and
losses alike.

### Honesty rules

Same as `eval.md`: no rerolling until a win without labeling it, no
protocol edits mid-probe, every recorded number traceable to a saved
`--json` run artifact.

## Run log

| Date | Vehicle | Tasks | Outcome | Notes |
|---|---|---|---|---|
| 2026-10-23 | nano gateway (`kilo/nvidia/nemotron-3.5-lightning:free-1M-txt`, $0) | fix / impl / refactor | **3/3 success** | fix: 8 turns, 9 tool calls, median bug fixed exactly (even-pair averaging); impl: 11 turns, rotate_right implemented (byte-based rotation — passes the pinned tests, though the doc says code points: spec-fidelity note, not a failure); refactor: 13 turns, `char`→`&str` signature changed and the `tag` caller updated. All runs `end_reason: completed`, **zero loop nudges, zero gate refusals** — the weak model wandered not; tasks are single-crate daily-driver scale. Schema-11 forensics (tool args/result heads, per-run `--json` artifacts) verified live in this probe. |
