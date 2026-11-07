# Repro-test tie-breaker — design + evidence gate (roadmap item 39)

Status: evidence gate run, feature deferred. Feature 8 of the
8-feature plan.

## What the feature would be

When the self-optimization loop ranks candidate overlays and two
adopted candidates tie on the selection key, break the tie by
*reproducibility*: re-run the probe suite for the tied candidates and
prefer the one whose improvement reproduces — flaky gains lose to
repeated gains.

Selection key today (`--tier lite`,
`crates/loom-eval/src/bin/selfopt.rs`):
`max_by(input_improvement_pct desc, then probe_failures asc)`. On an
exact tie Rust's `max_by` returns the **last** candidate —
deterministic (the decisions Vec follows proposal order), but
silent: nothing records that a tie was broken or how.

## Pre-registered evidence gate

Measured before building, decision rule fixed first: **build only if
ties between adopted candidates are observable at non-negligible
frequency in existing artifacts** — a tie-breaker that re-runs
suites to settle ties that never occur is machinery without a
population. Negligible or unmeasurable → defer and record the
trigger.

## Measurement (every readable surface)

| Surface | Could show ties? | Result |
| --- | --- | --- |
| selfopt ledger (`append_ledger` writes beside the sweep) | yes — `Decision` rows carry `input_improvement_pct`, `probe_failures`, `adopted` | **no ledger exists on disk anywhere** (`target/eval`, data dir, repo) and none in git history |
| selfopt reports (`selfopt-report-{ts}.md`) | yes — same decisions rendered | none ever written |
| suite-audit v3 artifacts (`mutants/*/predictions.jsonl`, `suite_audit.json`) | no — patches and structural counts only; scoring is delegated to the official external harness |
| swebench sweeps (`swebench-sweep-*.json`, scorer outputs) | no — outcomes are binary resolved/unresolved per instance, one candidate per run |
| in-tree tie sites | — | `order_instances` breaks ties by id explicitly (selfopt.rs:716); `pareto_frontier` keeps exact ties (correct for a frontier); `--tier lite` `max_by` last-wins silently but deterministically — zero recorded invocations exist to tie |

**Ties observed: 0. Tie population: unmeasurable — the artifact
type that would contain ties has never been produced.**

## Verdict: defer (kill criterion)

1. **No population.** Not one selfopt ledger or report exists on any
   readable surface, in the working tree, the data directory, or git
   history; the suite-audit and swebench artifacts carry no
   comparable scores at all. There is nothing for a tie-breaker to
   settle.
2. **The remedy is spend-gated anyway.** A repro re-run under
   selfopt means re-running agent probes — paid model calls. The
   offline counterfactual replay (roadmap item 32) replays recorded
   contexts; it cannot mint new probe outcomes, so repro re-runs
   have no offline path. Even with a tie population, the feature
   would need an explicit spend decision first.
3. **Flake detection at the right layer already exists.** Kernel
   autofix is gated by a re-run (`AutofixEffect`, kept only when
   diagnostics improve — tool.rs), and `probe_failures` already
   counts runs that failed to reproduce their claim inside a probe.
   What is missing is only tie *arbitration between candidates*, and
   no tie has ever reached arbitration.

## Revisit trigger (sharp)

The first selfopt ledger that records a selection where **two
adopted candidates tie on `(input_improvement_pct,
probe_failures)`**. The ledger's `Decision` rows already carry both
fields, so the trigger is checkable with one script against the next
real run — no code needed to detect it. When it fires, the build is
small and its seams are recorded here:

- selection site: the `--tier lite` `max_by` (bin/selfopt.rs),
  currently silent last-wins;
- remedy order: explicit deterministic key first (fewer
  `probe_failures`, then label order — no spend), repro re-run
  second (spend decision required, opt-in);
- the probe already re-runs identical instances per candidate —
  `probe_failures` is the natural reproducibility signal;
- ledger format (`LedgerEntry`) needs no schema change to record the
  tie and the chosen breaker.

No code changed.
