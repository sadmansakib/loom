# Best-of-N sweep-level selection: an offline measurement (item 24)

Date: 2026-11-08 · Vehicle: `aws-bedrock/openai.gpt-6-luna` via the funded
gateway · Cost: $0 (all data from the four stored N=5 A/B sweeps and their
official scorer reports).

## What was measured

The sweep runner's cross-run selection (`swebench-run --runs N`):
when an instance runs N times, the most frequent **normalized diff**
becomes the sweep's prediction (`select_patch`; ties keep the earlier
run). This is Trae/Agentless-style majority voting — distinct from the
runtime `--best-of N` gate selection in `engine::worktrees`, which was
not exercised here.

Data: 4 datasets × 5 instances × 5 runs + selected
(`n5-full`, `n5-bare`, `n5-effort-progressive`, `n5-effort-fixed`),
each with an official SWE-bench Verified scorer report per file.

## Results

| dataset | per-run resolved | median | mean | selected | verdict |
|---|---|---|---|---|---|
| n5-full | 3,3,3,3,3 | 3 | 3.0 | 3 | neutral |
| n5-bare | 3,3,4,3,3 | 3 | 3.2 | **3** | **below mean** |
| n5-effort-progressive | 4,3,4,4,3 | 4 | 3.6 | 4 | at median |
| n5-effort-fixed | 3,4,3,4,3 | 3 | 3.4 | **3** | **below mean** |

Per-instance, 17 of 20 (dataset, instance) pairs produced **five
distinct patches across five runs** — `majority_n = 1` everywhere
except scikit-learn (the one near-deterministic ~0.9 KB edit, where
2–5 runs converge and selection is harmless). With `majority_n = 1`
the strict-greater tie-break keeps the earlier run, so **selection is
byte-identical to run 1 in every row of this corpus** — the selector
carries no information.

The cost of that emptiness is visible: sympy__sympy-13974 resolved in
2/5 (bare) and 2/5 (effort-fixed) runs — but not in run 1 — so the
"selected" prediction lost a solvable instance both times
(SELECTOR-LOSS). That is precisely the weak-selector failure mode the
roadmap flags from arXiv 2608.03961 (Best-of-3 *worse* than Best-of-1);
here it fires because the selector is degenerate, not weak.

## Conclusions

1. **Agent diffs on this vehicle are fingerprints, not votes.**
   Independent runs of the same instance almost never produce the same
   normalized patch, so majority voting at sweep level has no signal
   population. Its only safe case (near-deterministic edits like
   scikit-learn) is also its useless case — those runs all resolve.
2. **At sweep level, N=5 bought zero solves over N=1** on this corpus:
   selected 13 total vs per-run mean 13.2 across the four datasets.
   The marginal instances (sympy, sphinx) flip run-to-run, and
   fingerprint voting cannot tell a resolving patch from a failing one.
3. **The selector that can work is the gate** (item 24's design rule:
   the deterministic outcome gate judges, not patch equality). Runtime
   `--best-of N` already selects that way; the sweep-level rule would
   need per-run test execution in the testbed (free of model spend)
   before it could beat a coin flip.

Recommendation: treat sweep `selected` files as *one more sample*, not
a selector — contrast tables should report per-run medians (as the N=5
contrast docs already do). If sweep-level selection is ever needed as a
real selector, replace fingerprint voting with gate-first selection;
documented in roadmap item 24.
