# Self-optimization outer loop (`selfopt`)

Status: shipped seam, 2026-10-25 (roadmap item 20.3). The loop is
offline: it never edits harness behavior at task time, and it never
changes what a running agent sees without a paired-run adoption gate.

## What it is

`selfopt` (crate `loom-eval`, binary `selfopt`) closes the last
ungated piece of item 20: an outer loop that reads raw traces and
scores, and proposes changes to the harness — in loom's case, to the
`LOOM_*` environment seams (tool budgets + prune shape) that the
engine already applies via `budgets_with_env` / `prune_with_env`.

The unit of change is an **overlay**: a `BTreeMap<String, String>` of
`LOOM_*` variable → value. Overlays ride into the frozen SWE-bench
container through `ContainerSpec::extra_env` (pure `-e K=V` argv
additions); `CONTAINER_SCRIPT` stays byte-identical, and reserved
variables (provider/model/timeout/…) are rejected so a proposal can
never smuggle a model change past the model-fixed invariant.

## Loop shape: Propose → Probe → Compose

Taken directly from Harness² (arXiv 2610.14224), which we adopt for
its search shape but invert in where the editing happens — offline in
the eval harness instead of online at task time. The online half
breaks the deterministic-controller principle and the never-worse
contract; recursion stays here, where the maintenance bar rules.

1. **Propose.** Two origins, mirroring the invariant/domain split:
   - *Rule proposer* (deterministic): for each of the 13 registered
     knobs, halve and double the current value, clamped to the knob's
     floor/ceiling, skipping no-ops and duplicates. Single-knob moves
     only, so attribution is trivial.
   - *Model proposer* (one-shot, any provider incl. free tiers):
     receives the knob table with defaults/ranges/docs, per-instance
     medians (input tokens, events, turns, failures), and the current
     ledger lessons; returns JSON proposals, each tagged
     `invariant` (task-agnostic mechanism) or `domain` (task-class
     specific) with a hypothesis and rationale. Invalid or
     out-of-range overlays are dropped, never clamped silently.
2. **Probe.** Each proposal runs N docker re-runs (default 5) per
   instance over the same prompts, same images, same model — matched
   environment, only the overlay differs. Rate-limit retries reuse the
   swebench-run convention (×3, 120 s cooldown).
3. **Decide.** Adoption gate, all three required:
   - median per-instance input tokens improve ≥ 5%;
   - median event count does not regress > 10%;
   - probe failures ≤ baseline failures.
   The gate is arithmetic, not judgment — the model proposes, numbers
   dispose.
4. **Compose.** Independently-winning overlays are merged (same-key
   different-value = conflict, named and refused), and the merge is
   **re-probed** and re-gated. Per-lever winners are not a shippable
   config; edits interact (prune trigger × read window × tool menu).
5. **Ledger.** Every cycle appends to `selfopt-ledger.json` (atomic
   tmp+rename). Lessons enter only when tied to the contrast that
   produced them — the Harness² evidence rule — each carrying
   statement + evidence. The next cycle's model proposer reads the
   last lessons, so knowledge compounds without ever being injected
   into a live agent by default.

## Why traces, not summaries

GEPA (arXiv 2507.19457) calls this *actionable side information*:
reflective evolution works when the proposer sees full traces — error
messages, usage, failure reasons — rather than aggregate scores, and
it reaches optima in ~10% of the rollouts GRPO needs. We feed the
proposer per-instance medians plus failure counts now, and the run
readers (`run_outcome`, `run_turns`, `run_usage`) already expose
everything needed to widen this to raw event streams without protocol
change.

## Knobs

The registry is compile-time (`selfopt::KNOBS`, 13 entries) and a unit
test cross-checks it against the engine's `BUDGET_ENV_VARS` /
`PRUNE_ENV_VARS` — a seam that grows in the engine must be registered
here or the test fails. Every knob carries floor/ceiling/unit/doc so
the model proposer sees the same contract the validator enforces.

## What is deliberately absent

- No harness-source self-editing (Harness²'s executable-edit half).
  Loom's equivalent surface is the overlay, which is total (any
  registered knob), typed (validated ranges), and reversible (it is
  data, not code).
- No online lesson injection. Ledger lessons inform proposals only;
  injecting into the facts seam at task time stays a separate,
  evidence-gated item (20.4).
- No correctness signal in the gate. Resolve rates come from the
  official scorer, offline and optional; the gate uses tokens, events,
  and failures because those are available per-probe without a scorer
  round-trip, and a config that fails more runs is never adopted even
  if cheaper.

## LLM-perspective evaluation

Written from the consuming model's side — the position of the agent
whose harness is being tuned — because the loop's real customer is
that model.

What makes proposals *useful to receive* as the proposer:
- The knob table arrives as data with ranges and one-line docs, not
  prose to mine. I can reason about trade-offs (a 4 KiB command head
  vs a 64 KiB one) without reverse-engineering source.
- Per-instance medians, not global means. One pathological instance
  dragging a mean hides a mechanism that helps 9 others; medians plus
  failure counts let me form class-level hypotheses ("read-heavy
  instances thrash the prune floor") instead of noise fitting.
- Lessons with evidence. Prior hypotheses and their outcomes are the
  cheapest way to stop me re-proposing a loser — accumulated ancestry,
  GEPA's mutation-inherits-lessons effect.
- A strict JSON contract with validation. Silent clamping would make
  my next proposal reason about a value that does not exist; drops
  with a count keep the feedback loop honest.

What makes the tuned harness *better to run in* as the agent:
- Budgets are the difference between a context I can trust and one I
  must defensively re-read. The knobs most likely to pay (read head,
  command tail, prune trigger) control exactly the failure mode that
  hurts most mid-task: discovering on turn N that the file view I
  built my plan on was silently truncated.
- Idle MCP eviction and prune-tail are about *keeping the working set
  warm* — recent tool output and the current plan — while shedding the
  cold prefix. That matches how attention is actually spent.
- The failure non-regression clause protects the one thing I cannot
  trade away: a cheaper harness that fails runs is worse for me than
  an expensive one that completes them, because partial credit for a
  truncated trace is near zero.

## Operation

```
cargo run -p loom-eval --bin selfopt -- \
  --eval-dir target/eval --provider deepseek --model deepseek-flash \
  --runs 5 --jobs 2 --max-proposals 8
```

Reads the latest sweep report (or `--sweep`), reuses its instances'
prompts/images, writes probe artifacts under
`instances/<slug>/selfopt/<label>/run-<n>/`, appends
`selfopt-ledger.json`, and emits a markdown report
(`selfopt-report-<ts>.md`) with every verdict and the winning overlay
as copy-paste `export LOOM_*=…` lines. `--dry-run` prints the
proposal plan (rules + model JSON) without spending a container;
`--rules-only` skips the model proposer entirely.
