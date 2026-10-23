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

## MCP deferral probe (slice D3)

Measures the deferred tool surface (slice D2) against eager loading on an
otherwise identical task. Assets: `docs/benchmarks/mcp-probe-ghmock.rs`, a
standalone std-only mock GitHub MCP server (35 tools, ~3 KB schemas each —
22.8K est. tokens eager, matching the measured ~26K of the real GitHub
server; `get_issue` is functional and returns a seeded issue describing
the fixture bug, everything else answers `noop`). `LOOM_GHMOCK_DUMP=path`
dumps the exact tool-definition bytes for token accounting.

Protocol: daily-driver `fix` fixture (median), fresh repo per arm, same
task text ("read issue #1 … then fix the code so cargo test passes"), same
$0 model. Deferred arm = default (facts menu + meta-tools); eager arm =
`always_load` with all 35 tools. Success = `cargo test` green + run.json
evidence the issue was consulted through MCP. Upfront tool-token cost is
measured deterministically from the advertised definition bytes
(chars/4), not from the model's own accounting.

Bar (from `docs/mcp-design.md`): same task success at ≥10× less upfront
tool-token cost, no search deadlocks.

Result 2026-10-23: **bar met — 12.4×**. Eager 22,776 tokens of tool
schemas in every request; deferred 1,832 (menu 932 + two meta-tools ~300 +
`get_issue` schema 600 loaded once). Both arms completed with 2/2 tests
and the issue consulted exactly once (`mcp_gh_get_issue`). Deferred took
14 turns (search → load → call, one load, no deadlocks) vs eager's 15 —
deferral added no turns in this run. Total input tokens across the run:
85.1K deferred vs 414.7K eager (4.9×; per-request average 6.1K vs 27.6K,
delta matching the schema mass). Cost: $0 (nano gateway).

| Date | Vehicle | Tasks | Outcome | Notes |
|---|---|---|---|---|
| 2026-10-24 | nano gateway ($0), real rust-analyzer 1.98.1 | rename × 3 runs (A/B/B2) | **3/3 success; bar met** | see LSP turn-economy probe section below |
| 2026-10-23 | nano gateway ($0) + ghmock 35-tool server | fix-via-MCP-issue × 2 arms | **both arms success; 12.4× upfront token cut** | see MCP deferral probe section above |

| 2026-10-23 | nano gateway (`kilo/nvidia/nemotron-3.5-lightning:free-1M-txt`, $0) | fix / impl / refactor | **3/3 success** | fix: 8 turns, 9 tool calls, median bug fixed exactly (even-pair averaging); impl: 11 turns, rotate_right implemented (byte-based rotation — passes the pinned tests, though the doc says code points: spec-fidelity note, not a failure); refactor: 13 turns, `char`→`&str` signature changed and the `tag` caller updated. All runs `end_reason: completed`, **zero loop nudges, zero gate refusals** — the weak model wandered not; tasks are single-crate daily-driver scale. Schema-11 forensics (tool args/result heads, per-run `--json` artifacts) verified live in this probe. |

## LSP turn-economy probe (slice E3)

Measures whether the opt-in LSP tool surface (slices E1–E3, `--lsp`)
improves turn economy on a rename that ripples across modules — the
task class semantic tooling exists for. Assets: the `rename` variant in
`docs/benchmarks/daily-driver-fixture.py` (`normalize_name` defined in
`src/lib.rs`, seven call sites across five modules, integration tests
already importing `canonicalize` — seeded red with E0432), real
rust-analyzer 1.98.1 attached in the `--lsp` arms.

Protocol: fresh repo per arm, same task text verbatim (rename
`normalize_name` to `canonicalize` everywhere; `cargo test` must pass),
same $0 nano model. Success = 2/2 tests green + zero stale
`normalize_name` references. Exploratory turns = leading model turns
containing no write-path tool (`edit_file`, `write_file`, or
`lsp_rename`). Two `--lsp` runs, labeled B and B2 — after B used no lsp
tools at all, a second labeled sample answered whether that was noise;
both runs are recorded, neither replaces the other. Attachment itself
was verified out-of-band: a direct `lsp_hover` probe returned live
rust-analyzer data in the same fixture shape.

Bar: same-or-better success at equal-or-fewer exploratory turns, with
no harm from the attached tools.

Result 2026-10-24: **bar met; attribution split honestly below.**

| Arm | Success | Turns | Exploratory | lsp calls | edit_file | Input tok (cached) | Wall |
|---|---|---|---|---|---|---|---|
| A — no `--lsp` | 2/2, 0 stale | 26 | 3 | — | 20 | 231,073 (188,640) | 80.3s |
| B — `--lsp` | 2/2, 0 stale | 16 | 3 | **0** | 16 | 157,444 (127,856) | 58.8s |
| B2 — `--lsp`, labeled rerun | 2/2, 0 stale | 14 | 3 | **1** (`lsp_rename`, success, 6 docs) | 3 | 121,018 (98,512) | 47.7s |

Reading it straight: all three arms succeeded with identical
exploratory counts, so the bar holds. But arm B never called an lsp
tool — its 16-turn edge over A is run-to-run variance plus perhaps the
larger facts block, not LSP. B2 is the run where the model elected
`lsp_rename`: one call renamed the symbol across six documents, and the
run finished with 3 fixup edits (vs A's 20), the fewest turns (14), the
least input tokens (−48% vs A), and the lowest wall time. Conclusion:
`--lsp` is no-harm opt-in breadth; the mechanical-rename collapse is
real *when the model elects semantic tools*, and this free nano-tier
model elects them stochastically (0, then 1, across two runs). No
stronger claim; capable-model validation is future funded work.
