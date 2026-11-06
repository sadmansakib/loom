# Offline counterfactual context replay (`loom replay-context`)

Status: design + implementation, 2026-11-06 (roadmap item 20 seam, funded-
measurement mitigation). The replay is **offline and free**: it makes zero
model calls, zero container runs, and touches no network.

## Problem

The roadmap's biggest open risk is funded measurement (item 1): N ≥ 5
re-runs cost credit the balance cannot cover, so most context levers ship
on N=1 evidence. But the context projection is a **pure function** of the
session (`DefaultContextCompiler` + `PruningContextCompiler`, no I/O, no
clocks — the property the replay-equivalence tests already lean on), and
sessions/traces are stored verbatim. A stored session can therefore be
re-compiled under different `PruneConfig` values and the *token and cache
cost* difference measured exactly, with no model in the loop.

What it cannot measure: task success. The transcript is fixed — the
model's replies do not change under a counterfactual pruning policy, so a
stub that would have changed behavior is invisible. Every report prints
this limitation; the tool is a cheap pre-filter for paid probes, never a
replacement for them.

## Evidence

- TurnCost (arXiv 2610.15093, mapped in the lit review): 98.6% of input
  tokens hit the prefix cache — cache-prefix stability, not raw bytes, is
  the cost unit, which is why the replay reports a prefix-stability
  metric next to token counts.
- SWE-Replay (arXiv 2601.22129, roadmap item 28): replaying a recorded
  exploratory prefix under varied continuations is a valid measurement
  vehicle; here the continuation is the fixed transcript itself.
- selfopt (docs/selfopt-design.md): every docker probe costs containers +
  model credit; the adoption gate (median input tokens ≥ 5% better,
  `ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`) is arithmetic. A pre-probe that
  predicts the token delta for free rejects doomed proposals before
  spend.
- In-repo invariants the replay reuses: byte-stable projection between
  events; checkpoint freeze/re-apply (`PruneCheckpoint` is serde and
  round-trips); `ContextCensus` per-source attribution; the trace format
  is a data contract (`engine::trace`).

## Options considered

1. **Re-derive per-turn requests from the event stream only.** Rejected
   as primary: events carry no census and not the full transcript.
2. **Record per-turn census into events (schema v14) and replay from
   events.** Rejected for v1: an additive schema bump buys per-turn
   ground truth, but the transcript alone already reproduces every
   compile deterministically; recording more is measurement of the same
   purity. Revisit if validity checks (below) prove insufficient.
3. **Chosen: replay the stored transcript through the real compilers.**
   Segment the transcript at assistant-item boundaries, feed growing
   prefixes through the *same* `PruningContextCompiler` the live run
   used, carry the checkpoint exactly like the loop does, with the
   estimator chosen from the recorded provider.

## Chosen design

### Inputs

- `loom replay-context --trace DIR` — a task directory written by
  `loom run --trace` (`transcript.json` + `manifest.json`; events
  optional).
- `loom replay-context SESSION` — a stored session id, loaded from the
  store the same way `loom sessions` resolves it (`LOOM_DB` → data-dir
  ladder). No provider credential is needed: nothing is sent anywhere.

### Reconstruction

The live loop compiles once per model turn, before appending that turn's
assistant item. The replay walks `transcript.json` in order and compiles
at every prefix that ends right before an assistant item — user items
(nudges, steering, refusals) land in the prefix exactly as the loop
appended them. A task with zero model turns (fast-path solve) contributes
no compile. Turn numbers are 1-based compile ordinals, matching
`TurnStarted` numbering.

What is reconstructed vs assumed:

| Input | Source | Fidelity |
|---|---|---|
| transcript | stored verbatim | exact |
| estimator | `manifest.harness.provider` → `estimator_for` | exact family |
| `PruneConfig` | provider base + `--set` overlay | exact when overlay reproduces the run's `LOOM_*` env |
| budget | `--window` (default 1,000,000) less `--reserved-output` (default 32,768) | **assumed** — traces do not record capabilities |
| system prompt | CLI default or `--system-prompt` | **assumed** — not recorded |
| facts block | empty (identical in both arms) | **assumed** — not recorded; deltas unaffected |
| tool registry | the standard `loom run` default set | **assumed** — opt-in flags are not recorded |

Because facts/system/tool-specs are reconstruction constants shared by
both arms, **paired deltas are exact attributes of the overlay**;
absolute totals carry the assumption table above, and the validity check
separates transcript-derived census sources (must match) from the
reconstructed ones (compared informationally).

### Overlays

`--set KEY=VAL` (repeatable). Keys are the five selfopt `LOOM_PRUNE_*`
seams (`prune_trigger_percent`, `prune_floor_tokens`,
`prune_min_reclaim_tokens`, `prune_tail_tokens`, `phase_mask_turns`) plus
replay-only compiler knobs selfopt does not env-sweep:
`age_mask_turns`, `elide_turns`, `elide_reasoning`,
`elide_assistant_text` (all `PruneConfig` fields; `off` disables an
optional one). Unknown keys fail loudly — a silently ignored overlay
would make the whole report a lie.

The command replays **both** arms: (a) baseline — the provider-base
config with no overlay; (b) overlay — baseline + `--set` pairs. Without
`--set`, only the baseline arm runs and the report is a per-turn census
trace.

### Per-turn report

For each turn: estimated input tokens (`census.total`), the per-source
split, whether this compile froze a checkpoint (trigger arm + reclaimed
tokens), and **cache-prefix stability**: the length of the common byte
prefix between this turn's serialized `messages` and the previous turn's,
as a ratio of the previous request's length (a drop is a cache-bust
proxy; a checkpoint freeze legitimately drops it once).

Paired output renders like `swebench-contrast`: per-turn Δtokens, total
Δtokens, Δreclaimed, count of freezes per arm, min prefix-stability per
arm, and the projected median input-token improvement percentage across
turns — the same statistic the selfopt gate computes, clearly labeled
*projected estimate*, not billed usage.

### Validity check (required)

When the input carries ground truth (trace manifest's
`summary.last_context_census`, `summary.turns`; the events file's
`ContextPruned`/`ContextHandoff` events), the replay self-validates and
prints a verdict:

- turns: replay compile count must equal `summary.turns` (fast-path tasks
  excepted by construction);
- census: transcript-derived sources (`user_items`,
  `assistant_history`, `reasoning_replay`, `tool_results`) must match
  **exactly** under the original config; `total` within a stated
  tolerance covering the reconstructed prefix (facts/system/tools);
- prune events: freeze turns, trigger arms, and reclaimed-token totals
  must match exactly (same compiler, same estimator, same config);
- **schema skew**: a trace recorded under an older
  `event_schema_version` predates the current compiler generation, so
  its census and prune events are *not comparable* to this binary's
  projection. The replay detects the skew, prints `NOT ASSERTABLE`
  with both schema numbers, and still compares turns (the transcript
  shape is stable). Measured: the checked-in `traces/smoke` trace
  (schema 3 vs binary 13) replays with an 8-token `tool_results` delta
  under the current compiler — exactly the generation drift this rule
  refuses to call a failure. Fresh same-schema traces validate
  normally (engine test `replay_reproduces_recorded_census_and_prunes`
  exercises the full live-run → validity-check path).

Any mismatch prints `VALIDITY: FAILED` with per-field deltas — the tool
declares itself untrustworthy for that input rather than guessing why.

### Non-replayable: LLM handoff

When a counterfactual overlay drives a compile past the rescue arm, the
live loop would spend one model call on a handoff summary. The replay
cannot synthesize that text: it marks the turn
`handoff-required`, stops that arm there, and says so. An input whose
*original* run handed off replays only up to the handoff turn (the
summary text is not recoverable from the transcript view).

### selfopt integration (opt-in)

`selfopt --pre-probe TRACE_DIR…` (repeatable flag, default off): before
docker-probing a proposal, replay every provided trace baseline-vs-
overlay, compute the projected median input-token improvement, and
reject the proposal without spend when it cannot clear
`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT` — the same constant the docker
gate uses. Proposals that pass still face the full docker gate; the
pre-probe only removes ones whose token mechanics are provably
unchanged. No traces given → no pre-probe (the frozen container script
runs `--no-save`, so sweep artifacts contain no transcripts by design;
the operator supplies locally recorded `--trace` dirs).

Pre-probe specifics, as shipped:

- Budget-only overlays (e.g. `LOOM_READ_HEAD_BYTES`) are kept without
  pricing: the recorded transcript is fixed, so replay cannot project
  them; the docker probe decides.
- The replay window is an assumption (traces record provider and model
  but not capabilities), so `--pre-probe-window` (default 200000) and
  `--pre-probe-reserved` (default 32768) let the operator match the
  sweep model's real window. A wrong window moves the trigger point
  and can misprice freeze-happy overlays in either direction — set it
  to the model's actual context size before trusting a reject.
- A trace whose baseline arm overruns past rescue is skipped and named;
  a proposal with no usable traces is kept — the pre-probe tightens on
  evidence only, never overrides the loop on none.

## Failure modes

- **Overlay ≠ what the run saw.** Traces do not record `LOOM_*` env. The
  baseline arm is the provider default; if the original run was itself
  overlaid, absolute numbers drift (deltas between arms stay valid).
  Stated in every report.
- **Multi-task sessions.** Only the trace's own task (or the session's
  last task record) is replayed; earlier tasks' items still project as
  history. Exact for single-task traces (the norm).
- **Handoff stop** (above) — an arm can end early; deltas then cover the
  common prefix only and say so.
- **Estimator mismatch** for custom gateways (o200k assumed for any
  non-Anthropic provider): absolute counts shift, deltas less so.

## Measurement plan

- Offline validity test (required): scripted session → real compilers →
  trace export → replay under original config → census/prune equality
  asserted; a corrupted transcript must fail validity.
- Offline paired test: same session replayed baseline vs overlay where
  the overlay provably stubs more (tiny floor/trigger) → Δtokens and
  freeze counts asserted.
- Prefix-stability test: a checkpoint freeze drops the ratio exactly
  once; steady turns hold it at 1.0.
- On first real use: replay one stored long session and record the
  projected-vs-billed input gap (census estimate vs manifest billed
  `input_tokens`) as the calibration number for future reports.

## Kill / revisit criteria

- If the validity check cannot pass on real traces for any reason other
  than documented reconstruction assumptions, the tool is untrustworthy
  and gets pulled, not patched into plausibility.
- Revisit recording per-turn census into the event schema only if the
  assumption table's drift makes absolute numbers matter for a decision
  (deltas are unaffected).
- The selfopt pre-probe is dead if its rejections ever disagree with
  docker outcomes on proposals that pass the gate — it filters only
  provably-flat token mechanics, so a disagreement means a bug.
